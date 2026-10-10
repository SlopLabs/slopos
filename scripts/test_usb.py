#!/usr/bin/env python3
"""The host half of `just test-usb`: boots the tests ISO with both of QEMU's
xHCI models, each carrying a SuperSpeed stick, QEMU's full-speed hub with a
stick, a tablet and a mouse behind it, a high-speed stick and a full-speed
keyboard, with a high-speed ext4 stick on qemu-xhci's fifth USB 2 connector
and a read-only ext4 drive for nec-usb-xhci's, and a usb-net behind
qemu-xhci's hub on a SLIRP network of its own, and acts on the guest's
`USB-TEST:` lines: it plugs and pulls devices through QMP and injects keys
and motion with `input-send-event`. The run passes when the suite is green,
the log shows every device enumerated, bound where a driver matches it, and
removed, each time, the NIC published as eth1 and retired with it, and both
ext4 volumes pass `e2fsck` at rest, the stick holding what the guest fsynced
before its pull.

QEMU sends an event that names no display to the unbound device activated
most recently, and one that names `video0` to a device bound to it. The
keyboards and tablets are bound, so a key or a button with no display reaches
the i8042, and motion with none the newest usb-mouse, or the PS/2 mouse once
both are pulled."""

import argparse
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from typing import NamedTuple

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))
import ext4_profile  # noqa: E402

CONTROLLERS = {
    "xhci1": ("qemu-xhci", "1b36:000d", ""),
    "xhci2": ("nec-usb-xhci", "1033:0194", ",msix=off"),
}
USB3_PORTS = 2
USB2_PORTS = 5
STICK_BYTES = 1 << 20
DISK_BYTES = 16 << 20
# What the guest writes and fsyncs on the ext4 stick before its pull, and
# what the read-only drive is seeded with (userland/src/bin/tests/usb_disk_test.rs).
KEPT = ("kept", b"fsynced before the pull\n")
SEEDED = ("seeded", b"written by the host\n")
COLD_BYTES = 1 << 20
DISPLAY = "video0"
# An event a guest poll has not taken yet merges with the next.
PACE = 0.15


class Device(NamedTuple):
    suffix: str
    driver: str
    connector: str
    props: dict
    path: str
    bound_by: str | None


# QEMU numbers the USB 3 ports first: a SuperSpeed device on connector N
# lands on root port N, any other device on connector N on root port p3 + N.
# A device behind the hub on connector 2 is at port "2.M", path "4.M".
DEVICES = (
    Device("ss", "usb-storage", "1", {}, "1", "usb-test"),
    Device("hub", "usb-hub", "2", {}, "4", None),
    Device("hub-stick", "usb-storage", "2.1", {}, "4.1", "usb-test"),
    Device("hub-tablet", "usb-tablet", "2.2", {"display": DISPLAY}, "4.2", "usb-hid"),
    Device("hub-mouse", "usb-mouse", "2.3", {}, "4.3", "usb-hid"),
    Device("hs", "usb-storage", "3", {}, "5", "usb-test"),
    Device("fs", "usb-kbd", "4", {"usb_version": 1, "display": DISPLAY}, "6", "usb-hid"),
)
# Each controller's own: on the fifth connector qemu-xhci's ext4 stick,
# `sda`, and nec-usb-xhci's read-only drive, which only the guest's `plug ro`
# attaches; and behind qemu-xhci's hub a usb-net, which QEMU offers in an
# RNDIS and an ECM configuration, on 10.0.3.0/24 with the echo peer at
# 10.0.3.100 (drivers/src/tests/usb_tests.rs, userland/src/bin/tests/usb_net_test.rs).
# QEMU 8.2 (CI's) reports the MAC with its first byte replaced by 0x40,
# QEMU 11 as given, so one that starts with 0x40 reads the same on both.
NET_MAC = "40:54:00:12:34:99"
EXTRA = {
    "xhci1": (
        Device("disk", "usb-storage", "5", {}, "7", "usb-storage"),
        Device("net", "usb-net", "2.4", {"netdev": "usbnet0", "mac": NET_MAC}, "4.4", "usb-net"),
    ),
    "xhci2": (Device("ro", "usb-storage", "5", {}, "7", "usb-storage"),),
}
NET_PEER = "10.0.3.100:9999"
LATE = {"xhci2-ro"}
ROUNDS = 2
TYPED = "exit 7\n"
QCODES = {" ": "spc", "\n": "ret"}


def devices():
    for bus in CONTROLLERS:
        for suffix, driver, port, props, path, bound in DEVICES + EXTRA[bus]:
            yield f"{bus}-{suffix}", bus, driver, port, props, path, bound


def image_of(build_dir, name):
    return os.path.join(build_dir, f"usb-{name}.img")


def ext4_image(image, label, files):
    """A volume in the profile SlopOS formats, holding `files`."""
    stage = image + ".d"
    shutil.rmtree(stage, ignore_errors=True)
    os.makedirs(stage)
    for name, data in files:
        with open(os.path.join(stage, name), "wb") as f:
            f.write(data)
    with open(image, "wb") as f:
        f.truncate(DISK_BYTES)
    subprocess.run(
        ["mke2fs", "-q", "-F", *ext4_profile.mkfs_args(), "-L", label, "-d", stage, image],
        check=True,
    )
    shutil.rmtree(stage)


def device_args(build_dir):
    """Each stick is a `-blockdev` node: QEMU deletes a `-drive` backend with
    the device that used it."""
    echo = os.environ.get("ECHO_PEER_CMD", "/bin/cat")
    args = ["-parallel", "none", "-trace", "ps2_set_ledstate"]
    args += ["-netdev", f"user,id=usbnet0,net=10.0.3.0/24,guestfwd=tcp:{NET_PEER}-cmd:{echo}"]
    for bus, (model, _, extra) in CONTROLLERS.items():
        args += ["-device", f"{model},id={bus},p2={USB2_PORTS},p3={USB3_PORTS}{extra}"]
    for name, bus, driver, port, props, _, _ in devices():
        device = f"{driver},id={name},bus={bus}.0,port={port}"
        device += "".join(f",{key}={value}" for key, value in props.items())
        if driver == "usb-storage":
            image = image_of(build_dir, name)
            node = f"driver=raw,node-name={name},file.driver=file,file.filename={image}"
            if name.endswith("-disk"):
                ext4_image(image, "usb-stick", [("cold", os.urandom(COLD_BYTES))])
            elif name.endswith("-ro"):
                ext4_image(image, "usb-ro", [SEEDED])
                node += ",read-only=on"
            else:
                with open(image, "wb") as f:
                    f.truncate(STICK_BYTES)
            args += ["-blockdev", node]
            device += f",drive={name},removable=on"
        if name not in LATE:
            args += ["-device", device]
    return args


class Qmp:
    def __init__(self, path, deadline, qemu):
        while True:
            try:
                self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                self.sock.connect(path)
                break
            except OSError:
                self.sock.close()
                if qemu.poll() is not None:
                    raise RuntimeError(f"QEMU exited {qemu.returncode} before opening QMP")
                if time.monotonic() > deadline:
                    raise RuntimeError(f"no QMP socket at {path}")
                time.sleep(0.2)
        self.file = self.sock.makefile("rw")
        self.events = []
        self._read()
        self.execute("qmp_capabilities")

    def _read(self):
        line = self.file.readline()
        if not line:
            raise RuntimeError("QMP closed")
        return json.loads(line)

    def execute(self, command, **arguments):
        message = {"execute": command}
        if arguments:
            message["arguments"] = arguments
        self.file.write(json.dumps(message) + "\n")
        self.file.flush()
        while True:
            reply = self._read()
            if "event" in reply:
                self.events.append(reply)
                continue
            if "error" in reply:
                raise RuntimeError(f"{command}: {reply['error']}")
            return reply.get("return")

    def wait_deleted(self, ids, deadline):
        pending = set(ids)
        while pending:
            pending -= {
                e.get("data", {}).get("device")
                for e in self.events
                if e.get("event") == "DEVICE_DELETED"
            }
            if not pending:
                break
            if time.monotonic() > deadline:
                raise RuntimeError(f"never deleted: {sorted(pending)}")
            self.sock.settimeout(max(0.1, deadline - time.monotonic()))
            try:
                self.events.append(self._read())
            except OSError:
                raise RuntimeError(f"never deleted: {sorted(pending)}")
            finally:
                self.sock.settimeout(None)

    def send(self, *events, display=False):
        """One `input-send-event` per call, paced so the guest takes each."""
        arguments = {"events": list(events)}
        if display:
            arguments["device"] = DISPLAY
        self.execute("input-send-event", **arguments)
        time.sleep(PACE)


def key(qcode, down):
    return {"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": qcode}}}


def button(down):
    return {"type": "btn", "data": {"down": down, "button": "left"}}


def axis(kind, name, value):
    return {"type": kind, "data": {"axis": name, "value": value}}


class Bench:
    """What is plugged, and how often each device came and went."""

    def __init__(self, qmp):
        self.qmp = qmp
        self.present = {name for name, *_ in devices() if name not in LATE}
        self.adds = {name: int(name in self.present) for name, *_ in devices()}
        self.deletes = {name: 0 for name, *_ in devices()}
        self.numbers = {}

    def pull(self, names, timeout):
        """A hub's devices go first: deleting a hub leaves the devices on its
        ports behind as orphans whose ids a later plug would collide with."""
        ports = {name: port for name, _, _, port, *_ in devices()}
        names = [name for name in names if name in self.present]
        for group in ([n for n in names if "." in ports[n]], [n for n in names if "." not in ports[n]]):
            for name in group:
                self.qmp.execute("device_del", id=name)
            self.qmp.wait_deleted(group, time.monotonic() + timeout)
            for name in group:
                self.present.discard(name)
                self.deletes[name] += 1
        self.qmp.events.clear()

    def plug(self, only=None):
        """Every device but the late ones, or the one `only` names."""
        for name, bus, driver, port, props, _, _ in sorted(devices(), key=lambda d: "." in d[3]):
            if name in self.present or (name in LATE if only is None else name != only):
                continue
            arguments = {"driver": driver, "id": name, "bus": f"{bus}.0", "port": port}
            if driver == "usb-storage":
                arguments.update(drive=name, removable=True)
            arguments.update(props)
            self.qmp.execute("device_add", **arguments)
            self.present.add(name)
            self.adds[name] += 1

    def device_at(self, where):
        """`<controller number>-<path>` as the guest names a device."""
        number, path = where.split("-", 1)
        bus = next((b for b, (_, ids, _) in CONTROLLERS.items() if self.numbers.get(ids) == number), None)
        name = next((n for n, b, _, _, _, p, _ in devices() if b == bus and p == path), None)
        if name is None:
            raise RuntimeError(f"no device at {where}")
        return name

    def act(self, words, timeout):
        qmp = self.qmp
        match words:
            case ["pull"]:
                self.pull(sorted(self.present), timeout)
            case ["plug"]:
                self.plug()
            case ["pull", "disk" | "ro" as which]:
                self.pull([n for n, *_ in devices() if n.endswith(f"-{which}")], timeout)
            case ["plug", "disk" | "ro" as which]:
                self.plug(next(n for n, *_ in devices() if n.endswith(f"-{which}")))
            case ["pull", "mice"]:
                self.pull([n for n, *_ in devices() if n.endswith("-mouse")], timeout)
            case ["pull", "keyboard", where]:
                self.pull([self.device_at(where)], timeout)
            case ["hold", "x"]:
                qmp.send(key("x", True), display=True)
            case ["release", "x"]:
                qmp.send(key("x", False), display=True)
            case ["ps2", "repeat", "x"]:
                for down in (True, True, False):
                    qmp.send(key("x", down))
            case ["caps"]:
                qmp.send(key("caps_lock", True), display=True)
                qmp.send(key("caps_lock", False), display=True)
            case ["sysrq", command]:
                for qcode, down in (("alt", True), ("print", True), ("print", False), ("alt", False)):
                    qmp.send(key(qcode, down), display=True)
                qmp.send(key(command, True), display=True)
                qmp.send(key(command, False), display=True)
            case ["tablet", "press" | "release" as edge]:
                qmp.send(button(edge == "press"), display=True)
            case ["mouse", "press" | "release" as edge]:
                qmp.send(button(edge == "press"))
            case ["tablet", x, y]:
                qmp.send(axis("abs", "x", int(x)), axis("abs", "y", int(y)), display=True)
            case ["mouse" | "ps2", *rest] if rest[-2:] and all(re.fullmatch(r"-?\d+", v) for v in rest[-2:]):
                dx, dy = rest[-2:]
                qmp.send(axis("rel", "x", int(dx)), axis("rel", "y", int(dy)))
            case ["hold", "shift"]:
                qmp.send(key("shift", True), display=True)
                qmp.send(key("shift", True))
            case ["ps2", "release", "shift"]:
                qmp.send(key("shift", False))
            case ["hold", "usb", "shift"]:
                qmp.send(key("shift", True), display=True)
            case ["type", *_]:
                # One command per keystroke: the guest may power off on Enter's
                # press, so nothing may follow it on QMP.
                for ch in TYPED:
                    qcode = QCODES.get(ch, ch)
                    qmp.send(key(qcode, True), key(qcode, False), display=True)
            case _:
                return False
        return True


def grade_listing(log, model, number, ids):
    """The kconsole listing names the controller, its hub, and the stick
    behind the hub with its function bound and its bulk-in pipe idle."""
    failures = []
    ports = USB2_PORTS + USB3_PORTS
    header = rf"^usb: xhci {number} at [0-9a-f:.]+ \({ids}\) running on MSI(-X)?, {ports} ports, settled\r?$"
    if not re.search(header, log, re.M):
        failures.append(f"{model}: the kconsole listing names no running, settled controller")
    if not re.search(rf"^usb:   {number}-4 slot \d+ 0409:55aa full speed, hub\r?$", log, re.M):
        failures.append(f"{model}: the kconsole listing names no hub at {number}-4")
    stick = re.search(
        rf"^usb:   {number}-4\.1 slot \d+ 46f4:0001 full speed, configured\r?\n((?:usb:     .*\n)+)",
        log,
        re.M,
    )
    if not stick:
        return failures + [f"{model}: the kconsole listing names no stick at {number}-4.1"]
    for line in (
        "usb:     function 0 interface 0 class 08/06/50: bound usb-test",
        "usb:     ep 0x81 dci 3 queued 0",
    ):
        if line not in stick.group(1):
            failures.append(f"{model}: the listing of {number}-4.1 has no line {line!r}")
    return failures


def grade_disks(log, bench, numbers):
    """The ext4 stick is `sda` each time it is plugged and the read-only
    drive `sdb`, write-protected; each disk leaves with its device."""
    failures = []
    for bus, name, disk, protected in (("xhci1", "xhci1-disk", "sda", ""), ("xhci2", "xhci2-ro", "sdb", ", write-protected")):
        number = numbers.get(CONTROLLERS[bus][1])
        if number is None:
            continue
        where = f"{number}-{next(d.path for d in EXTRA[bus] if d.driver == 'usb-storage')}"
        named = len(re.findall(rf"^USB: {where} LUN 0 is {disk}, \d+ MB in 512-byte blocks{protected}\r?$", log, re.M))
        if named != bench.adds[name]:
            failures.append(f"{name}: named {disk} {named} times, not {bench.adds[name]}")
        removed = len(re.findall(rf"^USB: {where} {disk} removed\r?$", log, re.M))
        if removed != bench.deletes[name]:
            failures.append(f"{name}: {disk} removed {removed} times, not {bench.deletes[name]}")
    return failures


def grade_nic(log, bench, numbers):
    """The usb-net is eth1 each time it is plugged, eth0 being virtio-net's,
    and its interface leaves with it."""
    number = numbers.get(CONTROLLERS["xhci1"][1])
    if number is None:
        return []
    failures = []
    where = f"{number}-4.4"
    named = len(re.findall(rf"^USB: {where} is eth1, {NET_MAC}, ECM\r?$", log, re.M))
    if named != bench.adds["xhci1-net"]:
        failures.append(f"xhci1-net: published as eth1 {named} times, not {bench.adds['xhci1-net']}")
    removed = len(re.findall(rf"^USB: {where} eth1 removed\r?$", log, re.M))
    if removed != bench.deletes["xhci1-net"]:
        failures.append(f"xhci1-net: eth1 removed {removed} times, not {bench.deletes['xhci1-net']}")
    if re.search(r"network function declined", log):
        failures.append("a network function was declined")
    return failures


def check_images(root, build_dir):
    """The ext4 stick whole, at rest and holding what the guest fsynced. The
    read-only drive is QEMU's to keep as built."""
    image = image_of(build_dir, "xhci1-disk")
    failures = []
    checked = subprocess.run(
        [os.path.join(root, "scripts/check_fs_image.sh"), image],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    if checked.returncode != 0:
        failures.append(f"{image}: {checked.stdout.decode(errors='replace').strip()}")
    path, text = KEPT
    held = subprocess.run(["debugfs", "-R", f"cat /{path}", image], capture_output=True).stdout
    if held != text:
        failures.append(f"{image}: /{path} holds {held!r}, not {text!r}")
    return failures


def grade(log, tests, bench):
    failures = []
    for test in tests:
        name = test.rsplit("::", 1)[-1]
        if not re.search(rf"(?:^|KTAP\t)ok \d+ - \S*{re.escape(name)}(?: # time_ms=\d+)?\r?$", log, re.M):
            failures.append(f"{test} did not pass")
    numbers = {}
    for number, ids in re.findall(r"USB: xhci (\d+) at [0-9a-f:.]+ \(([0-9a-f]{4}:[0-9a-f]{4})\)", log):
        numbers[ids] = number
    for bus, (model, ids, _) in CONTROLLERS.items():
        number = numbers.get(ids)
        if number is None:
            failures.append(f"{model} ({ids}) never bound")
            continue
        for name, device_bus, driver, port, _, path, bound in devices():
            if device_bus != bus:
                continue
            adds, deletes = bench.adds[name], bench.deletes[name]
            if "." not in path:
                where = f"{number}-{path}"
                attaches = len(re.findall(rf"USB: {where} attached", log))
                detaches = len(re.findall(rf"USB: {where} detached", log))
                if (attaches, detaches) != (adds, deletes):
                    failures.append(
                        f"{model} port {path}: {attaches} attaches, {detaches} detaches, not {adds} and {deletes}"
                    )
            where = re.escape(f"{number}-{path}")
            if bound is None:
                pattern = rf"^USB: {where} [0-9a-f]{{4}}:[0-9a-f]{{4}} hub, \d+ ports, full speed\r?$"
            else:
                pattern = rf"^USB: {where} [0-9a-f]{{4}}:[0-9a-f]{{4}} 1 function, bound {bound}\r?$"
            seen = len(re.findall(pattern, log, re.M))
            if seen != adds:
                failures.append(f"{model} {driver} at {path}: enumerated {seen} times, not {adds}")
            removed = len(re.findall(rf"USB: {where} removed from slot", log))
            if removed != deletes:
                failures.append(f"{model} {driver} at {path}: removed {removed} times, not {deletes}")
        failures += grade_listing(log, model, number, ids)
        if re.search(rf"USB: xhci {number} will not be reset at poweroff", log):
            failures.append(f"{model} has no shutdown hook")
        if not re.search(rf"USB: xhci {number} reset, off the bus", log):
            failures.append(f"{model} was not reset")
        if re.search(rf"USB: xhci {number} raised no interrupt", log):
            failures.append(f"{model} answered its first command only when polled")
        for line in re.findall(rf"USB: {number}-\S+ enumeration failed.*", log):
            failures.append(f"{model}: {line}")
    failures += grade_disks(log, bench, numbers)
    failures += grade_nic(log, bench, numbers)
    caps = log.find("USB-TEST: caps")
    if caps < 0 or not re.search(r"ps2_set_ledstate \S+ ledstate 6\b", log[caps:]):
        failures.append("Caps Lock on a USB keyboard never lit the i8042's LED")
    if re.search(r"USB: unsettled", log):
        failures.append("the bus never settled at boot")
    return failures


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--iso", required=True)
    parser.add_argument("--build-dir", required=True)
    parser.add_argument("--log", required=True)
    parser.add_argument("--tests", required=True, help="comma-separated explicit tests")
    parser.add_argument("--timeout", type=int, default=1800)
    args = parser.parse_args()

    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    qmp_path = os.path.join(os.path.abspath(args.build_dir), "usb-qmp.sock")
    env = dict(os.environ)
    env["QEMU_QMP"] = qmp_path
    env["QEMU_PCI_DEVICES"] = " ".join(device_args(args.build_dir))
    env["GPU"] = "vga"
    deadline = time.monotonic() + args.timeout

    # Its own session, so a kill reaches QEMU, a child of qemu_run.sh.
    qemu = subprocess.Popen(
        [os.path.join(root, "scripts/qemu_run.sh"), "test", args.iso, os.devnull],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        env=env,
        start_new_session=True,
    )

    def kill():
        try:
            os.killpg(qemu.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass

    timer = threading.Timer(args.timeout, kill)
    timer.daemon = True
    timer.start()
    lines = []
    errors = []
    bench = None
    try:
        qmp = Qmp(qmp_path, deadline, qemu)
        bench = Bench(qmp)
        with open(args.log, "w", encoding="utf-8", errors="replace") as log:
            for raw in qemu.stdout:
                line = raw.decode("utf-8", errors="replace")
                log.write(line)
                log.flush()
                lines.append(line)
                found = re.search(r"USB: xhci (\d+) at [0-9a-f:.]+ \(([0-9a-f]{4}:[0-9a-f]{4})\)", line)
                if found:
                    bench.numbers[found.group(2)] = found.group(1)
                marker = re.search(r"USB-TEST: (.*\S)", line)
                if not marker:
                    continue
                try:
                    words = marker.group(1).split()
                    if words[0] not in ("io-mem", "pages") and not bench.act(words, max(1, deadline - time.monotonic())):
                        errors.append(f"no action for {marker.group(0)!r}")
                except (RuntimeError, OSError) as err:
                    errors.append(f"{marker.group(0)!r}: {err}")
    except RuntimeError as err:
        errors.append(str(err))
    finally:
        if qemu.poll() is None:
            kill()
        status = qemu.wait()
        timer.cancel()

    log = "".join(lines)
    failures = errors + (grade(log, args.tests.split(","), bench) if bench else [])
    failures += check_images(root, args.build_dir)
    if status != 0:
        failures.append(f"qemu_run.sh exited {status}")
    if failures:
        tail = "".join(lines[-30:])
        sys.stderr.write(tail)
        for failure in failures:
            sys.stderr.write(f"FAIL: {failure}\n")
        sys.stderr.write(f"full log in {args.log}\n")
        return 1
    print(
        "test-usb: both controllers ran, every device enumerated, bound and removed, keys and motion "
        "reached the guest, the ext4 stick survived its pull, the read-only drive mounted read-only, "
        f"the USB NIC leased, carried TCP and left, both reset — {args.log}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

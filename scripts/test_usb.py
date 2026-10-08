#!/usr/bin/env python3
"""The host half of `just test-usb`: boots the tests ISO with both of QEMU's
xHCI models, each carrying a SuperSpeed stick, QEMU's full-speed hub with a
stick, a tablet and a mouse behind it, a high-speed stick and a full-speed
keyboard, and acts on the guest's `USB-TEST:` lines: it plugs and pulls
devices through QMP and injects keys and motion with `input-send-event`. The
run passes when the suite is green and the log shows every device
enumerated, bound where a driver matches it, and removed, each time.

QEMU sends an event that names no display to the unbound device activated
most recently, and one that names `video0` to a device bound to it. The
keyboards and tablets are bound, so a key or a button with no display reaches
the i8042, and motion with none the newest usb-mouse, or the PS/2 mouse once
both are pulled."""

import argparse
import json
import os
import re
import signal
import socket
import subprocess
import sys
import threading
import time
from typing import NamedTuple

CONTROLLERS = {
    "xhci1": ("qemu-xhci", "1b36:000d", ""),
    "xhci2": ("nec-usb-xhci", "1033:0194", ",msix=off"),
}
USB3_PORTS = 2
USB2_PORTS = 4
STICK_BYTES = 1 << 20
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
ROOT_PORTS = ("1", "4", "5", "6")
ROUNDS = 2
TYPED = "exit 7\n"
QCODES = {" ": "spc", "\n": "ret"}


def devices():
    for bus in CONTROLLERS:
        for suffix, driver, port, props, path, bound in DEVICES:
            yield f"{bus}-{suffix}", bus, driver, port, props, path, bound


def device_args(build_dir):
    """Each stick is a `-blockdev` node: QEMU deletes a `-drive` backend with
    the device that used it."""
    args = ["-parallel", "none", "-trace", "ps2_set_ledstate"]
    for bus, (model, _, extra) in CONTROLLERS.items():
        args += ["-device", f"{model},id={bus},p2={USB2_PORTS},p3={USB3_PORTS}{extra}"]
    for name, bus, driver, port, props, _, _ in devices():
        device = f"{driver},id={name},bus={bus}.0,port={port}"
        device += "".join(f",{key}={value}" for key, value in props.items())
        if driver == "usb-storage":
            image = os.path.join(build_dir, f"usb-{name}.img")
            with open(image, "wb") as f:
                f.truncate(STICK_BYTES)
            args += [
                "-blockdev",
                f"driver=raw,node-name={name},file.driver=file,file.filename={image}",
            ]
            device += f",drive={name},removable=on"
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
        self.present = {name for name, *_ in devices()}
        self.adds = {name: 1 for name in self.present}
        self.deletes = {name: 0 for name in self.present}
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

    def plug(self):
        for name, bus, driver, port, props, _, _ in sorted(devices(), key=lambda d: "." in d[3]):
            if name in self.present:
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
                for ch in TYPED:
                    qcode = QCODES.get(ch, ch)
                    qmp.send(key(qcode, True), display=True)
                    qmp.send(key(qcode, False), display=True)
            case _:
                return False
        return True


def grade_listing(log, model, number, ids):
    """The kconsole listing names the controller, its hub, and the stick
    behind the hub with its function bound and its bulk-in pipe idle."""
    failures = []
    header = rf"^usb: xhci {number} at [0-9a-f:.]+ \({ids}\) running on MSI(-X)?, 6 ports, settled\r?$"
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
            if path in ROOT_PORTS:
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
                except RuntimeError as err:
                    errors.append(str(err))
    except RuntimeError as err:
        errors.append(str(err))
    finally:
        if qemu.poll() is None:
            kill()
        status = qemu.wait()
        timer.cancel()

    log = "".join(lines)
    failures = errors + (grade(log, args.tests.split(","), bench) if bench else [])
    if status != 0:
        failures.append(f"qemu_run.sh exited {status}")
    if failures:
        tail = "".join(lines[-30:])
        sys.stderr.write(tail)
        for failure in failures:
            sys.stderr.write(f"FAIL: {failure}\n")
        sys.stderr.write(f"full log in {args.log}\n")
        return 1
    print(f"test-usb: both controllers ran, every device enumerated, bound and removed, keys and motion reached the guest, both reset — {args.log}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""The host half of `just test-usb`: boots the tests ISO with both of QEMU's
xHCI models, each carrying a SuperSpeed stick, QEMU's full-speed hub with a
stick and a tablet behind it, a high-speed stick and a full-speed keyboard,
and at the guest's `USB-TEST: pull` and `USB-TEST: plug` deletes or re-adds
every device through QMP. The run passes when the suite is green and the log
shows every device enumerated, bound where a test driver matches it, and
removed, each time."""

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

CONTROLLERS = {
    "xhci1": ("qemu-xhci", "1b36:000d", ""),
    "xhci2": ("nec-usb-xhci", "1033:0194", ",msix=off"),
}
USB3_PORTS = 2
USB2_PORTS = 4
STICK_BYTES = 1 << 20

# (id suffix, driver, connector, extra properties, path, the test driver
# that binds it).
# QEMU numbers the USB 3 ports first: a SuperSpeed device on connector N
# lands on root port N, any other device on connector N on root port p3 + N.
# A device behind the hub on connector 2 is at port "2.M", path "4.M".
DEVICES = (
    ("ss", "usb-storage", "1", "", "1", "usb-test"),
    ("hub", "usb-hub", "2", "", "4", None),
    ("hub-stick", "usb-storage", "2.1", "", "4.1", "usb-test"),
    ("hub-tablet", "usb-tablet", "2.2", "", "4.2", "usb-test-hid"),
    ("hs", "usb-storage", "3", "", "5", "usb-test"),
    ("fs", "usb-kbd", "4", ",usb_version=1", "6", "usb-test-hid"),
)
ROOT_PORTS = ("1", "4", "5", "6")
ROUNDS = 2


def devices():
    for bus in CONTROLLERS:
        for suffix, driver, port, extra, path, bound in DEVICES:
            yield f"{bus}-{suffix}", bus, driver, port, extra, path, bound


def device_args(build_dir):
    """Each stick is a `-blockdev` node: QEMU deletes a `-drive` backend with
    the device that used it."""
    args = []
    for bus, (model, _, extra) in CONTROLLERS.items():
        args += ["-device", f"{model},id={bus},p2={USB2_PORTS},p3={USB3_PORTS}{extra}"]
    for name, bus, driver, port, extra, _, _ in devices():
        device = f"{driver},id={name},bus={bus}.0,port={port}{extra}"
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


def pull(qmp, timeout):
    """A hub's devices go first: deleting a hub leaves the devices on its
    ports behind as orphans whose ids a later plug would collide with."""
    behind = [name for name, _, _, port, _, _, _ in devices() if "." in port]
    rest = [name for name, _, _, port, _, _, _ in devices() if "." not in port]
    for group in (behind, rest):
        for name in group:
            qmp.execute("device_del", id=name)
        qmp.wait_deleted(group, time.monotonic() + timeout)


def plug(qmp):
    for name, bus, driver, port, extra, _, _ in sorted(devices(), key=lambda d: "." in d[3]):
        arguments = {"driver": driver, "id": name, "bus": f"{bus}.0", "port": port}
        if driver == "usb-storage":
            arguments.update(drive=name, removable=True)
        if extra:
            key, value = extra.lstrip(",").split("=")
            arguments[key] = int(value)
        qmp.execute("device_add", **arguments)


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


def grade(log, tests):
    failures = []
    for test in tests:
        name = test.rsplit("::", 1)[-1]
        if not re.search(rf"^(KTAP\t)?ok \d+ - \S*{re.escape(name)}(?: # time_ms=\d+)?\r?$", log, re.M):
            failures.append(f"{test} did not pass")
    numbers = {}
    for number, ids in re.findall(r"USB: xhci (\d+) at [0-9a-f:.]+ \(([0-9a-f]{4}:[0-9a-f]{4})\)", log):
        numbers[ids] = number
    times = ROUNDS + 1
    for bus, (model, ids, _) in CONTROLLERS.items():
        number = numbers.get(ids)
        if number is None:
            failures.append(f"{model} ({ids}) never bound")
            continue
        for port in ROOT_PORTS:
            name = f"{number}-{port}"
            attaches = len(re.findall(rf"USB: {name} attached", log))
            detaches = len(re.findall(rf"USB: {name} detached", log))
            if attaches != times or detaches != ROUNDS:
                failures.append(f"{model} port {port}: {attaches} attaches, {detaches} detaches")
        for _, driver, _, _, path, bound in DEVICES:
            name = re.escape(f"{number}-{path}")
            if bound is None:
                pattern = rf"^USB: {name} [0-9a-f]{{4}}:[0-9a-f]{{4}} hub, \d+ ports, full speed\r?$"
            else:
                pattern = rf"^USB: {name} [0-9a-f]{{4}}:[0-9a-f]{{4}} 1 function, bound {bound}\r?$"
            seen = len(re.findall(pattern, log, re.M))
            if seen != times:
                failures.append(f"{model} {driver} at {path}: enumerated {seen} times, not {times}")
            removed = len(re.findall(rf"USB: {name} removed from slot", log))
            if removed != ROUNDS:
                failures.append(f"{model} {driver} at {path}: removed {removed} times, not {ROUNDS}")
        failures += grade_listing(log, model, number, ids)
        if not re.search(rf"USB: xhci {number} reset, off the bus", log):
            failures.append(f"{model} was not reset")
        if re.search(rf"USB: xhci {number} will not be reset at poweroff", log):
            failures.append(f"{model} has no shutdown hook")
        if re.search(rf"USB: xhci {number} raised no interrupt", log):
            failures.append(f"{model} answered its first command only when polled")
        for line in re.findall(rf"USB: {number}-\S+ enumeration failed.*", log):
            failures.append(f"{model}: {line}")
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
    try:
        qmp = Qmp(qmp_path, deadline, qemu)
        with open(args.log, "w", encoding="utf-8", errors="replace") as log:
            for raw in qemu.stdout:
                line = raw.decode("utf-8", errors="replace")
                log.write(line)
                log.flush()
                lines.append(line)
                try:
                    if "USB-TEST: pull" in line:
                        pull(qmp, max(1, deadline - time.monotonic()))
                    elif "USB-TEST: plug" in line:
                        plug(qmp)
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
    failures = errors + grade(log, args.tests.split(","))
    if status != 0:
        failures.append(f"qemu_run.sh exited {status}")
    if failures:
        tail = "".join(lines[-30:])
        sys.stderr.write(tail)
        for failure in failures:
            sys.stderr.write(f"FAIL: {failure}\n")
        sys.stderr.write(f"full log in {args.log}\n")
        return 1
    print(f"test-usb: both controllers ran, every device enumerated, bound and removed {ROUNDS} times over, both reset — {args.log}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

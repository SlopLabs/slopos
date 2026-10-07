#!/usr/bin/env python3
"""The host half of `just test-usb`: boots the tests ISO with both of QEMU's
xHCI models and a stick on each connector, and at the guest's `USB-TEST: pull`
and `USB-TEST: plug` deletes or re-adds every stick through QMP. The run
passes when the suite is green and the log shows each stick's port attach,
detach and attach again, and each controller reset."""

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
CONNECTORS = (1, 2, 3, 4)
USB3_PORTS = 2
USB2_PORTS = 4
STICK_BYTES = 1 << 20


def port_of(connector):
    """The root port a stick on `connector` lands on: QEMU numbers the USB 3
    ports first, and a high-speed device on connector N takes USB 2 port
    p3 + N."""
    return connector if connector <= USB3_PORTS else USB3_PORTS + connector


def sticks():
    for bus in CONTROLLERS:
        for connector in CONNECTORS:
            yield f"{bus}-{connector}", bus, connector


def device_args(build_dir):
    """Each stick is a `-blockdev` node: QEMU deletes a `-drive` backend with
    the device that used it."""
    args = []
    for bus, (model, _, extra) in CONTROLLERS.items():
        args += ["-device", f"{model},id={bus},p2={USB2_PORTS},p3={USB3_PORTS}{extra}"]
    for stick, bus, connector in sticks():
        image = os.path.join(build_dir, f"usb-{stick}.img")
        with open(image, "wb") as f:
            f.truncate(STICK_BYTES)
        args += [
            "-blockdev",
            f"driver=raw,node-name={stick},file.driver=file,file.filename={image}",
            "-device",
            f"usb-storage,id={stick},drive={stick},bus={bus}.0,port={connector},removable=on",
        ]
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
    ids = [stick for stick, _, _ in sticks()]
    for stick in ids:
        qmp.execute("device_del", id=stick)
    qmp.wait_deleted(ids, time.monotonic() + timeout)


def plug(qmp):
    for stick, bus, connector in sticks():
        qmp.execute(
            "device_add",
            driver="usb-storage",
            id=stick,
            drive=stick,
            bus=f"{bus}.0",
            port=str(connector),
            removable=True,
        )


def grade(log, tests):
    failures = []
    for test in tests:
        name = test.rsplit("::", 1)[-1]
        if not re.search(rf"^(KTAP\t)?ok \d+ - \S*{re.escape(name)}(?: # time_ms=\d+)?\r?$", log, re.M):
            failures.append(f"{test} did not pass")
    numbers = {}
    for number, ids in re.findall(r"USB: xhci (\d+) at [0-9a-f:.]+ \(([0-9a-f]{4}:[0-9a-f]{4})\)", log):
        numbers[ids] = number
    for bus, (model, ids, _) in CONTROLLERS.items():
        number = numbers.get(ids)
        if number is None:
            failures.append(f"{model} ({ids}) never bound")
            continue
        for connector in CONNECTORS:
            port = port_of(connector)
            name = f"{number}-{port}"
            attaches = len(re.findall(rf"USB: {name} attached", log))
            detaches = len(re.findall(rf"USB: {name} detached", log))
            if attaches < 2 or detaches < 1:
                failures.append(f"{model} port {port}: {attaches} attaches, {detaches} detaches")
        if not re.search(rf"USB: xhci {number} reset, off the bus", log):
            failures.append(f"{model} was not reset")
        if re.search(rf"USB: xhci {number} will not be reset at poweroff", log):
            failures.append(f"{model} has no shutdown hook")
        if re.search(rf"USB: xhci {number} raised no interrupt", log):
            failures.append(f"{model} answered its first command only when polled")
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
    print(f"test-usb: both controllers ran, every stick detached and attached again, both reset — {args.log}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

"""A QMP client for driving a QEMU the host started: device plugging, input
injection and screendumps."""

import json
import socket
import time

# An event a guest poll has not taken yet merges with the next.
PACE = 0.15


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

    def send(self, *events, device=None):
        """One `input-send-event` per call, to `device` or to whichever device
        QEMU picks for an event that names none, paced so the guest takes
        each."""
        arguments = {"events": list(events)}
        if device:
            arguments["device"] = device
        self.execute("input-send-event", **arguments)
        time.sleep(PACE)

    def screendump(self, path):
        self.execute("screendump", filename=path, format="png")


def key(qcode, down):
    return {"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": qcode}}}


def button(down, which="left"):
    return {"type": "btn", "data": {"down": down, "button": which}}


def axis(kind, name, value):
    return {"type": kind, "data": {"axis": name, "value": value}}

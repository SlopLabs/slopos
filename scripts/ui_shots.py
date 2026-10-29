#!/usr/bin/env python3
"""The host half of `just ui-shots`: boots the live ISO under QEMU with a QMP
socket, a USB tablet for absolute pointer moves and a blank scratch disk, and
plays scenes of keys, clicks and commands typed at the serial console's
shell, saving a PNG of each surface under the output directory.

A scene waits for what the guest says on the serial line, as the compositor's
`COMPOSITOR: shown <app id>` when a window first appears, and then for the
screen to hold still, before it saves the screen. The shots are for people to
look at; nothing here grades them."""

import argparse
import os
import re
import signal
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))
from qmp import Qmp, axis, button, key  # noqa: E402

SCENES = ("installer", "chrome", "desktop", "apps", "gallery")
APPS = (
    ("terminal", "terminal", "org.slopos.terminal"),
    ("system-monitor", "sysmon", "org.slopos.sysmon"),
    ("system-monitor-memory", "sysmon --page memory", "org.slopos.sysmon"),
    ("system-monitor-disk", "sysmon --page disk", "org.slopos.sysmon"),
    ("system-monitor-network", "sysmon --page network", "org.slopos.sysmon"),
    ("system-monitor-gpu", "sysmon --page gpu", "org.slopos.sysmon"),
    ("system-monitor-processes", "sysmon --page processes", "org.slopos.sysmon"),
    ("editor", "editor", "org.slopos.editor"),
    ("files", "file_manager", "org.slopos.files"),
    ("image-viewer", "image_viewer /usr/share/slopos/wallpapers/default.png", "org.slopos.image-viewer"),
)
TABLET_MAX = 0x7FFF
QCODES = {" ": "spc", "\n": "ret", "-": "minus", "/": "slash", ".": "dot"}
BLANK_BYTES = 16 << 30


def stated(source, pattern, name):
    """`name`'s value, read from the source that states it."""
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    path = os.path.join(root, source)
    with open(path, encoding="utf-8") as f:
        found = re.search(pattern, f.read(), re.S)
    if not found:
        sys.exit(f"ui_shots: no {name} in {path}")
    return found.group(1)


BAR_HEIGHT = int(stated("chrome-core/src/status.rs", r"pub const BAR_HEIGHT: i32 = (\d+);", "BAR_HEIGHT"))
GALLERY = tuple(re.findall(r'"([^"]+)"', stated(
    "appkit/src/catalogue.rs", r"pub const PAGES: \[&str; \d+\] = \[(.*?)\];", "PAGES")))


def below_bar(path):
    """The pixels of a binary PPM below the menu bar."""
    with open(path, "rb") as f:
        data = f.read()
    fields, at = [], 0
    while len(fields) < 4:
        while data[at:at + 1].isspace():
            at += 1
        end = at
        while not data[end:end + 1].isspace():
            end += 1
        fields.append(data[at:end])
        at = end
    width = int(fields[1])
    return data[at + 1 + width * 3 * BAR_HEIGHT:]


class Guest:
    def __init__(self, args):
        self.out = args.out
        self.width, self.height = args.width, args.height
        self.text = ""
        self.cursor = 0
        self.ended = False
        self.cond = threading.Condition()
        self.log = open(os.path.join(args.out, "serial.log"), "w", encoding="utf-8", errors="replace")
        self.shots = []
        self.shown = None
        blank = os.path.join(args.out, "blank.img")
        with open(blank, "wb") as f:
            f.truncate(BLANK_BYTES)
        qmp_path = os.path.join(args.out, "qmp.sock")
        env = dict(os.environ)
        env.update(
            QEMU_QMP=qmp_path,
            VIDEO="0",
            QEMU_NO_ROOT_DISK="1",
            QEMU_PCI_DEVICES=" ".join([
                "-device", "qemu-xhci,id=uishots",
                "-device", "usb-tablet,bus=uishots.0",
                "-drive", f"file={blank},if=none,id=blank,format=raw",
                "-device", "nvme,serial=ui-shots-blank,drive=blank",
            ]),
        )
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        # Its own session, so a kill reaches QEMU, a child of qemu_run.sh.
        self.qemu = subprocess.Popen(
            [os.path.join(root, "scripts/qemu_run.sh"), "interactive", args.iso, os.devnull],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            env=env,
            start_new_session=True,
        )
        threading.Thread(target=self._read, daemon=True).start()
        self.qmp = Qmp(qmp_path, time.monotonic() + 60, self.qemu)

    def _read(self):
        """The serial line as it comes, a prompt with no newline after it
        included."""
        fd = self.qemu.stdout.fileno()
        while chunk := os.read(fd, 4096):
            text = chunk.decode("utf-8", errors="replace")
            self.log.write(text)
            self.log.flush()
            with self.cond:
                self.text += text
                self.cond.notify_all()
        with self.cond:
            self.ended = True
            self.cond.notify_all()

    def close(self):
        if self.qemu.poll() is None:
            try:
                os.killpg(self.qemu.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        self.qemu.wait()
        self.log.close()

    def wait_line(self, pattern, timeout):
        """The first match of `pattern`, a line at a time, in what the serial
        line said since the last match."""
        found = re.compile(pattern, re.M)
        deadline = time.monotonic() + timeout
        with self.cond:
            while True:
                match = found.search(self.text, self.cursor)
                if match:
                    self.cursor = match.end()
                    return match
                if self.ended:
                    raise RuntimeError(f"QEMU exited waiting for {pattern!r}")
                left = deadline - time.monotonic()
                if left <= 0:
                    raise RuntimeError(f"waited {timeout}s for {pattern!r}")
                self.cond.wait(left)

    def drain(self):
        with self.cond:
            self.cursor = len(self.text)

    def settle(self, timeout, still=2.0, changed=False):
        """Waits until the screen below the menu bar, whose clock moves on
        its own, has not changed for `still` seconds, and with `changed` until
        it is no longer the last shot's."""
        probe = os.path.join(self.out, ".probe.ppm")
        deadline = time.monotonic() + timeout
        last, since = None, time.monotonic()
        while time.monotonic() < deadline:
            self.qmp.execute("screendump", filename=probe, format="ppm")
            now = below_bar(probe)
            if changed and now == self.shown:
                pass
            elif now != last:
                last, since = now, time.monotonic()
            elif time.monotonic() - since >= still:
                self.shown = now
                return
            time.sleep(0.5)
        print(f"ui-shots: the screen never held still for {still}s; saving it as it is", flush=True)

    def shot(self, name, timeout=120, changed=False, still=2.0):
        self.settle(timeout, still=still, changed=changed)
        path = os.path.join(self.out, f"{name}.png")
        self.qmp.screendump(path)
        self.shots.append(path)
        print(f"ui-shots: {path}", flush=True)

    def press(self, *qcodes):
        """A chord: each key down in order, then up in reverse."""
        for q in qcodes:
            self.qmp.send(key(q, True))
        for q in reversed(qcodes):
            self.qmp.send(key(q, False))

    def type_keys(self, text):
        for ch in text:
            self.press(QCODES.get(ch, ch))

    def point(self, x, y):
        ax = x * TABLET_MAX // max(1, self.width - 1)
        ay = y * TABLET_MAX // max(1, self.height - 1)
        self.qmp.send(axis("abs", "x", ax), axis("abs", "y", ay))

    def click(self, x, y):
        self.point(x, y)
        self.qmp.send(button(True))
        self.qmp.send(button(False))

    def serial(self, text):
        self.qemu.stdin.write(text.encode())
        self.qemu.stdin.flush()


def installer(g, install):
    g.wait_line(r"COMPOSITOR: shown org\.slopos\.installer", 900)
    g.shot("installer-welcome")
    g.press("ret")
    g.wait_line(r"COMPOSITOR: shown a question", 300)
    # The welcome page's bar moves while it waits for the answer.
    g.shot("consent-dialog", timeout=5)
    # Input is ignored while a question settles in, then Tab reaches Allow.
    time.sleep(1.5)
    g.press("tab")
    time.sleep(1.5)
    g.press("ret")
    g.shot("installer-disk", timeout=300, changed=True)
    g.press("ret")
    g.shot("installer-how", changed=True)
    g.press("ret")
    g.shot("installer-review", changed=True)
    if not install:
        return
    # Tab reaches the confirming checkbox; Shift+Tab wraps round to Install.
    g.press("tab")
    g.press("spc")
    g.press("shift", "tab")
    g.press("spc")
    time.sleep(5)
    path = os.path.join(g.out, "installer-install.png")
    g.qmp.screendump(path)
    g.shots.append(path)
    # The progress moves until the install ends.
    g.shot("installer-done", timeout=3600, changed=True, still=15)


def log_in(g):
    """A shell on the serial console, as the live system's one person."""
    g.serial("\n")
    g.wait_line(r"login:", 120)
    g.serial("user\n")
    time.sleep(2)
    g.serial("\n")
    time.sleep(3)
    g.serial("echo UI-SHOTS: $((6 * 7))\n")
    g.wait_line(r"UI-SHOTS: 42", 60)


def close_installer(g):
    g.drain()
    g.serial("ps\n")
    pid = g.wait_line(r"^\s*(\d+)\s.*installer-ui", 30).group(1)
    g.serial(f"kill {pid}\n")
    time.sleep(2)


def run_app(g, name, command, app_id):
    g.drain()
    g.serial(f"{command} &\n")
    g.wait_line(rf"COMPOSITOR: shown {re.escape(app_id)}", 300)
    # A terminal's caret blinks for as long as it runs.
    g.shot(name, timeout=30)
    g.serial("kill $!\n")
    time.sleep(2)


def chrome(g):
    g.click(12, 12)
    g.shot("system-menu", changed=True)
    # Up from no entry is the last, Power off…, which asks first.
    g.press("up")
    g.press("ret")
    g.shot("power-off-question", changed=True)
    g.press("esc")
    g.click(g.width - 20, 12)
    g.shot("clock", changed=True)
    g.press("esc")
    g.press("ctrl", "alt", "delete")
    g.shot("lock-screen", changed=True)
    # Tab from the password field reaches Switch user….
    g.press("tab")
    g.press("ret")
    g.shot("login-screen", changed=True)
    # Typing chooses the person, Enter moves to the password, Enter logs in.
    g.type_keys("user\n\n")
    g.settle(60)


def desktop(g):
    """Two windows, the one behind without focus, and the pointer on the
    dock's middle icon, which grows and names its app."""
    pids = []
    for command, app_id in (("file_manager", "org.slopos.files"), ("sysmon", "org.slopos.sysmon")):
        g.drain()
        g.serial(f"{command} & echo UI-JOB: $!\n")
        pids.append(g.wait_line(r"UI-JOB: (\d+)", 30).group(1))
        g.wait_line(rf"COMPOSITOR: shown {re.escape(app_id)}", 300)
    g.point(g.width // 2, g.height - 44)
    g.shot("desktop", timeout=30)
    for pid in pids:
        g.serial(f"kill {pid}\n")
        time.sleep(1)
    time.sleep(1)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--iso", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--width", type=int, default=1280)
    parser.add_argument("--height", type=int, default=800)
    parser.add_argument("--no-install", action="store_true", help="stop the installer at its review page")
    parser.add_argument("scenes", nargs="*", help=f"any of {', '.join(SCENES)}; all by default")
    args = parser.parse_args()
    scenes = args.scenes or list(SCENES)
    unknown = [s for s in scenes if s not in SCENES]
    if unknown:
        parser.error(f"no scene {unknown}; the scenes are {', '.join(SCENES)}")
    args.out = os.path.abspath(args.out)
    os.makedirs(args.out, exist_ok=True)
    for old in os.listdir(args.out):
        if old.endswith(".png"):
            os.remove(os.path.join(args.out, old))

    g = Guest(args)
    try:
        if "installer" in scenes:
            installer(g, not args.no_install)
        else:
            g.wait_line(r"COMPOSITOR: shown org\.slopos\.installer", 900)
        log_in(g)
        close_installer(g)
        if "chrome" in scenes:
            chrome(g)
        if "desktop" in scenes:
            desktop(g)
        if "apps" in scenes:
            for name, command, app_id in APPS:
                run_app(g, name, command, app_id)
        if "gallery" in scenes:
            for page in GALLERY:
                run_app(g, f"gallery-{page}", f"widget_gallery --page {page}", "org.slopos.widget-gallery")
    except RuntimeError as err:
        sys.stderr.write(f"ui-shots: {err}; the serial log is {g.log.name}\n")
        return 1
    finally:
        g.close()
    print(f"ui-shots: {len(g.shots)} shots in {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

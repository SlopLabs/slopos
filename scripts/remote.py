#!/usr/bin/env python3
"""Drive a SlopOS machine from this host over the LAN.

`/bin/remoted` on the SlopOS machine dials out to `serve`, the broker, on one
TCP port; everything else here is the CLI, which reaches the broker through a
unix socket and is spliced onto one of the agent's idle connections per
request. The wire protocol is remote-core's (remote-core/src/lib.rs).

A machine is paired by the base it boots: `provision` writes the broker's CA,
a token and the broker's address into a directory, and a base built with
`REMOTE_PAIRING_DIR` naming it carries them at /usr/share/slopos/remote, the
one place remoted reads them from.

    serve [--port 7330] [--bind ADDR] [--advertise HOST] [--offer NAME=PATH ...]
    provision DIR [--broker HOST:PORT]   a pairing, for building a base
    status [--json] | wait [--tag T] [--timeout S] [--json]
    run [--timeout S] [--cwd D] [--env K=V] [--stdin] -- ARGV...
    sh [--timeout S] 'CMDLINE'          through /bin/shell -c
    push LOCAL REMOTE | pull REMOTE LOCAL   SHA-256 checked end to end
    klog [--follow]
    install --kernel ELF --base IMG [--tag T] [--commit] [--timeout S]

Every command but `serve` and `provision` goes to the newest boot connected
(`--boot ID` names another) and fails, after `--connect-timeout` seconds, when
none is.

State (CA, broker certificate, token, the broker's address) lives in
${XDG_CONFIG_HOME:-~/.config}/slopos-remote; the broker's socket in
$XDG_RUNTIME_DIR. Python 3 standard library only; `openssl` mints the keys.
"""

import argparse
import asyncio
import hashlib
import hmac
import io
import json
import os
import secrets
import shutil
import socket
import ssl
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

# remote-core's constants.
PROTOCOL = "1"
HEADER = struct.Struct(">BI")
MAX_PAYLOAD = 1 << 20
CHUNK = 64 * 1024
DEFAULT_PORT = 7330
SERVER_NAME = "slopos-remote"

HELLO, WELCOME, PING, PONG = 0x01, 0x02, 0x03, 0x04
EXEC, PUT, GET = 0x10, 0x11, 0x12
DATA, STDOUT, STDERR, EOF = 0x20, 0x21, 0x22, 0x23
ACCEPT, EXIT, DONE, ERROR = 0x30, 0x31, 0x32, 0x3F
# Between the CLI and the broker only: a JSON request, a JSON answer.
CONTROL, ANSWER = 0x40, 0x41
TERMINAL = (EXIT, DONE, ERROR)

PING_EVERY = 15
DEAD_AFTER = 45
SCRATCH = "/var/lib/slopos-remote/install"


class RemoteError(Exception):
    pass


def frame(kind, payload=b""):
    if len(payload) > MAX_PAYLOAD:
        raise ValueError("frame payload past MAX_PAYLOAD")
    return HEADER.pack(kind, len(payload)) + payload


def encode_fields(pairs):
    out = bytearray()
    for key, value in pairs:
        if isinstance(value, str):
            value = value.encode()
        k = key.encode("ascii")
        out += bytes([len(k)]) + k + struct.pack(">I", len(value)) + value
    return bytes(out)


class Fields:
    def __init__(self, pairs):
        self.pairs = pairs

    @classmethod
    def decode(cls, data):
        pairs, i = [], 0
        while i < len(data):
            klen = data[i]
            if i + 1 + klen + 4 > len(data):
                raise RemoteError("a malformed control payload")
            key = data[i + 1 : i + 1 + klen].decode("ascii")
            (vlen,) = struct.unpack_from(">I", data, i + 1 + klen)
            start = i + 1 + klen + 4
            if start + vlen > len(data):
                raise RemoteError("a malformed control payload")
            pairs.append((key, data[start : start + vlen]))
            i = start + vlen
        return cls(pairs)

    def get(self, key, default=None):
        for k, v in self.pairs:
            if k == key:
                return v
        return default

    def text(self, key, default=""):
        v = self.get(key)
        return default if v is None else v.decode("utf-8", "replace")


def error_text(payload):
    try:
        return Fields.decode(payload).text("msg") or "unspecified error"
    except RemoteError:
        return "unreadable error"


# --- a base's pairing -------------------------------------------------------

PAIRING_DIR = "usr/share/slopos/remote/"
PAIRING_FILES = ("remote.conf", "ca.pem", "token")


def base_pairing(path):
    """The pairing files a base image (a newc cpio) carries, by name."""
    found = {}
    with open(path, "rb") as f:
        while True:
            head = f.read(110)
            if len(head) < 110 or head[:6] != b"070701":
                raise RemoteError(f"{path} is not a SlopOS base (a newc cpio)")
            size, name_len = int(head[54:62], 16), int(head[94:102], 16)
            name = f.read(name_len)[:-1].decode("utf-8", "replace").lstrip("/")
            f.seek((-(110 + name_len)) % 4, os.SEEK_CUR)
            if name == "TRAILER!!!":
                return found
            if name.startswith(PAIRING_DIR) and name[len(PAIRING_DIR):] in PAIRING_FILES:
                found[name[len(PAIRING_DIR):]] = f.read(size)
            else:
                f.seek(size, os.SEEK_CUR)
            f.seek((-size) % 4, os.SEEK_CUR)


def check_paired(state, path):
    """Refuse a base that would boot a machine this broker cannot reach."""
    pairing = base_pairing(path)
    token = pairing.get("token", b"").strip()
    if not token:
        raise RemoteError(f"{path} carries no remote pairing: the machine would boot unreachable. "
                          "Build the base with REMOTE_PAIRING_DIR (just remote-serve and just remote-install do)")
    if not hmac.compare_digest(token, state.token.encode()) or pairing.get("ca.pem") != state.ca_pem:
        raise RemoteError(f"{path} is paired with another broker than {state.path}'s")
    conf = pairing.get("remote.conf", b"").decode("utf-8", "replace")
    return next((line.split("=", 1)[1].strip() for line in conf.splitlines()
                 if line.split("=", 1)[0].strip() == "broker"), "?")


# --- state ------------------------------------------------------------------


def log(msg):
    print(f"remote: {msg}", flush=True)


def die(msg, code=1):
    print(f"remote: {msg}", file=sys.stderr, flush=True)
    sys.exit(code)


def default_state():
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config")
    return Path(os.environ.get("SLOPOS_REMOTE_STATE") or Path(base) / "slopos-remote")


def default_socket(state):
    if os.environ.get("SLOPOS_REMOTE_SOCKET"):
        return Path(os.environ["SLOPOS_REMOTE_SOCKET"])
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    return Path(runtime) / "slopos-remote.sock" if runtime else state / "broker.sock"


def write_private(path, data):
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    except BaseException:
        os.unlink(tmp)
        raise


OPENSSL_CNF = """[req]
distinguished_name = dn
prompt = no
[dn]
[ca]
basicConstraints = critical, CA:TRUE, pathlen:0
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
[leaf]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = serverAuth
subjectAltName = DNS:{name}
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid
"""


def openssl(*args):
    if not shutil.which("openssl"):
        die("openssl is needed to mint the broker's keys")
    run = subprocess.run(["openssl", *args], capture_output=True, text=True)
    if run.returncode != 0:
        raise RemoteError(f"openssl {args[0]}: {run.stderr.strip()}")


def mint(state):
    """A CA, a P-256 leaf it signs for `slopos-remote`, and the agents' token."""
    log(f"minting a CA, the broker's certificate and a token in {state}")
    with tempfile.TemporaryDirectory(dir=state) as tmp:
        tmp = Path(tmp)
        (tmp / "openssl.cnf").write_text(OPENSSL_CNF.format(name=SERVER_NAME))
        cnf = str(tmp / "openssl.cnf")
        for key in ("ca.key", "leaf.key"):
            openssl("genpkey", "-algorithm", "EC", "-pkeyopt", "ec_paramgen_curve:P-256",
                    "-out", str(tmp / key))
        # A day of slack each way for a SlopOS clock that is off.
        start = time.strftime("%Y%m%d%H%M%SZ", time.gmtime(time.time() - 86400))
        window = ["-not_before", start, "-days", "3651"]
        openssl("req", "-x509", "-new", "-config", cnf, "-extensions", "ca", "-key",
                str(tmp / "ca.key"), "-subj", "/CN=slopos-remote CA", "-sha256",
                "-out", str(tmp / "ca.pem"), *window)
        openssl("req", "-new", "-config", cnf, "-key", str(tmp / "leaf.key"),
                "-subj", f"/CN={SERVER_NAME}", "-out", str(tmp / "leaf.csr"))
        openssl("x509", "-req", "-in", str(tmp / "leaf.csr"), "-CA", str(tmp / "ca.pem"),
                "-CAkey", str(tmp / "ca.key"), "-set_serial", str(secrets.randbits(63)),
                "-sha256", "-extfile", cnf, "-extensions", "leaf",
                "-out", str(tmp / "leaf.pem"), *window)
        for name in ("ca.key", "ca.pem", "leaf.key", "leaf.pem"):
            write_private(state / name, (tmp / name).read_bytes())
    write_private(state / "token", (secrets.token_hex(16) + "\n").encode())


STATE_FILES = ("ca.key", "ca.pem", "leaf.key", "leaf.pem", "token")


class State:
    def __init__(self, path):
        self.path = Path(path)

    def ensure(self):
        self.path.mkdir(parents=True, exist_ok=True)
        os.chmod(self.path, 0o700)
        if not all((self.path / n).exists() for n in STATE_FILES):
            mint(self.path)
        return self

    @property
    def token(self):
        try:
            return (self.path / "token").read_text().strip()
        except FileNotFoundError:
            raise RemoteError(f"no broker state in {self.path}: run `just remote-serve` first") from None

    @property
    def ca_pem(self):
        return (self.path / "ca.pem").read_bytes()

    @property
    def broker(self):
        """The address the last `provision` paired a base with."""
        try:
            return (self.path / "broker").read_text().strip() or None
        except FileNotFoundError:
            return None


# --- the broker -------------------------------------------------------------


async def read_frame(reader):
    head = await reader.readexactly(HEADER.size)
    kind, length = HEADER.unpack(head)
    if length > MAX_PAYLOAD:
        raise RemoteError(f"a frame of {length} bytes, past the bound")
    return kind, (await reader.readexactly(length) if length else b"")


async def accepted_streams(sock, tls=None):
    loop = asyncio.get_running_loop()
    reader = asyncio.StreamReader(limit=64 * 1024)
    protocol = asyncio.StreamReaderProtocol(reader)
    extra = {"ssl": tls, "ssl_handshake_timeout": 15} if tls else {}
    transport, _ = await loop.connect_accepted_socket(lambda: protocol, sock=sock, **extra)
    return reader, asyncio.StreamWriter(transport, protocol, reader, loop)


async def peek_byte(sock):
    loop = asyncio.get_running_loop()
    while True:
        try:
            data = sock.recv(1, socket.MSG_PEEK)
            return data[0] if data else None
        except BlockingIOError:
            ready = loop.create_future()
            loop.add_reader(sock.fileno(), lambda: ready.done() or ready.set_result(None))
            try:
                await ready
            finally:
                loop.remove_reader(sock.fileno())


def fmt_secs(secs):
    secs = int(secs)
    h, rest = divmod(secs, 3600)
    m, s = divmod(rest, 60)
    return f"{h}h{m:02d}m{s:02d}s" if h else f"{m}m{s:02d}s" if m else f"{s}s"


class Agent:
    """One boot of one machine: what its hellos said, and its connections."""

    def __init__(self, ident):
        self.ident = ident
        self.first_seen = time.monotonic()
        self.hello_at = time.monotonic()
        self.conns = set()

    def describe(self):
        d = dict(self.ident)
        uptime = int(d.pop("uptime_ms", "0") or 0) / 1000 + time.monotonic() - self.hello_at
        d["uptime_s"] = int(uptime)
        d["idle"] = sum(1 for c in self.conns if c.session is None)
        d["busy"] = len(self.conns) - d["idle"]
        return d


class AgentConn:
    def __init__(self, broker, agent, reader, writer):
        self.broker, self.agent = broker, agent
        self.reader, self.writer = reader, writer
        self.session = None
        self.last_rx = time.monotonic()

    def write(self, kind, payload=b""):
        if not self.writer.is_closing():
            self.writer.write(frame(kind, payload))

    def close(self):
        if not self.writer.is_closing():
            self.writer.close()

    def abort(self):
        self.writer.transport.abort()

    async def run(self):
        why = "the agent closed the connection"
        try:
            while True:
                kind, payload = await read_frame(self.reader)
                self.last_rx = time.monotonic()
                if kind == PING:
                    self.write(PONG, payload)
                elif kind == PONG:
                    pass
                elif self.session is None:
                    why = f"frame {kind:#04x} on an idle connection"
                    break
                else:
                    await self.session.to_client(kind, payload)
        except (asyncio.IncompleteReadError, ConnectionError, ssl.SSLError, OSError) as e:
            if not isinstance(e, asyncio.IncompleteReadError):
                why = f"connection error: {e}"
        except RemoteError as e:
            why = str(e)
        finally:
            if self.session is not None and self.session.finished:
                self.close()
            else:
                self.abort()
            await self.broker.drop(self)
            if self.session is not None:
                await self.session.agent_lost(why)


class Session:
    def __init__(self, conn, writer):
        self.conn, self.writer = conn, writer
        self.finished = False

    async def to_client(self, kind, payload):
        if self.finished:
            return
        self.writer.write(frame(kind, payload))
        await self.writer.drain()
        if kind in TERMINAL:
            self.finished = True
            self.conn.close()
            self.writer.close()

    async def agent_lost(self, why):
        if self.finished:
            return
        self.finished = True
        try:
            self.writer.write(frame(ERROR, encode_fields([("msg", f"lost the agent: {why}")])))
            await self.writer.drain()
        except (ConnectionError, OSError):
            pass
        self.writer.close()


class Broker:
    def __init__(self, state, port, offers):
        self.state, self.port, self.offers = state, port, offers
        self.token = state.token
        self.agents = {}
        self.changed = asyncio.Condition()
        self.complaints = {}
        self.tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.tls.minimum_version = ssl.TLSVersion.TLSv1_3
        self.tls.load_cert_chain(state.path / "leaf.pem", state.path / "leaf.key")
        self.tls.num_tickets = 0

    def complain(self, key, msg):
        """Log `msg` at most once a minute per `key`: a misconfigured agent redials every 30 s."""
        now = time.monotonic()
        if now - self.complaints.get(key, -1e9) >= 60:
            self.complaints[key] = now
            log(msg)

    async def notify(self):
        async with self.changed:
            self.changed.notify_all()

    async def drop(self, conn):
        conn.agent.conns.discard(conn)
        await self.notify()

    def live(self):
        return sorted((a for a in self.agents.values() if a.conns), key=lambda a: a.first_seen, reverse=True)

    def idle_conn(self, boot=None):
        for agent in self.live():
            if boot and agent.ident["boot"] != boot:
                continue
            for conn in agent.conns:
                if conn.session is None and not conn.writer.is_closing():
                    return conn
        return None

    async def until(self, predicate, timeout):
        """Wait for `predicate()` to hold, re-checked whenever an agent comes or goes."""
        deadline = time.monotonic() + timeout
        async with self.changed:
            while True:
                found = predicate()
                if found:
                    return found
                left = deadline - time.monotonic()
                if left <= 0:
                    return None
                try:
                    await asyncio.wait_for(self.changed.wait(), left)
                except TimeoutError:
                    pass

    # The listening port.

    async def on_tcp(self, sock, peer):
        try:
            first = await asyncio.wait_for(peek_byte(sock), 15)
        except (TimeoutError, OSError):
            sock.close()
            return
        if first is None:
            sock.close()
        elif first == 0x16:
            await self.on_agent(sock, peer)
        else:
            await self.on_http(sock, peer)

    async def on_agent(self, sock, peer):
        ip = peer[0]
        try:
            reader, writer = await accepted_streams(sock, self.tls)
        except (ssl.SSLError, OSError, TimeoutError) as e:
            why = f"TLS handshake with {ip} failed: {e or 'timed out'}"
            if "ALERT" in str(e).upper():
                why += " (a base paired with another broker? boot one paired with this one)"
            self.complain(("tls", ip), why)
            sock.close()
            return
        try:
            kind, payload = await asyncio.wait_for(read_frame(reader), 15)
            hello = Fields.decode(payload)
        except (TimeoutError, asyncio.IncompleteReadError, RemoteError, ConnectionError, ssl.SSLError, OSError):
            writer.transport.abort()
            return
        refusal = None
        if kind != HELLO:
            refusal = "the first frame must be HELLO"
        elif hello.text("proto") != PROTOCOL:
            refusal = f"protocol {hello.text('proto')!r}, this broker speaks {PROTOCOL}"
        elif not hmac.compare_digest(hello.get("token", b""), self.token.encode()):
            refusal = "wrong token: this machine's base is paired with another broker"
        elif len(hello.text("boot")) < 8:
            refusal = "no boot id"
        if refusal:
            self.complain(("hello", ip), f"refused the agent at {ip}: {refusal}")
            writer.write(frame(ERROR, encode_fields([("msg", refusal)])))
            try:
                await writer.drain()
            except (ConnectionError, OSError):
                pass
            writer.close()
            return
        ident = {k: hello.text(k) for k in ("host", "version", "tag", "base_tag", "boot", "uptime_ms", "pid")}
        ident["peer"] = ip
        agent = self.agents.get(ident["boot"])
        if agent is None:
            agent = self.agents[ident["boot"]] = Agent(ident)
            log(f"agent {ident['boot'][:12]} up: {ident['version']} at {ip}"
                + (f" (base {ident['base_tag']})" if ident["base_tag"] else ""))
        else:
            agent.ident, agent.hello_at = ident, time.monotonic()
        conn = AgentConn(self, agent, reader, writer)
        agent.conns.add(conn)
        writer.write(frame(WELCOME))
        await self.notify()
        await conn.run()

    async def on_http(self, sock, peer):
        try:
            reader, writer = await accepted_streams(sock)
        except OSError:
            sock.close()
            return
        try:
            head = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 15)
            parts = head.split(b"\r\n", 1)[0].decode("latin-1").split()
            await self.answer_http(writer, peer[0], parts)
        except (TimeoutError, asyncio.IncompleteReadError, asyncio.LimitOverrunError, ConnectionError, OSError):
            pass
        finally:
            writer.close()

    async def answer_http(self, writer, ip, parts):
        def head(status, length, kind="text/plain; charset=utf-8"):
            writer.write(f"HTTP/1.0 {status}\r\nContent-Type: {kind}\r\nContent-Length: {length}\r\n"
                         "Connection: close\r\n\r\n".encode())

        if len(parts) < 2 or parts[0] != "GET":
            body = b"slopos-remote: only GET\n"
            head("405 Method Not Allowed", len(body))
            writer.write(body)
            return
        target = parts[1]
        name = target.removeprefix("/files/")
        if target.startswith("/files/") and name in self.offers:
            path = self.offers[name]
            size = path.stat().st_size
            log(f"sending {name} ({path}, {size} bytes) to {ip}")
            head("200 OK", size, "application/octet-stream")
            with open(path, "rb") as f:
                while chunk := f.read(CHUNK):
                    writer.write(chunk)
                    await writer.drain()
            return
        body = b"slopos-remote: GET /files/<name>\n"
        head("404 Not Found", len(body))
        writer.write(body)

    # The CLI's socket.

    async def on_client(self, reader, writer):
        try:
            kind, payload = await asyncio.wait_for(read_frame(reader), 15)
            if kind != CONTROL:
                return
            req = json.loads(payload)
            op = req.get("op")
            if op == "status":
                await self.reply(writer, {"ok": True, "agents": [a.describe() for a in self.live()]})
            elif op == "wait":
                await self.op_wait(writer, req)
            elif op == "open":
                await self.op_open(reader, writer, req)
            else:
                await self.reply(writer, {"ok": False, "error": f"unknown op {op!r}"})
        except (TimeoutError, asyncio.IncompleteReadError, ConnectionError, OSError, ValueError, RemoteError):
            pass
        finally:
            writer.close()

    async def reply(self, writer, answer):
        writer.write(frame(ANSWER, json.dumps(answer).encode()))
        await writer.drain()

    def no_agent(self):
        return ("no SlopOS agent is connected: does the machine boot a base paired with this broker "
                f"(remote-serve's), and can it reach port {self.port}?")

    async def op_wait(self, writer, req):
        tag, boot, exclude = req.get("tag"), req.get("boot"), set(req.get("exclude_boots") or ())

        def match():
            for agent in self.live():
                if agent.ident["boot"] in exclude or (boot and agent.ident["boot"] != boot):
                    continue
                if tag is not None and agent.ident["tag"] != tag:
                    continue
                return agent
            return None

        agent = await self.until(match, float(req.get("timeout", 60)))
        if agent:
            await self.reply(writer, {"ok": True, "agent": agent.describe()})
            return
        seen = ", ".join(f"{a.ident['boot'][:12]} tag {a.ident['tag'] or '-'}" for a in self.live())
        await self.reply(writer, {"ok": False, "error": "timed out waiting for "
                                  + (f"an agent with build tag {tag!r}" if tag is not None else "an agent")
                                  + (" on a new boot" if exclude else "")
                                  + (f" with boot id {boot}" if boot else "")
                                  + (f"; connected: {seen}" if seen else "; none connected")})

    async def op_open(self, reader, writer, req):
        boot = req.get("boot")

        def claim():
            conn = self.idle_conn(boot)
            if conn:
                conn.session = Session(conn, writer)
            return conn

        conn = await self.until(claim, float(req.get("timeout", 15)))
        if conn is None:
            live = self.live()
            busy = sum(len(a.conns) for a in live)
            await self.reply(writer, {"ok": False, "error": self.no_agent() if not live else
                                      f"the agent has no idle connection ({busy} busy)"})
            return
        await self.reply(writer, {"ok": True, "agent": conn.agent.describe()})
        session = conn.session
        try:
            while not session.finished:
                kind, payload = await read_frame(reader)
                if kind in (HELLO, WELCOME, PING, PONG, CONTROL, ANSWER):
                    break
                conn.write(kind, payload)
                await conn.writer.drain()
        except (asyncio.IncompleteReadError, ConnectionError, OSError, RemoteError):
            pass
        if not session.finished:
            session.finished = True
            conn.abort()

    async def pinger(self):
        while True:
            await asyncio.sleep(PING_EVERY)
            now = time.monotonic()
            for agent in list(self.agents.values()):
                for conn in list(agent.conns):
                    if now - conn.last_rx > DEAD_AFTER:
                        log(f"agent {agent.ident['boot'][:12]} silent for {DEAD_AFTER} s; dropping a connection")
                        conn.abort()
                    else:
                        conn.write(PING)

    async def run(self, bind, sock_path):
        loop = asyncio.get_running_loop()
        listener = socket.create_server((bind, self.port), backlog=64)
        listener.setblocking(False)
        if sock_path.exists():
            probe = socket.socket(socket.AF_UNIX)
            try:
                probe.connect(str(sock_path))
                die(f"a broker is already running on {sock_path}")
            except (ConnectionRefusedError, FileNotFoundError):
                sock_path.unlink()
            finally:
                probe.close()
        sock_path.parent.mkdir(parents=True, exist_ok=True)
        old = os.umask(0o177)
        try:
            server = await asyncio.start_unix_server(self.on_client, path=str(sock_path))
        finally:
            os.umask(old)
        os.chmod(sock_path, 0o600)
        tasks = {asyncio.create_task(self.pinger())}
        try:
            async with server:
                while True:
                    sock, peer = await loop.sock_accept(listener)
                    task = asyncio.create_task(self.on_tcp(sock, peer))
                    tasks.add(task)
                    task.add_done_callback(tasks.discard)
        finally:
            listener.close()
            sock_path.unlink(missing_ok=True)


def lan_address():
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        probe.connect(("192.0.2.1", 9))
        return probe.getsockname()[0]
    except OSError:
        return "127.0.0.1"
    finally:
        probe.close()


def ufw_active():
    if shutil.which("systemctl"):
        if subprocess.run(["systemctl", "is-active", "--quiet", "ufw"]).returncode == 0:
            return True
    try:
        return "ENABLED=yes" in Path("/etc/ufw/ufw.conf").read_text()
    except OSError:
        return False


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


def cmd_serve(args):
    state = State(args.state).ensure()
    offers = {}
    for offer in args.offer:
        name, sep, path = offer.partition("=")
        if not sep or not name or "/" in name:
            die(f"--offer wants NAME=PATH, got {offer!r}")
        path = Path(path).resolve()
        if not path.is_file():
            die(f"--offer {name}: no file at {path}")
        offers[name] = path
    # What the last `provision` paired a base with, when it names this port.
    recorded_host, _, recorded_port = (state.broker or "").rpartition(":")
    host = args.advertise or (recorded_host if recorded_port == str(args.port) else lan_address())
    addr = f"{host}:{args.port}"
    paired = None
    if "base" in offers:
        try:
            paired = check_paired(state, offers["base"])
        except RemoteError as e:
            die(f"--offer base: {e}")
    broker = Broker(state, args.port, offers)
    log(f"broker on {args.bind}:{args.port} (agents over TLS, bootstrap over HTTP); CLI socket {args.socket}")
    if paired is not None and paired != addr:
        log(f"warning: the offered base dials {paired}, not {addr}")
    if offers:
        log("bootstrap files:")
        for name, path in offers.items():
            print(f"    {name:8} sha256 {sha256_file(path)}  {path}")
    if {"kernel", "base"} <= offers.keys():
        log(f"on the SlopOS machine, boot the offered system, whose base dials {paired}, from the spare slot:")
        print("    mkdir -p /var/tmp")
        for name in ("kernel", "base"):
            print(f"    curl -fsS -o /var/tmp/{name} http://{addr}/files/{name}")
        print("    sha256sum /var/tmp/kernel /var/tmp/base    # as above")
        print("    slot=$(bootctl spare)")
        print("    bootctl install $slot /var/tmp/kernel /var/tmp/base")
        print("    bootctl oneshot slopos-$slot")
        print("    bootctl reboot")
        log("init starts remoted there; once `just remote status` shows it, keep the slot with:")
        print("    just remote run -- bootctl commit")
    if args.bind not in ("127.0.0.1", "localhost") and ufw_active():
        lan = ".".join(host.split(".")[:3]) + ".0/24"
        log(f"ufw is active; open the port once: sudo ufw allow proto tcp from {lan} to any port {args.port}")
    try:
        asyncio.run(broker.run(args.bind, Path(args.socket)))
    except KeyboardInterrupt:
        pass


def cmd_provision(args):
    """The pairing a base carries: this broker's CA, its token and where it listens."""
    state = State(args.state).ensure()
    if args.broker:
        broker = args.broker
    elif args.port:
        broker = f"{lan_address()}:{args.port}"
    else:
        broker = state.broker or f"{lan_address()}:{DEFAULT_PORT}"
    host, sep, port = broker.rpartition(":")
    if not sep or not host or not port.isdigit() or not 0 < int(port) < 65536:
        die(f"provision: the broker must be HOST:PORT, got {broker!r}")
    dest = Path(args.dir)
    dest.mkdir(parents=True, exist_ok=True)
    os.chmod(dest, 0o700)
    write_private(dest / "ca.pem", state.ca_pem)
    write_private(dest / "token", (state.token + "\n").encode())
    write_private(dest / "remote.conf", (
        "# Written by `remote.py provision`; remoted dials the broker here.\n"
        f"broker = {broker}\nserver_name = {SERVER_NAME}\n").encode())
    write_private(state.path / "broker", (broker + "\n").encode())
    log(f"wrote a pairing with {broker} into {dest}")


# --- the CLI ----------------------------------------------------------------


class BrokerLink:
    """A blocking connection to the broker's socket."""

    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        try:
            self.sock.connect(str(path))
        except (FileNotFoundError, ConnectionRefusedError):
            raise RemoteError(f"no broker on {path}: start one with `just remote-serve` "
                              "(or scripts/remote.py serve)") from None
        self.lock = threading.Lock()

    def send(self, kind, payload=b""):
        with self.lock:
            self.sock.sendall(frame(kind, payload))

    def send_fields(self, kind, pairs):
        self.send(kind, encode_fields(pairs))

    def _exact(self, n):
        buf = bytearray()
        while len(buf) < n:
            got = self.sock.recv(min(n - len(buf), 1 << 20))
            if not got:
                raise RemoteError("the broker closed the connection")
            buf += got
        return bytes(buf)

    def recv(self):
        kind, length = HEADER.unpack(self._exact(HEADER.size))
        if length > MAX_PAYLOAD:
            raise RemoteError("the broker sent an oversized frame")
        return kind, self._exact(length) if length else b""

    def control(self, **req):
        self.send(CONTROL, json.dumps(req).encode())
        kind, payload = self.recv()
        if kind != ANSWER:
            raise RemoteError(f"the broker answered with frame {kind:#04x}")
        answer = json.loads(payload)
        if not answer.get("ok"):
            raise RemoteError(answer.get("error", "the broker refused"))
        return answer

    def close(self):
        self.sock.close()


def session(args):
    link = BrokerLink(args.socket)
    link.control(op="open", boot=args.boot, timeout=args.connect_timeout)
    return link


def expect_accept(link):
    kind, payload = link.recv()
    if kind == ERROR:
        raise RemoteError(error_text(payload))
    if kind != ACCEPT:
        raise RemoteError(f"expected ACCEPT, got frame {kind:#04x}")
    return Fields.decode(payload)


def execute(args, argv, out, err, cwd=None, env=(), timeout=None, stdin=None):
    """Run `argv` on the agent, streaming its output; the exit as (code, signal, timed_out)."""
    pairs = [("arg", a) for a in argv]
    if cwd:
        pairs.append(("cwd", cwd))
    pairs += [("env", e) for e in env]
    if timeout:
        pairs.append(("timeout_ms", str(int(timeout * 1000))))
    if stdin is not None:
        pairs.append(("stdin", "1"))
    link = session(args)
    try:
        link.send_fields(EXEC, pairs)
        expect_accept(link)
        if stdin is not None:
            def feed():
                try:
                    while chunk := stdin.read1(CHUNK) if hasattr(stdin, "read1") else stdin.read(CHUNK):
                        link.send(DATA, chunk)
                    link.send(EOF)
                except OSError:
                    pass
            threading.Thread(target=feed, daemon=True).start()
        while True:
            kind, payload = link.recv()
            if kind == STDOUT:
                out.write(payload)
                out.flush()
            elif kind == STDERR:
                err.write(payload)
                err.flush()
            elif kind == EXIT:
                f = Fields.decode(payload)
                code = f.text("code")
                signal = f.text("signal")
                return (int(code) if code else None, int(signal) if signal else None,
                        f.text("timed_out") == "1")
            elif kind == ERROR:
                raise RemoteError(error_text(payload))
            else:
                raise RemoteError(f"unexpected frame {kind:#04x} from the agent")
    finally:
        link.close()


def exit_status(code, signal, timed_out, timeout):
    if timed_out:
        print(f"remote: timed out after {timeout:g} s; killed", file=sys.stderr)
        return 124
    if signal is not None:
        print(f"remote: killed by signal {signal}", file=sys.stderr)
        return 128 + signal
    return code if code is not None else 1


def run_remote(args, argv, timeout=None, cwd=None, env=(), stdin=None):
    return exit_status(*execute(args, argv, sys.stdout.buffer, sys.stderr.buffer,
                                cwd=cwd, env=env, timeout=timeout, stdin=stdin), timeout)


def capture(args, argv, timeout=120):
    out, err = io.BytesIO(), io.BytesIO()
    code, signal, timed_out = execute(args, argv, out, err, timeout=timeout)
    if code != 0 or signal is not None or timed_out:
        why = "timed out" if timed_out else f"signal {signal}" if signal is not None else f"exit {code}"
        detail = (err.getvalue() or out.getvalue()).decode(errors="replace").strip()
        raise RemoteError(f"`{' '.join(argv)}` failed ({why}){': ' + detail if detail else ''}")
    return out.getvalue().decode(errors="replace")


def cmd_run(args):
    argv = args.argv[1:] if args.argv[:1] == ["--"] else args.argv
    if not argv:
        die("run: name a program: run -- ARGV...", 2)
    stdin = sys.stdin.buffer if args.stdin else None
    return run_remote(args, argv, timeout=args.timeout, cwd=args.cwd, env=args.env, stdin=stdin)


def cmd_sh(args):
    return run_remote(args, ["/bin/shell", "-c", args.cmdline], timeout=args.timeout)


def drain_error(link):
    """After a failed send: what the agent said before it closed, if anything."""
    try:
        while True:
            kind, payload = link.recv()
            if kind == ERROR:
                return error_text(payload)
    except (RemoteError, OSError):
        return None


def push(args, local, remote):
    local = Path(local)
    if remote.endswith("/"):
        remote += local.name
    mode = stat.S_IMODE(local.stat().st_mode)
    digest, size = hashlib.sha256(), 0
    link = session(args)
    try:
        link.send_fields(PUT, [("path", remote), ("mode", f"{mode:o}")])
        expect_accept(link)
        try:
            with open(local, "rb") as f:
                while chunk := f.read(CHUNK):
                    link.send(DATA, chunk)
                    digest.update(chunk)
                    size += len(chunk)
            link.send(EOF)
        except OSError as e:
            raise RemoteError(drain_error(link) or f"sending {local}: {e}") from None
        kind, payload = link.recv()
        if kind == ERROR:
            raise RemoteError(error_text(payload))
        if kind != DONE:
            raise RemoteError(f"expected DONE, got frame {kind:#04x}")
    finally:
        link.close()
    done = Fields.decode(payload)
    if done.text("sha256") != digest.hexdigest() or done.text("size") != str(size):
        raise RemoteError(f"{remote}: the agent wrote {done.text('size')} bytes with sha256 "
                          f"{done.text('sha256')}, this host sent {size} with {digest.hexdigest()}")
    return remote, size, digest.hexdigest()


def cmd_push(args):
    remote, size, sha = push(args, args.local, args.remote)
    log(f"pushed {args.local} to {remote}: {size} bytes, sha256 {sha}")


def cmd_pull(args):
    local = Path(args.local)
    if local.is_dir():
        local = local / Path(args.remote).name
    digest, size = hashlib.sha256(), 0
    fd, tmp = tempfile.mkstemp(dir=local.parent or ".", prefix=f".{local.name}.")
    link = session(args)
    try:
        link.send_fields(GET, [("path", args.remote)])
        expect_accept(link)
        with os.fdopen(fd, "wb") as f:
            while True:
                kind, payload = link.recv()
                if kind == DATA:
                    f.write(payload)
                    digest.update(payload)
                    size += len(payload)
                elif kind == EOF:
                    break
                elif kind == ERROR:
                    raise RemoteError(error_text(payload))
                else:
                    raise RemoteError(f"unexpected frame {kind:#04x} in a GET")
        kind, payload = link.recv()
        if kind == ERROR:
            raise RemoteError(error_text(payload))
        done = Fields.decode(payload)
        if kind != DONE or done.text("sha256") != digest.hexdigest() or done.text("size") != str(size):
            raise RemoteError(f"{args.remote}: the agent read {done.text('size')} bytes with sha256 "
                              f"{done.text('sha256')}, this host got {size} with {digest.hexdigest()}")
        os.replace(tmp, local)
        tmp = None
    finally:
        link.close()
        if tmp:
            Path(tmp).unlink(missing_ok=True)
    log(f"pulled {args.remote} to {local}: {size} bytes, sha256 {digest.hexdigest()}")


def new_text(previous, current):
    """What `current` adds past `previous`, two reads of a ring that drops old lines."""
    if not previous:
        return current
    anchor = previous[-512:]
    at = current.rfind(anchor)
    if at < 0:
        return b"[remote: the log moved on more than a read holds; lines may be missing]\n" + current
    return current[at + len(anchor):]


def cmd_klog(args):
    if not args.follow:
        return run_remote(args, ["cat", "/dev/kmsg"], timeout=60)
    previous = b""
    while True:
        out, err = io.BytesIO(), io.BytesIO()
        code, _, _ = execute(args, ["cat", "/dev/kmsg"], out, err, timeout=60)
        if code != 0:
            sys.stderr.buffer.write(err.getvalue())
            return code or 1
        current = out.getvalue()
        sys.stdout.buffer.write(new_text(previous, current))
        sys.stdout.flush()
        previous = current
        time.sleep(1)


def describe(agent):
    tag = agent.get("tag") or "-"
    base = agent.get("base_tag") or "-"
    return (f"boot {agent['boot']}  peer {agent['peer']}  up {fmt_secs(agent['uptime_s'])}  "
            f"conns {agent['idle']} idle/{agent['busy']} busy\n"
            f"  {agent['version']}  (kernel tag {tag}, base tag {base})")


def cmd_status(args):
    agents = BrokerLink(args.socket).control(op="status")["agents"]
    if args.json:
        print(json.dumps(agents, indent=2))
    elif not agents:
        print("no agent connected")
    else:
        print("\n".join(describe(a) for a in agents))
    return 0 if agents else 1


def wait_agent(args, tag=None, exclude_boots=(), timeout=60):
    """The agent the broker reports matching; a broker still starting up is waited for too."""
    deadline = time.monotonic() + timeout
    while True:
        try:
            link = BrokerLink(args.socket)
            break
        except RemoteError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.5)
    try:
        return link.control(op="wait", tag=tag, boot=args.boot, exclude_boots=list(exclude_boots),
                            timeout=max(deadline - time.monotonic(), 0))["agent"]
    finally:
        link.close()


def cmd_wait(args):
    agent = wait_agent(args, tag=args.tag, timeout=args.timeout)
    print(json.dumps(agent, indent=2) if args.json else describe(agent))


def cmd_install(args):
    kernel, base = Path(args.kernel), Path(args.base)
    for path in (kernel, base):
        if not path.is_file():
            die(f"install: no file at {path}")
    # A base without this broker's pairing boots a machine nothing can reach
    # to commit it, or to do anything else.
    paired = check_paired(State(args.state), base)
    log(f"{base} is paired with {paired}")
    before = wait_agent(args, timeout=args.connect_timeout)
    args.boot = before["boot"]
    link = BrokerLink(args.socket)
    try:
        old_boots = [a["boot"] for a in link.control(op="status")["agents"]]
    finally:
        link.close()
    log(f"installing onto boot {before['boot'][:12]} ({before['version']})")
    capture(args, ["mkdir", "-p", SCRATCH])
    remote_kernel, size, sha = push(args, kernel, f"{SCRATCH}/kernel.elf")
    log(f"pushed {kernel}: {size} bytes, sha256 {sha}")
    remote_base, size, sha = push(args, base, f"{SCRATCH}/base.img")
    log(f"pushed {base}: {size} bytes, sha256 {sha}")
    slot = capture(args, ["bootctl", "spare"]).strip()
    if not slot:
        die("install: bootctl spare named no slot")
    out = capture(args, ["bootctl", "install", slot, remote_kernel, remote_base], timeout=600)
    sys.stdout.write(out)
    capture(args, ["rm", "-f", remote_kernel, remote_base])
    capture(args, ["bootctl", "oneshot", f"slopos-{slot}"])
    log(f"slot {slot} installed; rebooting into slopos-{slot} once")
    try:
        execute(args, ["bootctl", "reboot"], io.BytesIO(), io.BytesIO(), timeout=30)
    except RemoteError:
        pass
    args.boot = None
    after = wait_agent(args, exclude_boots=old_boots, timeout=args.timeout)
    args.boot = after["boot"]
    log(f"back as boot {after['boot'][:12]}: {after['version']}")
    if args.tag is not None and after["tag"] != args.tag:
        print(capture(args, ["bootctl", "status"]), end="")
        die(f"install: the machine came back with kernel tag {after['tag'] or '-'!r}, not {args.tag!r}: "
            f"slopos-{slot} did not boot (it fell back) and nothing was committed")
    if args.commit:
        capture(args, ["bootctl", "commit"])
        log(f"committed slopos-{slot}: it boots by default now")
    else:
        log(f"not committed: the next reboot goes back to the default; `remote.py run -- bootctl commit` keeps slopos-{slot}")
    print(capture(args, ["bootctl", "status"]), end="")


def main():
    parser = argparse.ArgumentParser(prog="remote.py", description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--state", type=Path, default=None, help="state directory (default: $XDG_CONFIG_HOME/slopos-remote)")
    parser.add_argument("--socket", type=Path, default=None, help="the broker's unix socket")
    parser.add_argument("--connect-timeout", type=float, default=15, help="seconds to wait for an idle agent connection")
    parser.add_argument("--boot", help="the agent to use, by boot id (default: the newest boot)")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("serve", help="run the broker")
    p.add_argument("--port", type=int, default=DEFAULT_PORT)
    p.add_argument("--bind", default="0.0.0.0")
    p.add_argument("--advertise", help="the address the SlopOS machine dials (default: this host's LAN address)")
    p.add_argument("--offer", action="append", default=[], metavar="NAME=PATH", help="serve PATH at /files/NAME")
    p.set_defaults(func=cmd_serve)

    p = sub.add_parser("provision", help="write a pairing (remote.conf, ca.pem, token) for a base into DIR")
    p.add_argument("dir")
    p.add_argument("--broker", metavar="HOST:PORT",
                   help="where the machine dials (default: the last provision's, else this host's LAN address:7330)")
    p.add_argument("--port", type=int, help="dial this host's LAN address at PORT")
    p.set_defaults(func=cmd_provision)

    p = sub.add_parser("status", help="the connected agents")
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=cmd_status)

    p = sub.add_parser("wait", help="wait for an agent, optionally one with build tag T")
    p.add_argument("--tag")
    p.add_argument("--timeout", type=float, default=300)
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=cmd_wait)

    p = sub.add_parser("run", help="run ARGV on the agent; its exit code is this one's")
    p.add_argument("--timeout", type=float)
    p.add_argument("--cwd")
    p.add_argument("--env", action="append", default=[], metavar="NAME=VALUE")
    p.add_argument("--stdin", action="store_true", help="send this process's stdin")
    p.add_argument("argv", nargs=argparse.REMAINDER)
    p.set_defaults(func=cmd_run)

    p = sub.add_parser("sh", help="run a command line through /bin/shell -c")
    p.add_argument("--timeout", type=float)
    p.add_argument("cmdline")
    p.set_defaults(func=cmd_sh)

    p = sub.add_parser("push", help="copy a local file to an absolute path on the agent")
    p.add_argument("local")
    p.add_argument("remote")
    p.set_defaults(func=cmd_push)

    p = sub.add_parser("pull", help="copy a file off the agent")
    p.add_argument("remote")
    p.add_argument("local")
    p.set_defaults(func=cmd_pull)

    p = sub.add_parser("klog", help="the kernel log (/dev/kmsg)")
    p.add_argument("--follow", "-f", action="store_true")
    p.set_defaults(func=cmd_klog)

    p = sub.add_parser("install", help="install a kernel and base into the spare slot and boot it")
    p.add_argument("--kernel", required=True)
    p.add_argument("--base", required=True)
    p.add_argument("--tag", help="the SLOPOS_BUILD_TAG the new kernel reports")
    p.add_argument("--commit", action="store_true", help="make the new slot the default once it is up")
    p.add_argument("--timeout", type=float, default=600, help="seconds to wait for the new boot")
    p.set_defaults(func=cmd_install)

    args = parser.parse_args()
    args.state = args.state or default_state()
    args.socket = args.socket or default_socket(args.state)
    try:
        sys.exit(args.func(args) or 0)
    except RemoteError as e:
        die(str(e))
    except KeyboardInterrupt:
        sys.exit(130)
    except BrokenPipeError:
        sys.exit(141)


if __name__ == "__main__":
    main()

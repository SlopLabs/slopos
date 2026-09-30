#!/usr/bin/env python3
"""Copy a host directory tree into an ext2 image through one debugfs session.

`mkfs.ext2 -d` populates only a new filesystem; a preserved image holds what
its guest wrote, so a tree reaches one through debugfs, which writes
everything as uid 0.

A host tree (`install --manifests DIR --name NAME`) is the host's. What it
installed is recorded in DIR/NAME, on the host, where the guest cannot edit
it — an `identity` line, then `<kind> <size or -> <path>` per entry — and a reinstall removes exactly that and keeps whatever the guest put
beside it; a copy goes to /var/lib/slopos/trees/NAME for the guest to read. A
seed tree (no `--name`) is copied onto an image that lacks its directory and
is the guest's from then on.

Nothing on the image is trusted. It is read only through `ls -l` listings of
directories, each bound to the inode its parent's listing gave it, so no
symlink or crafted name redirects a request; and anything the guest left
where the tree goes that the manifest does not name, or that is not the kind
the manifest names, refuses the install before anything is written.

    identity HOST
    installed MANIFESTS NAME
    exists IMAGE PATH
    install IMAGE HOST GUEST [--manifests DIR --name NAME]
    --self-test

An install refused before anything was written exits 3, any other failure 1.
"""

import contextlib
import hashlib
import io
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile

IMAGE_MANIFESTS = "/var/lib/slopos/trees"
ROOT_INODE = 2
# What debugfs's request parser can carry inside double quotes.
COMPONENT = re.compile(r'[^/"\\\n\r\t\x00]+')
TARGET = re.compile(r'[^"\\\n\r\x00]+')
IDENTITY = re.compile(r"[0-9a-f]{64}")
# Recorded while an install is under way, so an interrupted one reinstalls.
UNFINISHED = "0" * 64
# An `ls -l` entry: inode, mode, links, owner, size, date, time, and the name
# with every control byte and backslash written `\xNN`, so one line each.
LISTED = re.compile(rb"\s*(\d+)\s+([0-7]+)\s+\(\s*\d+\)\s+\d+\s+\d+\s+\d+\s+\S+ \S+ (.+)")
KINDS = {0o040000: "d", 0o100000: "f", 0o120000: "l"}
NAMES = {"d": "directory", "f": "file", "l": "symlink", "o": "special file"}


class Refused(Exception):
    pass


class Conflict(Refused):
    pass


def safe_rel(rel):
    parts = rel.split("/")
    return all(COMPONENT.fullmatch(p) and p not in (".", "..") for p in parts)


def safe_abs(path):
    return path == "/" or (path.startswith("/") and safe_rel(path[1:]))


def join(parent, name):
    return "/" + name if parent == "/" else parent + "/" + name


def parent_of(path):
    head = path.rsplit("/", 1)[0]
    return head or "/"


def ancestors(path):
    """`path`'s ancestors, `/` first, `path` excluded."""
    out, cur = [], path
    while cur != "/":
        cur = parent_of(cur)
        out.append(cur)
    return out[::-1]


def walk(host):
    """(kind, mode, rel, target, lstat) for everything below `host`, parents
    first."""
    out = []

    def visit(dir_abs, dir_rel):
        with os.scandir(dir_abs) as it:
            names = sorted(e.name for e in it)
        for name in names:
            rel = name if not dir_rel else dir_rel + "/" + name
            full = os.path.join(dir_abs, name)
            st = os.lstat(full)
            kind = KINDS.get(stat.S_IFMT(st.st_mode))
            if kind is None:
                raise Refused(f"{full} is neither a file, a directory nor a symlink")
            if not COMPONENT.fullmatch(name):
                raise Refused(f"{full}: a name debugfs cannot take")
            target = os.readlink(full) if kind == "l" else ""
            if kind == "l" and not TARGET.fullmatch(target):
                raise Refused(f"{full}: a link target debugfs cannot take")
            out.append((kind, stat.S_IMODE(st.st_mode), rel, target, st))
            if kind == "d":
                visit(full, rel)

    visit(host, "")
    return out


def identity(host):
    digest = hashlib.sha256()
    root = os.lstat(host)
    digest.update(f"d {root.st_mode:o} {root.st_mtime_ns} .\n".encode())
    for kind, mode, rel, target, st in walk(host):
        size = st.st_size if kind == "f" else 0
        digest.update(f"{kind} {mode:o} {size} {st.st_mtime_ns} {rel}\t{target}\n".encode())
    return digest.hexdigest()


def debugfs(image, requests, write=False):
    """Run `requests` in one debugfs session: its stdout, and every line it
    wrote to stderr but its banner."""
    fd, path = tempfile.mkstemp(prefix="fs_tree.")
    try:
        with os.fdopen(fd, "w") as f:
            f.write("".join(r + "\n" for r in requests))
        cmd = ["debugfs"] + (["-w"] if write else []) + ["-f", path, image]
        run = subprocess.run(cmd, capture_output=True)
    finally:
        os.unlink(path)
    said = [
        line
        for line in run.stderr.decode(errors="replace").splitlines()
        if line.strip() and not re.match(r"debugfs [0-9][0-9.]* \(", line)
    ]
    return run.stdout, said


class Image:
    """What the image holds at the paths an install is about, read through
    directories bound to their inodes."""

    def __init__(self, image, dirs):
        self.entries = {"/": ("d", ROOT_INODE)}
        self.listed = {}
        want = set(dirs)
        for d in dirs:
            want.update(ancestors(d))
        frontier = ["/"]
        while frontier:
            out, _ = debugfs(image, [f'ls -l "{d}"' for d in frontier])
            parsed = self._parse(out, frontier)
            nxt = []
            for d in frontier:
                children = parsed[d]
                if children.get(".", (None, None))[1] != self.entries[d][1]:
                    raise Conflict(f"{d} did not list as the directory its parent names")
                children = {n: v for n, v in children.items() if n not in (".", "..")}
                self.listed[d] = set(children)
                for name, (kind, ino) in children.items():
                    path = join(d, name)
                    self.entries[path] = (kind, ino)
                    if kind == "d" and path in want:
                        nxt.append(path)
            frontier = nxt

    @staticmethod
    def _parse(out, dirs):
        """Each of `dirs`' entries: every line is one, or the echo of the next
        request."""
        parsed = {d: {} for d in dirs}
        echoes = iter(dirs)
        pending, cur = next(echoes), None
        for line in out.split(b"\n"):
            if pending is not None and line == f'debugfs: ls -l "{pending}"'.encode():
                cur, pending = pending, next(echoes, None)
                continue
            if not line:
                continue
            m = LISTED.fullmatch(line)
            if cur is None or not m or b"/" in m.group(3):
                raise Conflict(f"{cur or dirs[0]} holds a name this tool cannot read")
            name = m.group(3).decode(errors="surrogateescape")
            kind = KINDS.get(int(m.group(2), 8) & 0o170000, "o")
            if name in parsed[cur]:
                raise Conflict(f"{cur} lists {name!r} twice")
            parsed[cur][name] = (kind, int(m.group(1)))
        return parsed

    def state(self, path):
        """`absent`, a kind, or `blocked` when an ancestor is not a directory."""
        if path == "/":
            return "d"
        parent = self.state(parent_of(path))
        if parent == "absent":
            return "absent"
        if parent != "d":
            return "blocked"
        if parent_of(path) not in self.listed:
            raise Refused(f"{parent_of(path)} was never listed")
        return self.entries.get(path, ("absent",))[0]


def read_manifest(path):
    """The identity and `(kind, rel)` entries a host manifest records."""
    try:
        with open(path) as f:
            lines = f.read().splitlines()
    except FileNotFoundError:
        return None, []
    head = lines[0].split(" ", 1) if lines else []
    if len(head) != 2 or head[0] != "identity" or not IDENTITY.fullmatch(head[1]):
        raise Refused(f"{path} is not a manifest")
    out = []
    for line in lines[1:]:
        kind, size, rel = (line.split(" ", 2) + ["", ""])[:3]
        if kind not in ("d", "f", "l") or not (size == "-" or size.isdigit()) or not safe_rel(rel):
            raise Refused(f"{path}: {line!r} is not a manifest entry")
        out.append((kind, rel))
    return head[1], out


def installed(manifests, name):
    try:
        return read_manifest(os.path.join(manifests, name))[0] or ""
    except Refused:
        return ""


def exists(image, path):
    """Whether `path` is there; a listing this tool cannot read answers yes,
    since the answer decides whether to copy a tree over it."""
    if not safe_abs(path):
        raise Refused(f"{path}: not a path this tool reads")
    try:
        return Image(image, [parent_of(path)]).state(path) != "absent"
    except Conflict as e:
        print(f"fs_tree: {e}", file=sys.stderr)
        return True


def plan(image, host, guest, name):
    """The requests that install `host` at `guest`, and the manifest they
    leave, or `Refused` naming every conflict."""
    host = os.path.abspath(host)
    if not safe_abs(host) or not safe_abs(guest) or guest == "/":
        raise Refused(f"cannot install {host} at {guest}")
    tree = walk(host)
    manifest_path = os.path.join(name[0], name[1]) if name else None
    known, old = read_manifest(manifest_path) if manifest_path else (None, [])
    copy = join(IMAGE_MANIFESTS, name[1]) if name else None

    dirs = {guest} | {join(guest, rel) for kind, rel in old if kind == "d"}
    dirs |= {join(guest, rel) for kind, _, rel, _, _ in tree if kind == "d"}
    if copy:
        dirs.add(IMAGE_MANIFESTS)
    img = Image(image, sorted(dirs))

    conflicts = {}

    def conflict(path, why):
        conflicts.setdefault(path, f"{path} {why}")

    def claim_dir(path, what):
        state = img.state(path)
        if state not in ("absent", "d", "blocked"):
            conflict(path, f"is a {NAMES[state]}, where {what} needs a directory")
        return state

    creates = []
    for a in ancestors(guest) + [guest]:
        if a != "/" and claim_dir(a, host) == "absent":
            creates.append(f'mkdir "{a}"')

    removed = set()
    for kind, rel in old:
        path = join(guest, rel)
        state = img.state(path)
        if state in ("absent", "blocked"):
            continue
        if state != kind:
            conflict(path, f"is a {NAMES[state]}; {manifest_path} installed a {NAMES[kind]} there")
        elif kind != "d":
            removed.add(path)
    removes = [f'rm "{p}"' for p in sorted(removed)]
    kept = {join(guest, rel) for kind, _, rel, _, _ in tree if kind == "d"}
    for kind, rel in sorted(old, key=lambda e: e[1].count("/"), reverse=True):
        path = join(guest, rel)
        if kind == "d" and path not in kept and img.state(path) == "d":
            if all(join(path, c) in removed for c in img.listed.get(path, ())):
                removed.add(path)
                removes.append(f'rmdir "{path}"')

    def present(path):
        return img.state(path) not in ("absent", "blocked") and path not in removed

    for kind, mode, rel, target, _ in tree:
        path = join(guest, rel)
        if kind == "d":
            if present(path):
                if img.state(path) != "d":
                    conflict(path, f"is a {NAMES[img.state(path)]}, where {host} has a directory")
            else:
                creates.append(f'mkdir "{path}"')
            creates.append(f'sif "{path}" mode 0{0o040000 | mode:o}')
        elif present(path):
            conflict(path, f"is already there, and {host} installs one")
        elif kind == "f":
            creates.append(f'write "{os.path.join(host, rel)}" "{path}"')
            creates.append(f'sif "{path}" mode 0{0o100000 | mode:o}')
        else:
            creates.append(f'symlink "{path}" "{target}"')

    entries = [f"{k} {st.st_size if k == 'f' else '-'} {rel}" for k, _, rel, _, st in tree]
    lines = [f"identity {identity(host)}"] + entries
    during = [f"identity {UNFINISHED}"] + sorted(set(entries) | {f"{k} - {r}" for k, r in old})
    if copy:
        for a in ancestors(copy):
            if a != "/" and claim_dir(a, copy) == "absent":
                creates.append(f'mkdir "{a}"')
        state = img.state(copy)
        if state in ("f", "l"):
            creates.append(f'rm "{copy}"')
        elif state not in ("absent", "blocked"):
            conflict(copy, f"is a {NAMES[state]}, where the manifest copy goes")
    if conflicts:
        unrecorded = ""
        if name and known is None:
            unrecorded = f"No manifest at {manifest_path} records an earlier install. "
        raise Conflict(
            "the image holds what the install would overwrite:\n  "
            + "\n  ".join(list(conflicts.values())[:10])
            + ("\n  ..." if len(conflicts) > 10 else "")
            + f"\n{unrecorded}Move it aside in the guest, or discard the root with `just reset root`."
        )
    return removes + creates, (during, lines), copy, manifest_path


def write_manifest(path, lines):
    with open(path + ".part", "w") as f:
        f.write("".join(line + "\n" for line in lines))
    os.replace(path + ".part", path)


def install(image, host, guest, name=None):
    requests, (during, lines), copy, manifest_path = plan(image, host, guest, name)
    work = tempfile.mkdtemp(prefix="fs_tree.")
    try:
        if manifest_path:
            os.makedirs(name[0], exist_ok=True)
            write_manifest(manifest_path, during)
            listing = os.path.join(work, "manifest")
            write_manifest(listing, lines)
            requests.append(f'write "{listing}" "{copy}"')
        _, said = debugfs(image, requests, write=True)
    finally:
        shutil.rmtree(work)
    if said:
        raise Refused(f"debugfs refused requests for {image}:\n" + "\n".join(said[:5]))
    if manifest_path:
        write_manifest(manifest_path, lines)


def self_test():
    for tool in ("mkfs.ext2", "debugfs", "e2fsck"):
        if not shutil.which(tool):
            raise SystemExit(f"fs_tree: --self-test needs {tool}")
    work = tempfile.mkdtemp(prefix="fs_tree-self-test.")
    try:
        _self_test(work)
    finally:
        shutil.rmtree(work)
    print("fs_tree: --self-test OK")


def _self_test(work):
    image = os.path.join(work, "root.img")
    manifests = os.path.join(work, "trees")
    tree = os.path.join(work, "tree")
    name = (manifests, "usr_local")
    with open(image, "wb") as f:
        f.truncate(16 << 20)
    subprocess.run(["mkfs.ext2", "-q", "-F", "-b", "4096", image], check=True)

    def put(rel, text="x\n", mode=0o644):
        path = os.path.join(tree, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(text)
        os.chmod(path, mode)

    def guest(*requests):
        _, said = debugfs(image, list(requests), write=True)
        assert not said, said

    def kind_in(img_, path):
        return Image(img_, [parent_of(path)]).state(path)

    def kind(path):
        return kind_in(image, path)

    def read(path):
        out, _ = debugfs(image, [f'cat "{path}"'])
        return out.split(b"\n", 1)[1] if out.startswith(b"debugfs: ") else out

    def refused(what, target="/usr/local", manifest=name):
        try:
            install(image, tree, target, manifest)
        except Refused:
            return
        raise AssertionError(f"not refused: {what}")

    def sound():
        run = subprocess.run(["e2fsck", "-fn", image], capture_output=True)
        assert run.returncode == 0, run.stdout.decode()

    put("bin/tool", "one\n", 0o755)
    put("lib/libx.so", "lib\n")
    os.symlink("tool", os.path.join(tree, "bin/alias"))
    install(image, tree, "/usr/local", name)
    assert kind("/usr/local/bin/tool") == "f" and kind("/usr/local/bin/alias") == "l"
    assert read(f"{IMAGE_MANIFESTS}/usr_local").startswith(b"identity ")
    assert installed(manifests, "usr_local") == identity(tree)
    sound()

    guest("write /dev/null /usr/local/bin/guest", "mkdir /usr/local/lib/guest")
    put("bin/tool", "two\n", 0o755)
    os.unlink(os.path.join(tree, "lib/libx.so"))
    install(image, tree, "/usr/local", name)
    assert read("/usr/local/bin/tool") == b"two\n"
    assert kind("/usr/local/lib/libx.so") == "absent"
    assert kind("/usr/local/bin/guest") == "f" and kind("/usr/local/lib/guest") == "d"
    sound()

    # A listing that lies is refused, whatever the lie.
    module = sys.modules[__name__]
    honest = module.debugfs
    local = rb"^(\s*)(\d+)(\s+40755 .* local)$"
    for what, lie in (
        ("a directory listed under another inode", rb"\g<1>999\3"),
        ("a name listed twice", rb"\1\2\3\n\1\2\3"),
    ):

        def lying(img_, requests, write=False, lie=lie):
            out, said = honest(img_, requests, write)
            return re.sub(local, lie, out, flags=re.M), said

        module.debugfs = lying
        try:
            refused(what)
        finally:
            module.debugfs = honest

    guest("write /dev/null /usr/local/bin/new")
    put("bin/new")
    refused("a guest file where the tree adds one")
    assert read("/usr/local/bin/tool") == b"two\n"
    os.unlink(os.path.join(tree, "bin/new"))

    guest("mkdir /home", "mkdir /home/keep", "write /dev/null /home/keep/tool")
    guest(*(f"rm /usr/local/bin/{n}" for n in ("alias", "tool", "guest", "new")))
    guest("rmdir /usr/local/bin", "symlink /usr/local/bin /home/keep")
    refused("a host directory the guest made a symlink")
    assert kind("/home/keep/tool") == "f"
    guest("rm /usr/local/bin", "mkdir /usr/local/bin")
    install(image, tree, "/usr/local", name)
    assert read("/usr/local/bin/tool") == b"two\n"

    guest("rm /usr/local/bin/tool", "mkdir /usr/local/bin/tool")
    refused("a host file the guest replaced with a directory")
    guest("rmdir /usr/local/bin/tool")
    sound()

    guest("mkdir /sbin", "write /dev/null /sbin/init")
    with open(os.path.join(manifests, "usr_local"), "a") as f:
        f.write("f ../../sbin/init\n")
    refused("a manifest entry that climbs out of the tree")
    assert kind("/sbin/init") == "f"
    os.unlink(os.path.join(manifests, "usr_local"))
    refused("files no manifest records")

    guest("mkdir /srv", "symlink /srv/tree /sbin")
    refused("an install through a symlinked target", "/srv/tree", None)
    assert kind("/sbin/init") == "f"

    # A name holding a newline lists escaped and hides nothing; one holding
    # a `/`, which only an edit behind the filesystem's back makes, is refused.
    stage = os.path.join(work, "stage")
    os.makedirs(os.path.join(stage, "src"))
    for crafted in ('a\ndebugfs: ls -l "q"\nb', "a\nb"):
        with open(os.path.join(stage, crafted), "w"):
            pass
    other = os.path.join(work, "crafted.img")
    with open(other, "wb") as f:
        f.truncate(8 << 20)
    subprocess.run(["mkfs.ext2", "-q", "-F", "-b", "4096", "-d", stage, other], check=True)
    assert exists(other, "/src") and not exists(other, "/nowhere")
    install(other, tree, "/usr/local", None)
    assert kind_in(other, "/usr/local/bin/tool") == "f"
    debugfs(other, ["mknod /slashed p"], write=True)
    with contextlib.redirect_stderr(io.StringIO()):
        assert exists(other, "/nowhere"), "a name holding a / passed"
    try:
        install(other, tree, "/srv", None)
    except Conflict:
        pass
    else:
        raise AssertionError("not refused: a listing holding a name with a /")

    seed = os.path.join(work, "seed")
    os.makedirs(os.path.join(seed, "clone"))
    with open(os.path.join(seed, "clone/README"), "w") as f:
        f.write("seed\n")
    install(image, seed, "/src")
    assert read("/src/clone/README") == b"seed\n" and exists(image, "/src")
    assert not exists(image, "/nowhere")
    sound()


def main(argv):
    try:
        if argv == ["--self-test"]:
            self_test()
        elif len(argv) == 2 and argv[0] == "identity":
            print(identity(argv[1]))
        elif len(argv) == 3 and argv[0] == "installed":
            print(installed(argv[1], argv[2]))
        elif len(argv) == 3 and argv[0] == "exists":
            return 0 if exists(argv[1], argv[2]) else 1
        elif len(argv) == 4 and argv[0] == "install":
            install(*argv[1:])
        elif len(argv) == 8 and argv[0] == "install" and argv[4:7:2] == ["--manifests", "--name"]:
            if not COMPONENT.fullmatch(argv[7]):
                raise Refused(f"{argv[7]}: not a manifest name")
            install(argv[1], argv[2], argv[3], (argv[5], argv[7]))
        else:
            print(__doc__.split("\n\n")[-1], file=sys.stderr)
            return 2
    except Refused as e:
        print(f"fs_tree: {e}", file=sys.stderr)
        return 3 if isinstance(e, Conflict) else 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

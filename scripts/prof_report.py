#!/usr/bin/env python3
"""Summarize a prof=on boot log: where a self-hosting build's time went.

Usage: prof_report.py LOG [--libc LIBC_SO] [--lib-dir DIR ...] [--top N]

Kernel ticks arrive symbolized by the kernel and are folded by function. User
ticks arrive as raw 64-byte lines; the `PROF[..]: map` lines name every
executable file mapping by base, length, file offset and file size, and a file
of that size under one of the --lib-dir trees is taken to be the one that was
mapped. The interpreter (`libc.so`) is mapped by the kernel's loader rather
than by mmap, so it has no map line and is placed at its fixed base.
"""

import argparse
import collections
import os
import re
import struct
import subprocess
import sys

INTERP_BASE = 0x5_0000_0000

SYSCALLS = {
    0: "read", 1: "write", 2: "open", 3: "close", 4: "stat", 5: "fstat",
    6: "lstat", 7: "poll", 8: "lseek", 9: "mmap", 10: "mprotect",
    11: "munmap", 12: "brk", 13: "rt_sigaction", 14: "rt_sigprocmask",
    16: "ioctl", 17: "pread64", 18: "pwrite64", 19: "readv", 20: "writev",
    21: "access", 22: "pipe", 23: "select", 24: "sched_yield", 28: "madvise",
    32: "dup", 33: "dup2", 39: "getpid", 56: "clone", 57: "fork", 59: "execve",
    60: "exit", 61: "wait4", 62: "kill", 72: "fcntl", 73: "flock",
    74: "fsync", 75: "fdatasync", 76: "truncate", 77: "ftruncate",
    78: "getdents", 79: "getcwd", 80: "chdir", 82: "rename", 83: "mkdir",
    84: "rmdir", 86: "link", 87: "unlink", 89: "readlink", 90: "chmod",
    96: "gettimeofday", 97: "getrlimit", 158: "arch_prctl", 162: "sync",
    186: "gettid", 202: "futex", 217: "getdents64", 228: "clock_gettime",
    230: "clock_nanosleep", 231: "exit_group", 257: "openat", 262: "newfstatat",
    263: "unlinkat", 264: "renameat", 265: "linkat", 267: "readlinkat",
    270: "pselect6", 271: "ppoll", 280: "utimensat", 285: "fallocate",
    290: "eventfd2", 292: "dup3", 293: "pipe2", 302: "prlimit64",
    316: "renameat2", 318: "getrandom", 332: "statx",
}


def load_segments(path):
    """PT_LOAD (offset, vaddr, filesz) of an ELF64 file."""
    with open(path, "rb") as f:
        head = f.read(64)
        if head[:4] != b"\x7fELF" or head[4] != 2:
            return []
        phoff, = struct.unpack_from("<Q", head, 32)
        phentsize, phnum = struct.unpack_from("<HH", head, 54)
        f.seek(phoff)
        table = f.read(phentsize * phnum)
    segs = []
    for i in range(phnum):
        p_type, _flags, p_offset, p_vaddr, _paddr, p_filesz = struct.unpack_from(
            "<IIQQQQ", table, i * phentsize)
        if p_type == 1:
            segs.append((p_offset, p_vaddr, p_filesz))
    return segs


def offset_to_vaddr(segs, off):
    for p_offset, p_vaddr, p_filesz in segs:
        if p_offset <= off < p_offset + p_filesz:
            return off - p_offset + p_vaddr
    return None


def index_by_size(dirs):
    by_size = {}
    for d in dirs:
        for root, _dirs, files in os.walk(d):
            for name in files:
                path = os.path.join(root, name)
                if os.path.islink(path) or not os.path.isfile(path):
                    continue
                by_size.setdefault(os.path.getsize(path), path)
    return by_size


def symbolize(path, vaddrs):
    """{vaddr: function} through one llvm-symbolizer run."""
    if not vaddrs:
        return {}
    vaddrs = sorted(set(vaddrs))
    try:
        out = subprocess.run(
            ["llvm-symbolizer", "--obj=" + path, "--demangle", "--no-inlines",
             "--functions=linkage", "--output-style=GNU"],
            input="".join(f"0x{a:x}\n" for a in vaddrs), capture_output=True,
            text=True, check=False).stdout
    except FileNotFoundError:
        return {}
    names = [l for l in out.splitlines()[0::2]]
    return dict(zip(vaddrs, names))


def short(name, width=110):
    name = re.sub(r" \(\.llvm\.\d+\)", "", name)
    return name if len(name) <= width else name[: width - 1] + "…"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("log")
    ap.add_argument("--libc")
    ap.add_argument("--lib-dir", action="append", default=[])
    ap.add_argument("--top", type=int, default=40)
    args = ap.parse_args()

    lines = open(args.log, errors="replace").read().splitlines()
    prof = [l.split("]: ", 1)[1] for l in lines if l.startswith("PROF[post-userland-tests]: ")]

    print("== build times")
    for l in lines:
        m = re.search(r"guest_builds_the_(\w+)_system # (.*)", l)
        if m:
            print(f"  {m.group(1):6s} {m.group(2)}")

    kernel = collections.Counter()
    user_lines = []
    maps = []
    cpu = []
    other = []
    for p in prof:
        if m := re.match(r"kernel tick\s+(\d+) 0x([0-9a-f]+)(?: <(.*)\+0x[0-9a-f]+>)?", p):
            kernel[m.group(3) or "0x" + m.group(2)] += int(m.group(1))
        elif m := re.match(r"user tick\s+(\d+) 0x([0-9a-f]+)", p):
            user_lines.append((int(m.group(2), 16), int(m.group(1))))
        elif m := re.match(r"map base=0x(\w+) len=0x(\w+) off=0x(\w+) ino=(\d+) size=(\d+)", p):
            maps.append(tuple(int(m.group(i), 16) for i in (1, 2, 3)) + (int(m.group(4)), int(m.group(5))))
        elif p.startswith("cpu=") or p.startswith("span_ms"):
            cpu.append(p)
        elif m := re.match(r"syscall nr=(\d+) (.*)", p):
            other.append(f"syscall {SYSCALLS.get(int(m.group(1)), m.group(1)):>14s} {m.group(2)}")
        elif not p.startswith("    ") and not p.startswith("park site") and not p.startswith("switch masked"):
            other.append(p)

    print("== cpus")
    totals = collections.Counter()
    for c in cpu:
        print("  " + c)
        for k in ("user", "kernel", "idle"):
            if m := re.search(rf"{k}=(\d+)", c):
                totals[k] += int(m.group(1))
    all_ticks = sum(totals.values()) or 1
    print("  total " + " ".join(f"{k}={v} ({100 * v // all_ticks}%)" for k, v in totals.items()))

    print("== counters")
    for o in other:
        print("  " + o)

    print(f"== kernel ticks by function (top {args.top} of {sum(kernel.values())})")
    for name, n in kernel.most_common(args.top):
        print(f"  {n:7d} {short(name)}")

    by_size = index_by_size(args.lib_dir)
    objects = []
    for base, length, off, ino, size in maps:
        path = by_size.get(size)
        objects.append((base, base + length, off, path or f"<ino {ino} size {size}>"))
    if args.libc:
        segs = load_segments(args.libc)
        end = max(v + s for _o, v, s in segs) if segs else 0
        objects.append((INTERP_BASE, INTERP_BASE + end, None, args.libc))

    per_obj = collections.defaultdict(list)
    unmapped = 0
    for addr, n in user_lines:
        for base, end, off, path in objects:
            if base <= addr < end:
                if off is None:
                    vaddr = addr - base
                elif os.path.isfile(path):
                    vaddr = offset_to_vaddr(load_segments(path), addr - base + off)
                else:
                    vaddr = None
                per_obj[path].append((vaddr, n, addr))
                break
        else:
            unmapped += n

    user = collections.Counter()
    obj_total = collections.Counter()
    for path, hits in per_obj.items():
        names = symbolize(path, [v for v, _n, _a in hits if v is not None]) if os.path.isfile(path) else {}
        obj = os.path.basename(path)
        for vaddr, n, addr in hits:
            fn = names.get(vaddr) or f"0x{addr:x}"
            user[(obj, fn)] += n
            obj_total[obj] += n
    total_user = sum(n for _a, n in user_lines)
    print(f"== user ticks by object (sampled lines {total_user}, unmapped {unmapped})")
    for obj, n in obj_total.most_common():
        print(f"  {n:7d} {obj}")
    print(f"== user ticks by function (top {args.top})")
    for (obj, fn), n in user.most_common(args.top):
        print(f"  {n:7d} {obj[:24]:24s} {short(fn, 90)}")


if __name__ == "__main__":
    sys.exit(main())

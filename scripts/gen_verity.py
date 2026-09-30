#!/usr/bin/env python3
"""Append a SlopOS block-integrity ("verity") trailer to a raw ext2 image.

Layout (little-endian), appended after the existing image content:

    v1  [ image: N full blocks ][ pad ][ hash array: N × u32 ][ 32-byte header ]
    v2  [ image: N full blocks ][ pad ][ hash array: N × u32 ]
        [ attested bitmap: ceil(N/8) ][ 32-byte header ]

The 32-byte header is the LAST 32 bytes of the file, so the kernel locates the
trailer from the block device's capacity alone — no filesystem parsing. Each
entry of the hash array is the CRC-32 (IEEE/zlib, the same `zlib.crc32` used
here) of the corresponding 4 KiB data block; the header's `root` field is the
CRC-32 of the whole hash array, so a corrupt array is self-detecting.

Version 2 adds the attested bitmap and spends the header's `reserved` u32 on
its CRC-32: a set bit means the block holds what this host wrote and its hash
entry is that content's CRC; a clear bit's entry is zero and the kernel reads
the block unverified. A v2 image is a writable root; a v1 image is
write-protected outright and every one of its blocks is hashed.

A v2 seal attests the blocks the ext2 block bitmaps allocate, less the
superblock's (the kernel rewrites it on every mount) and less the guest's
taint, and with `--record FILE` keeps what it attested — the bitmap and every
block's hash — in FILE on the host. The guest can write every byte of the
image, trailer included, so the next build's `--taint-out` takes its measure
from that record alone: the guest's blocks are the allocated ones that are not
attested there with the content recorded still in place. A missing or foreign
record vouches for nothing. `--taint-out` runs before a preserved image is
grown or refreshed, and the seal takes its answer with `--taint`: a block the
guest wrote is never re-blessed, and one it freed is the host's again. Only
attested blocks are read, so each costs what the host wrote, not the image.

The pad makes the finished file a whole number of 512-byte sectors. A block
device reports its capacity in sectors, so an unpadded trailer whose header
straddled the last partial sector would sit *beyond* the reported capacity and
the kernel would never see it (SLOPOS-2026-0053). The pad goes before the
hash array, never after the header, so the header stays the last 32 bytes, and
neither the hash array nor the bitmap describes it.

Kernel side: fs/src/verity.rs (must keep the header layout + CRC in sync). A
v1 trailer-carrying device is write-protected there: verification and
writability are one decision, as in dm-verity.

This is an INTEGRITY check (detects accidental corruption / tampering loudly at
read time), not a cryptographic authenticity guarantee — see verity.rs docs.
"""

import argparse
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import zlib

MAGIC = 0x53565254  # 'TVRS' LE — SlopOS verity
ALGO_CRC32 = 1
HEADER_FMT = "<IIIIQII"  # magic, version, algo, block_size, block_count(u64), root, reserved
HEADER_SIZE = 32
SECTOR_SIZE = 512
READ_CHUNK = 1 << 20

EXT2_SUPERBLOCK_OFFSET = 1024
EXT2_SUPERBLOCK_LEN = 1024
EXT2_MAGIC = 0xEF53
EXT2_DESC_SIZE = 32
EXT4_FEATURE_INCOMPAT_64BIT = 0x80


class Ext2:
    """The geometry and block allocation of the ext2 filesystem at the start
    of an image, read straight from its superblock and group descriptors."""

    def __init__(self, f):
        f.seek(EXT2_SUPERBLOCK_OFFSET)
        sb = f.read(EXT2_SUPERBLOCK_LEN)
        if len(sb) < EXT2_SUPERBLOCK_LEN or struct.unpack_from("<H", sb, 56)[0] != EXT2_MAGIC:
            raise SystemExit("gen_verity: not an ext2 image")
        self.blocks = struct.unpack_from("<I", sb, 4)[0]
        self.first_data_block = struct.unpack_from("<I", sb, 20)[0]
        self.block_size = 1024 << struct.unpack_from("<I", sb, 24)[0]
        self.blocks_per_group = struct.unpack_from("<I", sb, 32)[0]
        if struct.unpack_from("<I", sb, 96)[0] & EXT4_FEATURE_INCOMPAT_64BIT:
            raise SystemExit("gen_verity: a 64bit filesystem, which SlopOS's ext2 does not mount")
        self.f = f

    def allocated(self) -> bytearray:
        """Bit `i` set when block `i` is in use, `1 << (i % 8)` of byte `i // 8`."""
        bitmap = bytearray((self.blocks + 7) // 8)
        for block in range(min(self.first_data_block, self.blocks)):
            bitmap[block >> 3] |= 1 << (block & 7)
        groups = (self.blocks - self.first_data_block + self.blocks_per_group - 1) // self.blocks_per_group
        self.f.seek((self.first_data_block + 1) * self.block_size)
        table = self.f.read(groups * EXT2_DESC_SIZE)
        for group in range(groups):
            self.f.seek(struct.unpack_from("<I", table, group * EXT2_DESC_SIZE)[0] * self.block_size)
            group_bits = self.f.read(self.blocks_per_group // 8)
            base = self.first_data_block + group * self.blocks_per_group
            count = min(self.blocks_per_group, self.blocks - base)
            if base % 8 == 0:
                whole = count // 8
                bitmap[base // 8 : base // 8 + whole] = group_bits[:whole]
                start = whole * 8
            else:
                start = 0
            for i in range(start, count):
                if group_bits[i >> 3] >> (i & 7) & 1:
                    block = base + i
                    bitmap[block >> 3] |= 1 << (block & 7)
        return bitmap


def trailer_of(f, size: int):
    """`(header fields, filesystem bytes)`: the filesystem's extent is its
    superblock's, and a trailer is one only past it, describing exactly it.
    Anything else is the guest's bytes, which a seal replaces."""
    fs = Ext2(f)
    fs_bytes = fs.blocks * fs.block_size
    if fs_bytes > size:
        raise SystemExit(f"gen_verity: the filesystem claims {fs_bytes}B of a {size}B file")
    if size - fs_bytes < HEADER_SIZE:
        return None, fs_bytes
    f.seek(size - HEADER_SIZE)
    fields = struct.unpack(HEADER_FMT, f.read(HEADER_SIZE))
    magic, version, algo, block_size, block_count, _root, _reserved = fields
    if magic != MAGIC or version not in (1, 2) or algo != ALGO_CRC32:
        return None, fs_bytes
    if block_size * block_count != fs_bytes:
        return None, fs_bytes
    return fields, fs_bytes


RECORD_MAGIC = b"SVRECORD"
RECORD_FMT = "<8sIIQ"  # magic, version, block_size, block_count


def write_record(path: str, block_size: int, bitmap: bytes, hashes: bytes) -> None:
    n = len(hashes) // 4
    with open(path + ".part", "wb") as f:
        f.write(struct.pack(RECORD_FMT, RECORD_MAGIC, 1, block_size, n))
        f.write(bitmap)
        f.write(hashes)
    os.replace(path + ".part", path)


def read_record(path: str | None):
    """`(block_size, bitmap, hashes)` of a record `write_record` left, or
    `None`."""
    try:
        with open(path, "rb") as f:
            data = f.read()
    except (FileNotFoundError, TypeError):
        return None
    head = struct.calcsize(RECORD_FMT)
    if len(data) < head:
        return None
    magic, version, block_size, n = struct.unpack_from(RECORD_FMT, data)
    bitmap_len = (n + 7) // 8
    if magic != RECORD_MAGIC or version != 1 or len(data) != head + bitmap_len + 4 * n:
        return None
    return block_size, data[head : head + bitmap_len], data[head + bitmap_len :]


def superblock_blocks(block_size: int) -> range:
    first = EXT2_SUPERBLOCK_OFFSET // block_size
    last = (EXT2_SUPERBLOCK_OFFSET + EXT2_SUPERBLOCK_LEN - 1) // block_size
    return range(first, last + 1)


def and_not(a: bytearray, b: bytes) -> bytearray:
    """`a & ~b` over the bytes both describe; `a` beyond `b` is kept."""
    out = bytearray(a)
    for i in range(min(len(a), len(b))):
        out[i] &= ~b[i] & 0xFF
    return out


def taint(path: str, record_path: str | None) -> bytes:
    """The blocks the guest owns now: allocated, and not attested in the
    host's record with the content it recorded still there."""
    record = read_record(record_path)
    with open(path, "rb") as f:
        fs = Ext2(f)
        allocated = fs.allocated()
        still = bytearray(len(allocated))
        if record is not None and record[0] == fs.block_size:
            _, bitmap, hashes = record
            block_size = fs.block_size
            per_chunk = max(1, READ_CHUNK // block_size)
            for first, count in attested_blocks(bitmap, min(len(hashes) // 4, fs.blocks)):
                done = 0
                while done < count:
                    take = min(per_chunk, count - done)
                    f.seek((first + done) * block_size)
                    chunk = f.read(take * block_size)
                    for i in range(take):
                        block = first + done + i
                        crc = zlib.crc32(chunk[i * block_size : (i + 1) * block_size]) & 0xFFFFFFFF
                        if hashes[4 * block : 4 * block + 4] == struct.pack("<I", crc):
                            still[block >> 3] |= 1 << (block & 7)
                    done += take
    return bytes(and_not(allocated, still))


def attested_blocks(bitmap: bytes, n: int):
    """Runs `(first, count)` of set bits below `n`."""
    run_start = None
    for byte_index, byte in enumerate(bitmap):
        if byte == 0:
            if run_start is not None:
                yield run_start, byte_index * 8 - run_start
                run_start = None
            continue
        for bit in range(8):
            block = byte_index * 8 + bit
            if block >= n:
                break
            if byte >> bit & 1:
                if run_start is None:
                    run_start = block
            elif run_start is not None:
                yield run_start, block - run_start
                run_start = None
    if run_start is not None:
        yield run_start, n - run_start


def seal(path: str, version: int, taint_bits: bytes | None, record_path: str | None = None) -> str:
    size = os.path.getsize(path)
    with open(path, "rb") as f:
        _fields, fs_bytes = trailer_of(f, size)
        fs = Ext2(f)
        block_size = fs.block_size
        if fs_bytes % block_size != 0:
            raise SystemExit(f"gen_verity: image size {fs_bytes} is not a multiple of {block_size}")
        n = fs_bytes // block_size
        if version == 2:
            bitmap = bytearray((n + 7) // 8)
            allocated = fs.allocated()
            bitmap[: len(allocated)] = allocated[: len(bitmap)]
            bitmap = and_not(bitmap, taint_bits or b"")
            for block in superblock_blocks(block_size):
                if block < n:
                    bitmap[block >> 3] &= ~(1 << (block & 7)) & 0xFF
            if n % 8:
                bitmap[-1] &= (1 << (n % 8)) - 1
        else:
            bitmap = bytearray(b"\xff" * ((n + 7) // 8))
        hashes = bytearray(4 * n)
        hashed = 0
        per_chunk = max(1, READ_CHUNK // block_size)
        for first, count in attested_blocks(bitmap, n):
            done = 0
            while done < count:
                take = min(per_chunk, count - done)
                f.seek((first + done) * block_size)
                chunk = f.read(take * block_size)
                for i in range(take):
                    struct.pack_into(
                        "<I", hashes, 4 * (first + done + i),
                        zlib.crc32(chunk[i * block_size : (i + 1) * block_size]) & 0xFFFFFFFF,
                    )
                done += take
            hashed += count

    root = zlib.crc32(bytes(hashes)) & 0xFFFFFFFF
    if version == 2:
        bitmap_bytes = bytes(bitmap)
        reserved = zlib.crc32(bitmap_bytes) & 0xFFFFFFFF
    else:
        bitmap_bytes = b""
        reserved = 0
    if version == 2 and record_path:
        write_record(record_path, block_size, bitmap_bytes, bytes(hashes))
    header = struct.pack(HEADER_FMT, MAGIC, version, ALGO_CRC32, block_size, n, root, reserved)
    unpadded = fs_bytes + len(hashes) + len(bitmap_bytes) + len(header)
    pad = (-unpadded) % SECTOR_SIZE
    # `r+b` plus truncate, not `ab`: a re-run replaces the stripped trailer
    # instead of appending a second one.
    with open(path, "r+b") as f:
        f.seek(fs_bytes)
        f.write(b"\0" * pad)
        f.write(bytes(hashes))
        f.write(bitmap_bytes)
        f.write(header)
        f.truncate()
    total = unpadded + pad
    assert total % SECTOR_SIZE == 0
    attested = f", {hashed}/{n} blocks attested (crc 0x{reserved:08x})" if version == 2 else ""
    return (
        f"verity: appended v{version} trailer for {n} blocks ({block_size}B),"
        f" root crc 0x{root:08x}{attested}, {pad}B pad, {total} bytes total"
    )


def self_test() -> None:
    for tool in ("mkfs.ext2", "debugfs", "resize2fs", "e2fsck"):
        if shutil.which(tool) is None:
            raise SystemExit(f"gen_verity: --self-test needs {tool}")
    work = tempfile.mkdtemp(prefix="gen_verity.")
    try:
        _self_test(work)
    finally:
        shutil.rmtree(work)
    print("gen_verity: --self-test OK")


def _self_test(work: str) -> None:
    image = os.path.join(work, "root.img")
    record = os.path.join(work, "record")
    payload = os.path.join(work, "payload")

    def run(*argv):
        subprocess.run(argv, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def debugfs(*requests):
        script = os.path.join(work, "requests")
        with open(script, "w") as f:
            f.write("\n".join(requests) + "\n")
        run("debugfs", "-w", "-f", script, image)

    def blocks_of(name):
        out = subprocess.run(["debugfs", "-R", f"blocks {name}", image],
                             check=True, capture_output=True, text=True).stdout
        return [int(b) for b in out.split()]

    def trailer():
        size = os.path.getsize(image)
        with open(image, "rb") as f:
            fields, fs_bytes = trailer_of(f, size)
            n = fs_bytes // fields[3]
            f.seek(size - HEADER_SIZE - (n + 7) // 8 - 4 * n)
            hashes = f.read(4 * n)
            bitmap = f.read((n + 7) // 8)
        return fields, hashes, bitmap

    def attested(bitmap, block):
        return bitmap[block >> 3] >> (block & 7) & 1

    def check(what, held):
        if not held:
            raise SystemExit(f"gen_verity: --self-test: {what}")

    def strip():
        size = os.path.getsize(image)
        with open(image, "rb") as f:
            _fields, fs_bytes = trailer_of(f, size)
        os.truncate(image, fs_bytes)

    def write_at(block, data):
        with open(image, "r+b") as f:
            f.seek(block * 4096)
            f.write(data)

    def forge_bitmap(set_every_bit):
        """What a guest's own kernel may do to the trailer's bitmap."""
        size = os.path.getsize(image)
        with open(image, "r+b") as f:
            f.seek(size - HEADER_SIZE)
            header = list(struct.unpack(HEADER_FMT, f.read(HEADER_SIZE)))
            length = (header[4] + 7) // 8
            forged = bytes([0xFF] * length) if set_every_bit else bytes(length)
            f.seek(size - HEADER_SIZE - length)
            f.write(forged)
            header[6] = zlib.crc32(forged) & 0xFFFFFFFF
            f.write(struct.pack(HEADER_FMT, *header))

    with open(image, "wb") as f:
        f.truncate(16 << 20)
    run("mkfs.ext2", "-F", "-q", "-b", "4096", image)
    with open(payload, "wb") as f:
        f.write(os.urandom(64 << 10))
    debugfs(f"write {payload} host")
    seal(image, 2, None, record)
    fields, hashes, bitmap = trailer()
    host = blocks_of("host")
    with open(image, "rb") as f:
        allocated = Ext2(f).allocated()
    free = next(b for b in range(fields[4] - 1, 0, -1) if not allocated[b >> 3] >> (b & 7) & 1)
    check("a fresh seal left a block the host wrote unattested", all(attested(bitmap, b) for b in host))
    check("a fresh seal attested a free block", not attested(bitmap, free) and hashes[4 * free : 4 * free + 4] == b"\0" * 4)
    check("the superblock's block came out attested", not attested(bitmap, 0))
    with open(image, "rb") as f:
        f.seek(host[0] * 4096)
        want = struct.pack("<I", zlib.crc32(f.read(4096)) & 0xFFFFFFFF)
    check("a host block's hash is not its content's CRC", hashes[4 * host[0] : 4 * host[0] + 4] == want)
    check("the record is not what the seal attested", read_record(record) == (4096, bitmap, hashes))

    # A guest's kernel rewrites a block the host attested and sets every bit
    # of the trailer's bitmap, its CRC made to match: the record still says
    # the block no longer holds what the host wrote.
    guest_block = host[1]
    write_at(guest_block, b"guest" * 800)
    forge_bitmap(set_every_bit=True)
    taint_path = os.path.join(work, "taint")
    with open(taint_path, "wb") as f:
        f.write(taint(image, record))
    with open(taint_path, "rb") as f:
        guest_owned = f.read()
    check("a block the guest rewrote under a forged bitmap is not the guest's", attested(guest_owned, guest_block))
    check("a block nobody touched came out the guest's", not attested(guest_owned, host[0]))
    strip()
    run("e2fsck", "-fy", image)
    with open(image, "r+b") as f:
        f.truncate(32 << 20)
    run("resize2fs", image)
    debugfs(f"write {payload} refreshed")
    seal(image, 2, guest_owned, record)
    fields, hashes, bitmap = trailer()
    check("a grown image re-attested the guest's block", not attested(bitmap, guest_block))
    check("a refresh left a block it wrote unattested", all(attested(bitmap, b) for b in blocks_of("refreshed")))
    check("a refresh un-attested a block nobody touched", attested(bitmap, host[0]))

    # A trailer the guest zeroed vouches for nothing it was not already
    # vouching for, and a record that is missing vouches for nothing at all.
    forge_bitmap(set_every_bit=False)
    check("a zeroed trailer bitmap moved the record's measure",
          not attested(taint(image, record), host[0]))
    with open(image, "rb") as f:
        allocated = bytes(Ext2(f).allocated())
    check("a missing record vouched for a block", taint(image, os.path.join(work, "none")) == allocated)

    # A header the guest writes into the filesystem's last bytes is its data,
    # and so is one past them that does not describe the filesystem: neither
    # is a trailer, and a seal cuts nothing off at what they claim.
    strip()
    fs_size = os.path.getsize(image)
    with open(image, "r+b") as f:
        f.seek(fs_size - HEADER_SIZE)
        f.write(struct.pack(HEADER_FMT, MAGIC, 2, ALGO_CRC32, 4096, 1, 0, 0))
    with open(image, "rb") as f:
        check("a header inside the filesystem passed for a trailer", trailer_of(f, fs_size)[0] is None)
    with open(image, "ab") as f:
        f.write(struct.pack(HEADER_FMT, MAGIC, 2, ALGO_CRC32, 4096, 999999, 0, 0))
    with open(image, "rb") as f:
        check("a header past the filesystem that does not describe it passed for a trailer",
              trailer_of(f, os.path.getsize(image)) == (None, fs_size))
    seal(image, 2, taint(image, record), record)
    with open(image, "rb") as f:
        check("a seal cut the filesystem at a forged header",
              trailer_of(f, os.path.getsize(image))[1] == fs_size)


def main() -> int:
    parser = argparse.ArgumentParser(description="append a SlopOS verity trailer")
    parser.add_argument("image", nargs="?", help="raw ext2 image to seal in place")
    parser.add_argument("--version", type=int, choices=(1, 2), default=1,
                        help="1 = write-protected, every block hashed (default); 2 = writable, allocated blocks attested")
    parser.add_argument("--taint", metavar="FILE", help="v2: blocks never to attest, as --taint-out wrote them")
    parser.add_argument("--taint-out", metavar="FILE", help="write the guest's blocks of IMAGE and seal nothing")
    parser.add_argument("--record", metavar="FILE",
                        help="v2: where the host keeps what a seal attested, which --taint-out measures against")
    parser.add_argument("--self-test", action="store_true", help="prove a seal attests what it should")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    if args.image is None:
        parser.error("an image is required")
    if args.taint_out:
        with open(args.taint_out, "wb") as f:
            f.write(taint(args.image, args.record))
        return 0
    if (args.taint or args.record) and args.version != 2:
        parser.error("--taint and --record are for a v2 seal")
    taint_bits = None
    if args.taint:
        with open(args.taint, "rb") as f:
            taint_bits = f.read()
    print(seal(args.image, args.version, taint_bits, args.record))
    return 0


if __name__ == "__main__":
    sys.exit(main())

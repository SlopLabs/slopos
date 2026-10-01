"""The volume SlopOS formats, from `ext4-core/profile`, for the scripts that
make one."""

import os

PROFILE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "ext4-core", "profile")


def values() -> dict:
    out = {}
    with open(PROFILE) as f:
        for line in f:
            line = line.strip()
            if line and not line.startswith("#"):
                key, _, value = line.partition("=")
                out[key] = value
    return out


def mkfs_args() -> list:
    """`mke2fs` arguments for a profile volume; `-O none` first, so nothing
    the host's mke2fs.conf adds survives."""
    v = values()
    return ["-t", "ext4", "-O", "none", "-O", v["features"], "-I", v["inode_size"], "-b", v["block_size"]]

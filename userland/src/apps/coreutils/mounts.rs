//! `mount` and `umount`, in util-linux's and BusyBox's spelling.

use std::ffi::CString;

use slopos_abi::fs::{MNT_DETACH, MS_RDONLY};

use crate::syscall::fs;

use super::opts::{Opt, Opts};
use super::{Ctx, Tool};

pub static TOOLS: &[Tool] = &[
    Tool {
        name: "mount",
        desc: "Attach a filesystem at a directory",
        usage: MOUNT_USAGE,
        run: mount,
    },
    Tool {
        name: "umount",
        desc: "Detach the filesystem at a directory",
        usage: UMOUNT_USAGE,
        run: umount,
    },
];

const MOUNT_USAGE: &str = "mount [-rw] [-t type] [-o ro|rw[,...]] source directory";
const UMOUNT_USAGE: &str = "umount [-l] directory";

/// What a device holds when no type is named: the kernel mounts ext2, ext3
/// and ext4 from a device, one driver, which refuses a device holding none.
const DEVICE_TYPE: &[u8] = b"ext4";

/// util-linux's status for a mount or an unmount that failed.
const FAILED: i32 = 32;

fn mount(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut read_only = false;
    let mut fstype = DEVICE_TYPE;
    let mut opts = Opts::new(argv, "rwt:o:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'r') => read_only = true,
            Opt::Flag(b'w') => read_only = false,
            Opt::Value(b't', named) => fstype = named,
            Opt::Value(b'o', list) => {
                for option in list.split(|&b| b == b',') {
                    match option {
                        b"ro" => read_only = true,
                        b"rw" => read_only = false,
                        other => {
                            ctx.warn_at(other, b"not an option mount takes");
                            return ctx.usage(MOUNT_USAGE);
                        }
                    }
                }
            }
            Opt::Unknown(flag) | Opt::Missing(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(MOUNT_USAGE);
            }
            _ => return ctx.usage(MOUNT_USAGE),
        }
    }
    let [source, target] = opts.operands() else {
        return ctx.usage(MOUNT_USAGE);
    };
    let flags = if read_only { MS_RDONLY } else { 0 };
    match fs::mount(source, target, fstype, flags) {
        Ok(()) => 0,
        Err(e) => {
            ctx.warn_at(source, e.to_string().as_bytes());
            FAILED
        }
    }
}

fn umount(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut flags = 0;
    let mut opts = Opts::new(argv, "l");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'l') => flags |= MNT_DETACH,
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(UMOUNT_USAGE);
            }
            _ => return ctx.usage(UMOUNT_USAGE),
        }
    }
    let [target] = opts.operands() else {
        return ctx.usage(UMOUNT_USAGE);
    };
    let Ok(path) = CString::new(*target) else {
        ctx.warn_at(target, b"a NUL in the path");
        return 1;
    };
    match fs::umount2(path.as_ptr(), flags) {
        Ok(()) => 0,
        Err(e) => {
            ctx.warn_at(target, e.to_string().as_bytes());
            FAILED
        }
    }
}

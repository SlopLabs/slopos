//! The cargo and rustc a check runs, chosen by the system xtask runs on. On
//! Linux and macOS that is rustup's proxy with `+TOOLCHAIN`: the pinned
//! nightly for the host's own tools, `slopos` for the owned sysroot. SlopOS
//! has no rustup, and its one installed toolchain is both.

use std::process::Command;

use crate::Result;
use crate::repo::Repo;

/// The owned sysroot SlopOS's targets build against.
pub const SLOPOS: &str = "slopos";

/// `cargo`, with `+TOOLCHAIN` where rustup chooses one. `toolchain` is
/// [`SLOPOS`] or a channel.
pub fn cargo(toolchain: &str) -> Command {
    proxied("cargo", toolchain)
}

/// `rustc`, as [`cargo`] chooses it.
#[cfg(test)]
pub fn rustc(toolchain: &str) -> Command {
    proxied("rustc", toolchain)
}

/// `rustdoc`, as [`cargo`] chooses it.
#[cfg(test)]
pub fn rustdoc(toolchain: &str) -> Command {
    proxied("rustdoc", toolchain)
}

fn proxied(program: &str, toolchain: &str) -> Command {
    let mut cmd = Command::new(program);
    if let Some(plus) = plus(toolchain) {
        cmd.arg(plus);
    }
    cmd
}

/// How [`cargo`] reads in a log line.
pub fn shown(toolchain: &str) -> String {
    match plus(toolchain) {
        Some(plus) => format!("cargo {plus}"),
        None => "cargo".to_string(),
    }
}

fn plus(toolchain: &str) -> Option<String> {
    (!cfg!(target_os = "slopos")).then(|| format!("+{toolchain}"))
}

/// The host's channel, from the checkout's `rust-toolchain.toml`.
pub fn channel(repo: &Repo) -> Result<String> {
    channel_of(&repo.read("rust-toolchain.toml")?)
}

/// `rust-toolchain.toml`'s `channel = "..."`.
pub fn channel_of(toml: &str) -> Result<String> {
    toml.lines()
        .filter_map(|l| l.trim().strip_prefix("channel"))
        .filter_map(|rest| rest.trim_start().strip_prefix('='))
        .map(|v| v.trim().trim_matches('"').to_string())
        .find(|v| !v.is_empty())
        .ok_or_else(|| "rust-toolchain.toml names no channel".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_channel_comes_from_rust_toolchain_toml() {
        let toml = "[toolchain]\nchannel = \"nightly-2026-09-03\"\ncomponents = [\"clippy\"]\n";
        assert_eq!(channel_of(toml).unwrap(), "nightly-2026-09-03");
        assert!(channel_of("[toolchain]\n").is_err());
    }

    #[test]
    fn rustup_chooses_the_toolchain_everywhere_but_slopos() {
        let args: Vec<String> = cargo(SLOPOS)
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if cfg!(target_os = "slopos") {
            assert!(args.is_empty());
            assert_eq!(shown(SLOPOS), "cargo");
        } else {
            assert_eq!(args, ["+slopos"]);
            assert_eq!(shown(SLOPOS), "cargo +slopos");
        }
    }
}

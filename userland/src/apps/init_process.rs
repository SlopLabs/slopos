use slopos_abi::caps::{DISPLAY_SEAT, INPUT_SEAT, POWER, RAW_BLOCK};
use slopos_abi::fs::{SESSION_GID, SESSION_HOME, SESSION_UID};
use slopos_abi::syscall::F_DUPFD_CLOEXEC;
use slopos_abi::syscall::{BOOT_FLAG_ROULETTE_SKIP, BOOT_FLAG_TESTS_ENABLED};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_font::atlas::GlyphAtlas;
use slopos_slibc::pal::{Pal, Sys};

use crate::apps::privd::CONSENT_FD;
use crate::readiness::{NOTIFIER_FD, ReadinessGate};
use crate::ring::{Ring, slopfut};
use crate::syscall::process::Endowment;
use crate::syscall::{OwnedFd, UserSysInfo, core as sys_core, net, process, tty};

fn upgrade_console_font() {
    let font_data = match crate::gfx::font_loader::load_font("mono") {
        Some(data) => data,
        None => return,
    };
    let atlas = match GlyphAtlas::new(font_data, 16) {
        Some(a) => a,
        None => return,
    };
    let mut payload = Vec::with_capacity(atlas.coverage_len() + atlas.replacement().len());
    for chunk in atlas.coverage_chunks() {
        payload.extend_from_slice(chunk);
    }
    payload.extend_from_slice(atlas.replacement());
    tty::font_set_coverage(
        &payload,
        atlas.cell_width() as u16,
        atlas.cell_height() as u16,
    );
}

/// A service init starts: the capabilities init delegates to it beside the
/// universal ones, its tier, and whether it runs as the session or as uid 0.
struct Service {
    path: &'static str,
    caps: u64,
    priority: TaskPriority,
    session: bool,
}

/// Runs what the policy allows, holding everything an action may name.
const PRIVD: Service = Service {
    path: "/sbin/privd",
    caps: slopos_priv_core::DELEGABLE,
    priority: TaskPriority::Normal,
    session: false,
};

const COMPOSITOR: Service = Service {
    path: "/bin/compositor",
    caps: DISPLAY_SEAT | INPUT_SEAT,
    priority: TaskPriority::High,
    session: true,
};

const TERMINAL: Service = Service {
    path: "/bin/terminal",
    caps: 0,
    priority: TaskPriority::Normal,
    session: true,
};

const REMOTED: Service = Service {
    path: "/bin/remoted",
    caps: 0,
    priority: TaskPriority::Normal,
    session: true,
};

const ROULETTE: Service = Service {
    path: "/bin/roulette",
    caps: DISPLAY_SEAT,
    priority: TaskPriority::Normal,
    session: true,
};

/// The session's runtime directory, its own alone.
fn runtime_dir() -> String {
    format!("/run/user/{SESSION_UID}")
}

/// What every program the session starts finds in its environment.
fn session_env() -> [String; 4] {
    [
        format!("HOME={SESSION_HOME}\0"),
        "USER=user\0".to_owned(),
        "LOGNAME=user\0".to_owned(),
        format!("XDG_RUNTIME_DIR={}\0", runtime_dir()),
    ]
}

fn spawn_service(service: &Service) -> i32 {
    spawn_service_with(service, &[])
}

/// The child gets the caller's stdio and each `(fd, as)` pair: init's `fd` at
/// the child's `as`.
fn spawn_service_with(service: &Service, extra_fds: &[(i32, i32)]) -> i32 {
    let mut actions = vec![
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    actions.extend(extra_fds.iter().map(|&(fd, at)| process::clone_fd(fd, at)));
    let env = session_env();
    let (envp, cwd, ids): (Vec<*const u8>, &str, _) = if service.session {
        let cwd = if std::path::Path::new(SESSION_HOME).is_dir() {
            SESSION_HOME
        } else {
            "/"
        };
        (
            env.iter().map(|var| var.as_ptr()).collect(),
            cwd,
            Some((SESSION_UID, SESSION_GID)),
        )
    } else {
        (Vec::new(), "/", None)
    };
    let tid = process::spawn_endowed(
        service.path.as_bytes(),
        &[],
        &envp,
        cwd.as_bytes(),
        service.priority,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
        Endowment {
            caps: service.caps,
            ids,
            groups: service.session.then_some(&[][..]),
        },
    );
    if tid <= 0 {
        eprintln!("init: failed to spawn {}", service.path);
    }
    tid
}

/// Start privd and wait until it listens, handing it its end of the consent
/// channel; the other end is the compositor's, `None` when there is no
/// channel.
fn start_privd() -> Option<OwnedFd> {
    let gate = ReadinessGate::create();
    let pair = net::socketpair(slopos_abi::net::SOCK_CLOEXEC);
    let mut fds = Vec::new();
    if gate.is_some() {
        fds.push((NOTIFIER_FD, NOTIFIER_FD));
    }
    if let Ok((ours, _)) = &pair {
        fds.push((ours.raw(), CONSENT_FD));
    }
    spawn_service_with(&PRIVD, &fds);
    if let Some(gate) = gate {
        gate.wait();
    }
    let (_, agent) = pair.ok()?;
    // The compositor's readiness gate takes the notifier's number next.
    let moved = Sys::fcntl(agent.raw(), F_DUPFD_CLOEXEC as i32, NOTIFIER_FD as u64 + 1).ok()?;
    // SAFETY: fcntl just opened this descriptor for us.
    Some(unsafe { OwnedFd::from_raw(moved as i32) })
}

/// What a crashed boot left goes to `/var/log/crash` before the tests and the
/// services run.
fn collect_crash_records() {
    let argv = [b"bootctl\0".as_ptr(), b"collect\0".as_ptr()];
    let actions = [process::clone_fd(1, 1), process::clone_fd(2, 2)];
    let tid = process::spawn_endowed(
        b"/bin/bootctl",
        &argv,
        &[],
        &[],
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
        Endowment {
            caps: RAW_BLOCK | POWER,
            ..Endowment::NONE
        },
    );
    if tid > 0 {
        let _ = process::waitpid(tid as u32);
    }
}

/// In the sealed base of a machine the host built paired; the daemon runs
/// only there.
const REMOTE_CONF: &str = "/usr/share/slopos/remote/remote.conf";

/// On a live boot, the medium's apps at the RAM root's `/usr/local`, where an
/// install puts them on a disk.
fn install_live_apps() {
    use slopos_boot_core::layout::{MEDIUM_APPS, MEDIUM_DIR};
    let live = crate::syscall::fs::statfs_path(c"/".as_ptr())
        .is_ok_and(|stats| stats.f_type == slopos_abi::fs::RAMFS_MAGIC);
    let apps = std::path::Path::new(MEDIUM_DIR).join(MEDIUM_APPS);
    if !live || !apps.is_dir() {
        return;
    }
    let install = crate::tree::Install {
        root: "",
        name: slopos_tree_core::APPS,
        guest: "/usr/local",
        source: &apps,
        owner: Some((SESSION_UID, SESSION_GID)),
        keeper: Some((SESSION_UID, SESSION_GID)),
        remedy: "move it aside",
    };
    match crate::tree::scan(&apps).and_then(|new| install.run(&new, &mut |_| {})) {
        Ok(Some(_)) => println!("init: installed the medium's apps at /usr/local"),
        Ok(None) => {}
        Err(e) => eprintln!("init: the medium's apps: {e}"),
    }
}

pub fn init_user_main() {
    upgrade_console_font();
    collect_crash_records();
    crate::root_layout::lay_out(std::path::Path::new("/"));
    install_live_apps();
    crate::root_layout::make_runtime_dir(&runtime_dir());
    // Before anything that may ask it; until the compositor says it can draw,
    // privd has nobody to ask.
    let consent_agent = start_privd();

    // Early, so a machine whose desktop never comes up can still be reached;
    // never waited for. The final loop reaps it if it exits.
    if std::fs::metadata(REMOTE_CONF).is_ok() {
        spawn_service(&REMOTED);
    }

    // Must precede anything interactive; a missing or invalid /etc/keymap
    // leaves the built-in US default active.
    crate::keymap::apply_persisted_layout();

    let mut info = UserSysInfo::default();
    let info_ok = sys_core::sys_info(&mut info) == 0;
    let skip_roulette = info_ok && (info.boot_flags & BOOT_FLAG_ROULETTE_SKIP) != 0;
    let tests_enabled = info_ok && (info.boot_flags & BOOT_FLAG_TESTS_ENABLED) != 0;

    if tests_enabled {
        // Exiting on success stops the boot pipeline waiting for input;
        // failure falls through to the normal flow so a shell is reachable.
        let rc = sys_core::run_userland_tests();
        if rc == 0 {
            sys_core::exit_with_code(0);
        }
    }

    if !skip_roulette {
        let roulette_tid = spawn_service(&ROULETTE);
        if roulette_tid > 0 {
            let _ = process::waitpid(roulette_tid as u32);
        }
    }

    let gate = ReadinessGate::create();
    // Without the inherited notifier (fd 3), init's gate read EOFs at once and
    // races the terminal ahead of a ready compositor. Clone it only when the
    // gate was actually set up.
    let mut fds = Vec::new();
    if gate.is_some() {
        fds.push((NOTIFIER_FD, NOTIFIER_FD));
    }
    if let Some(agent) = &consent_agent {
        fds.push((agent.raw(), CONSENT_FD));
    }
    let compositor_tid = spawn_service_with(&COMPOSITOR, &fds);
    drop(consent_agent);

    // Peak in-flight is one gate read followed by one pidfd poll, so a small
    // ring suffices.
    match Ring::setup(8) {
        Ok(ring) => {
            slopfut::block_on(ring, async {
                if let Some(gate) = gate {
                    gate.wait_async().await;
                }

                spawn_service(&TERMINAL);

                if compositor_tid > 0 {
                    let _ = slopfut::process::Child::from_pid(compositor_tid as u32)
                        .wait()
                        .await;
                }
            });
        }
        Err(_) => {
            if let Some(gate) = gate {
                gate.wait();
            }
            spawn_service(&TERMINAL);
            if compositor_tid > 0 {
                let _ = process::waitpid(compositor_tid as u32);
            }
        }
    }

    // Init outlives every service it starts, so nothing else ever reaps them:
    // yielding alone accumulates one zombie per terminal the user closes.
    loop {
        process::reap_exited_children();
        sys_core::yield_now();
    }
}

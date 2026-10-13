use slopos_abi::caps::{DISPLAY_SEAT, INPUT_SEAT, LAUNCH, POWER, RAW_BLOCK};
use slopos_abi::fs::{SESSION_GID, SESSION_HOME, SESSION_UID};
use slopos_abi::syscall::{BOOT_FLAG_ROULETTE_SKIP, BOOT_FLAG_TESTS_ENABLED};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_font::atlas::GlyphAtlas;

use crate::readiness::ReadinessGate;
use crate::ring::{Ring, slopfut};
use crate::syscall::process::Endowment;
use crate::syscall::{UserSysInfo, core as sys_core, process, tty};

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

/// A service init starts as the session: the capabilities init delegates to
/// it beside the universal ones, and its tier.
struct Service {
    path: &'static str,
    caps: u64,
    priority: TaskPriority,
}

/// `Launch` until a broker runs what the session asks for: the grant rows
/// that remain are reached through the session's launchers.
const COMPOSITOR: Service = Service {
    path: "/bin/compositor",
    caps: DISPLAY_SEAT | INPUT_SEAT | LAUNCH,
    priority: TaskPriority::High,
};

const TERMINAL: Service = Service {
    path: "/bin/terminal",
    caps: LAUNCH,
    priority: TaskPriority::Normal,
};

const REMOTED: Service = Service {
    path: "/bin/remoted",
    caps: LAUNCH,
    priority: TaskPriority::Normal,
};

const ROULETTE: Service = Service {
    path: "/bin/roulette",
    caps: DISPLAY_SEAT,
    priority: TaskPriority::Normal,
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
    spawn_service_inheriting(service, &[])
}

/// The child inherits the caller's stdio plus each fd in `extra_fds` via the
/// fd-action allow-list. At most two extra fds are honored.
fn spawn_service_inheriting(service: &Service, extra_fds: &[i32]) -> i32 {
    let mut actions = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
        process::clone_fd(-1, -1),
        process::clone_fd(-1, -1),
    ];
    let base = 3;
    let n = extra_fds.len().min(actions.len() - base);
    for (slot, &fd) in actions[base..base + n].iter_mut().zip(extra_fds) {
        *slot = process::clone_fd(fd, fd);
    }
    let env = session_env();
    let envp: Vec<*const u8> = env.iter().map(|var| var.as_ptr()).collect();
    let cwd = if std::path::Path::new(SESSION_HOME).is_dir() {
        SESSION_HOME
    } else {
        "/"
    };
    let tid = process::spawn_endowed(
        service.path.as_bytes(),
        &[],
        &envp,
        cwd.as_bytes(),
        service.priority,
        TASK_FLAG_USER_MODE,
        &actions[..base + n],
        0,
        Endowment {
            caps: service.caps,
            ids: Some((SESSION_UID, SESSION_GID)),
            groups: Some(&[]),
        },
    );
    if tid <= 0 {
        eprintln!("init: failed to spawn {}", service.path);
    }
    tid
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

pub fn init_user_main() {
    upgrade_console_font();
    collect_crash_records();
    crate::root_layout::lay_out(std::path::Path::new("/"));
    crate::root_layout::make_runtime_dir(&runtime_dir());

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
    let compositor_tid = if gate.is_some() {
        spawn_service_inheriting(&COMPOSITOR, &[crate::readiness::NOTIFIER_FD])
    } else {
        spawn_service(&COMPOSITOR)
    };

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

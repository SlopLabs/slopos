use super::wait::*;
use slopos_abi::signal::{
    WAIT_STATUS_CONTINUED, wait_status_exited, wait_status_signalled, wait_status_stopped,
};

pub fn run_process_tests() -> (u32, u32) {
    let mut pass = 0u32;
    let mut fail = 0u32;

    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond {
                pass += 1;
            } else {
                fail += 1;
            }
        };
    }

    // Literals, not the encoder: a drift on either side of the ABI fails here.
    let normal_exit_42 = wait_status_exited(42) as i32;
    check!("WIFEXITED normal", WIFEXITED(normal_exit_42));
    check!("WEXITSTATUS 42", WEXITSTATUS(normal_exit_42) == 42);
    check!("!WIFSIGNALED normal", !WIFSIGNALED(normal_exit_42));
    check!("!WIFSTOPPED normal", !WIFSTOPPED(normal_exit_42));
    check!("!WIFCONTINUED normal", !WIFCONTINUED(normal_exit_42));

    let normal_exit_0 = wait_status_exited(0) as i32;
    check!("WIFEXITED 0", WIFEXITED(normal_exit_0));
    check!("WEXITSTATUS 0", WEXITSTATUS(normal_exit_0) == 0);

    let normal_exit_255 = wait_status_exited(255) as i32;
    check!("WIFEXITED 255", WIFEXITED(normal_exit_255));
    check!("WEXITSTATUS 255", WEXITSTATUS(normal_exit_255) == 255);

    let killed_by_9 = wait_status_signalled(9) as i32;
    check!("!WIFEXITED signal", !WIFEXITED(killed_by_9));
    check!("WIFSIGNALED signal", WIFSIGNALED(killed_by_9));
    check!("WTERMSIG SIGKILL", WTERMSIG(killed_by_9) == 9);

    let killed_by_11 = wait_status_signalled(11) as i32;
    check!("WIFSIGNALED SIGSEGV", WIFSIGNALED(killed_by_11));
    check!("WTERMSIG SIGSEGV", WTERMSIG(killed_by_11) == 11);

    let stopped_sigstop = wait_status_stopped(19) as i32;
    check!("!WIFEXITED stopped", !WIFEXITED(stopped_sigstop));
    check!("!WIFSIGNALED stopped", !WIFSIGNALED(stopped_sigstop));
    check!("WIFSTOPPED stopped", WIFSTOPPED(stopped_sigstop));
    check!("WSTOPSIG SIGSTOP", WSTOPSIG(stopped_sigstop) == 19);
    check!("!WIFCONTINUED stopped", !WIFCONTINUED(stopped_sigstop));

    let continued = WAIT_STATUS_CONTINUED as i32;
    check!("WIFCONTINUED continued", WIFCONTINUED(continued));
    check!("!WIFEXITED continued", !WIFEXITED(continued));
    check!("!WIFSIGNALED continued", !WIFSIGNALED(continued));
    check!("!WIFSTOPPED continued", !WIFSTOPPED(continued));

    check!("WNOHANG value", WNOHANG == 1);
    check!("WUNTRACED value", WUNTRACED == 2);
    check!("WCONTINUED value", WCONTINUED == 8);

    (pass, fail)
}

//! Shell regression tests.
//!
//! Most cases feed `/bin/shell` a script down a pipe and assert on the exact
//! bytes it produces.  Two properties do the work: the output must be the
//! script's output and nothing else — no banner, no prompt, no SGR — and every
//! line must run exactly once, which a reader that over-reads cannot manage.
//!
//! One case instead drives the shell on a PTY, because the continuation prompt
//! only exists on the interactive path and the shape of that path (raw mode,
//! echo, a banner) makes exact-output matching impossible there.

// Links the lib crate's `_start` ELF entry point into the binary; without it
// the linker emits entry 0x0 and `do_exec` rejects the ELF.
use slopos_userland as _;

use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_userland::apps::shell::script::SCRIPT_LINE_MAX;
use slopos_userland::syscall::{SyscallError, core as sys_core, fs, process};

/// Bounded wait so a regressed shell fails the case rather than hanging the
/// whole harness.
const REAP_SPINS: usize = 5000;

/// Feeding and draining are interleaved on non-blocking descriptors: a script
/// larger than one pipe buffer would otherwise block the parent in `write`
/// while the child blocks in `write` on an output pipe nobody is reading.
fn run_script(script: &[u8]) -> Option<(Vec<u8>, i32)> {
    let (script_r, script_w) = fs::pipe().ok()?;
    let (out_r, out_w) = fs::pipe().ok()?;
    let script_r = script_r.into_raw();
    let script_w = script_w.into_raw();
    let out_r = out_r.into_raw();
    let out_w = out_w.into_raw();

    // No TASK_FLAG_FOREGROUND / TASK_FLAG_NEW_PGRP: this test must not move the
    // harness console's foreground process group.
    let actions = [
        process::clone_fd(script_r, 0),
        process::clone_fd(out_w, 1),
        process::clone_fd(2, 2),
    ];
    let tid = process::spawn_path_with_actions(
        b"/bin/shell",
        &[],
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
    );

    // After these closes the child holds the only copy of each end it reads
    // from or writes to, so both sides see EOF.
    let _ = fs::close_fd_raw(script_r);
    let _ = fs::close_fd_raw(out_w);

    if tid <= 0 {
        eprintln!("shell_script_test: spawn of /bin/shell returned {tid}");
        let _ = fs::close_fd_raw(script_w);
        let _ = fs::close_fd_raw(out_r);
        return None;
    }

    let _ = fs::set_fd_nonblocking(script_w);
    let _ = fs::set_fd_nonblocking(out_r);

    let mut written = 0usize;
    let mut script_open = true;
    let mut output = Vec::new();
    let mut buf = [0u8; 512];
    let mut idle = 0usize;

    loop {
        let mut progress = false;

        if script_open {
            match fs::write_slice(script_w, &script[written..]) {
                Ok(n) if n > 0 => {
                    written += n;
                    progress = true;
                }
                Err(SyscallError::EAGAIN) => {}
                _ => written = script.len(),
            }
            if written >= script.len() {
                // Closing the write end is what gives the shell its EOF.
                let _ = fs::close_fd_raw(script_w);
                script_open = false;
                progress = true;
            }
        }

        match fs::read_slice(out_r, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                output.extend_from_slice(&buf[..n]);
                progress = true;
            }
            Err(SyscallError::EAGAIN) => {}
            Err(_) => break,
        }

        if progress {
            idle = 0;
        } else {
            idle += 1;
            if idle > REAP_SPINS {
                eprintln!("shell_script_test: no progress draining /bin/shell");
                break;
            }
            sys_core::sleep_ms(1);
        }
    }
    if script_open {
        let _ = fs::close_fd_raw(script_w);
    }
    let _ = fs::close_fd_raw(out_r);

    let pid = tid as u32;
    for _ in 0..REAP_SPINS {
        if let Some(status) = process::wait_exit_code_nohang(pid) {
            return Some((output, status));
        }
        sys_core::sleep_ms(1);
    }
    eprintln!("shell_script_test: /bin/shell never exited");
    None
}

fn expect_output(name: &str, script: &[u8], want: &[u8]) -> bool {
    let Some((got, _)) = run_script(script) else {
        return false;
    };
    if got != want {
        eprintln!(
            "shell_script_test: {name}: output mismatch\n  want: {:?}\n  got:  {:?}",
            String::from_utf8_lossy(want),
            String::from_utf8_lossy(&got)
        );
        return false;
    }
    true
}

fn expect_status(name: &str, script: &[u8], want: i32) -> bool {
    let Some((_, status)) = run_script(script) else {
        return false;
    };
    if status != want {
        eprintln!("shell_script_test: {name}: status {status}, want {want}");
        return false;
    }
    true
}

fn script_output_is_exact() -> bool {
    expect_output(
        "script_output_is_exact",
        b"echo one\necho two\necho three\n",
        b"one\ntwo\nthree\n",
    )
}

/// Forty short lines span several 256-byte reads, so a reader that keeps a
/// fixed chunk and discards the rest loses most of them.
fn every_line_runs_once_in_order() -> bool {
    let mut script = Vec::new();
    let mut want = Vec::new();
    for i in 0..40u32 {
        script.extend_from_slice(b"echo L");
        want.extend_from_slice(b"L");
        for part in [i / 10, i % 10] {
            script.push(b'0' + part as u8);
            want.push(b'0' + part as u8);
        }
        script.push(b'\n');
        want.push(b'\n');
    }
    expect_output("every_line_runs_once_in_order", &script, &want)
}

/// The shell shares its script descriptor with the commands it runs, so it must
/// consume exactly the line it is about to execute.  `cat` here reads the
/// remainder of the script — which is only there if the shell left it.
fn no_overread_leaves_stdin_for_the_child() -> bool {
    expect_output(
        "no_overread_leaves_stdin_for_the_child",
        b"cat\npayload-a\npayload-b\n",
        b"payload-a\npayload-b\n",
    )
}

fn exit_status_is_last_command() -> bool {
    expect_status("exit_status_is_last_command/false", b"true\nfalse\n", 1)
        && expect_status("exit_status_is_last_command/true", b"false\ntrue\n", 0)
}

fn diagnostics_go_to_stderr() -> bool {
    expect_output(
        "diagnostics_go_to_stderr",
        b"echo before\nnosuchcmd\necho after\n",
        b"before\nafter\n",
    ) && expect_status("diagnostics_go_to_stderr/status", b"nosuchcmd\n", 127)
}

/// Refused, not truncated: a shortened command is a different command from the
/// one that was written.
fn over_long_line_is_diagnosed_not_truncated() -> bool {
    let mut script = Vec::new();
    script.extend_from_slice(b"echo ");
    script.resize(script.len() + SCRIPT_LINE_MAX + 64, b'x');
    script.push(b'\n');
    script.extend_from_slice(b"echo after\n");
    // The line after the refused one must be whole, not its tail.
    expect_output(
        "over_long_line_is_diagnosed_not_truncated",
        &script,
        b"after\n",
    )
}

fn comments_are_ignored() -> bool {
    expect_output(
        "comments_are_ignored",
        b"# a comment\necho ok   # trailing\necho a#b\n",
        b"ok\na#b\n",
    )
}

fn crlf_script_lines() -> bool {
    expect_output(
        "crlf_script_lines",
        b"echo one\r\necho two\r\n",
        b"one\ntwo\n",
    )
}

fn blank_lines_are_skipped() -> bool {
    expect_output("blank_lines_are_skipped", b"\n\n   \necho ok\n", b"ok\n")
}

fn exit_builtin_terminates_with_status() -> bool {
    expect_output(
        "exit_builtin_terminates_with_status",
        b"echo a\nexit 3\necho b\n",
        b"a\n",
    ) && expect_status(
        "exit_builtin_terminates_with_status",
        b"echo a\nexit 3\necho b\n",
        3,
    )
}

fn sequence_and_shortcircuit() -> bool {
    expect_output(
        "sequence_and_shortcircuit",
        b"echo hi; echo bye\nfalse && echo no\nfalse || echo yes\ntrue && echo also\n",
        b"hi\nbye\nyes\nalso\n",
    )
}

fn stderr_redirection() -> bool {
    expect_output(
        "stderr_redirection",
        b"nosuchcmd 2>/dev/null\necho ok\n",
        b"ok\n",
    )
}

/// An assignment prefix applies to one command and is put back afterwards; a
/// bare assignment sets a shell variable.
fn assignments_scope_correctly() -> bool {
    expect_output(
        "assignments_scope_correctly",
        b"FOO=bar\necho $FOO\nFOO=baz echo $FOO\n",
        b"bar\nbar\n",
    )
}

fn dash_c_runs_the_string() -> bool {
    let actions = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    // argv[0] is the program name; the shell's option parsing skips it.
    let arg0 = *b"shell\0";
    let arg_c = *b"-c\0";
    let arg_cmd = *b"exit 7\0";
    let argv = [arg0.as_ptr(), arg_c.as_ptr(), arg_cmd.as_ptr()];
    let tid = process::spawn_path_with_actions(
        b"/bin/shell",
        &argv,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
    );
    if tid <= 0 {
        eprintln!("shell_script_test: dash_c spawn returned {tid}");
        return false;
    }
    let status = process::wait_exit_code(tid as u32);
    if status != 7 {
        eprintln!("shell_script_test: `shell -c 'exit 7'` status {status}, want 7");
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// The POSIX grammar
// ---------------------------------------------------------------------------
//
// Related properties share one shell invocation: `MAX_PROCESSES` is 1024 and a
// run reaches ~170 before this test starts, so a spawn per assertion is a
// budget this test does not have.

fn branches() -> bool {
    expect_output(
        "branches",
        b"if true; then echo yes; else echo no; fi\n\
          if false; then echo yes; else echo no; fi\n\
          if false; then echo a; elif true; then echo b; else echo c; fi\n\
          if true\nthen\n  echo spanned\nfi\n\
          while\nfalse\ndo\n  echo never\ndone\n\
          if\ntrue\nthen\n  echo keyword-alone\nfi\n",
        b"yes\nno\nb\nspanned\nkeyword-alone\n",
    )
}

fn loops() -> bool {
    expect_output(
        "loops",
        b"i=0\nwhile [ $i -lt 3 ]; do echo w$i; i=$((i+1)); done\n\
          i=0\nuntil [ $i -ge 2 ]; do echo u$i; i=$((i+1)); done\n\
          for f in one two; do echo $f; done\n\
          set -- a b\nfor x; do echo p$x; done\n\
          for a in 1 2 3; do if [ $a = 2 ]; then continue; fi; echo c$a; done\n\
          for a in 1 2; do for b in x y; do echo b$a$b; break 2; done; done\n",
        b"w0\nw1\nw2\nu0\nu1\none\ntwo\npa\npb\nc1\nc3\nb1x\n",
    )
}

/// A `case` pattern in quotes is a literal, which is the whole reason the
/// expander tracks quoting per byte.
fn case_patterns() -> bool {
    expect_output(
        "case_patterns",
        b"for v in apple banana kiwi; do\n\
           case $v in\n\
             a*|b*) echo early ;;\n\
             *) echo late ;;\n\
           esac\n\
          done\n\
          case abc in \"*\") echo glob ;; *) echo other ;; esac\n\
          case '*' in \"*\") echo glob ;; *) echo other ;; esac\n\
          case x in (a) echo a ;; (x) echo paren ;; esac\n",
        b"early\nearly\nlate\nother\nglob\nparen\n",
    )
}

/// Functions, their positional parameters, and the `command` builtin that has
/// to see past them — the canonical wrapper otherwise calls itself until the
/// stack runs out.
fn functions_and_command() -> bool {
    expect_output(
        "functions_and_command",
        b"greet() { echo hi $1; return 3; }\ngreet world\necho $?\n\
          set -- outer\nf() { echo in:$1:$#; }\nf inner extra\necho out:$1:$#\n\
          cat() { command cat \"$@\"; }\necho body | cat\n\
          command echo '>'\n\
          command -v cd\ncommand -v ls\n",
        b"hi world\n3\nin:inner:2\nout:outer:1\nbody\n>\ncd\n/bin/ls\n",
    )
}

/// A subshell's `cd` and assignments do not reach the shell; a brace group's
/// do. That difference is the only thing distinguishing them.
fn subshells_groups_and_negation() -> bool {
    expect_output(
        "subshells_groups_and_negation",
        b"x=1\n(x=2)\necho $x\n{ x=3; }\necho $x\n! false\necho $?\n! true\necho $?\n",
        b"1\n3\n0\n1\n",
    )
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

/// The captured bytes are data: a `;` in them is a semicolon, not an
/// operator. And POSIX gives an assignment-only command the status of its last
/// substitution, which is what makes `x=$(cmd) || ...` mean anything.
fn command_substitution() -> bool {
    expect_output(
        "command_substitution",
        b"x=$(echo 'a;b')\necho \"$x\"\necho `echo back`\n\
          echo \"[$(printf 'x\\n\\n')]\"\n\
          y=$(false)\necho $?\ny=$(true)\necho $?\nz=plain\necho $?\n",
        b"a;b\nback\n[x]\n1\n0\n0\n",
    )
}

fn parameter_and_arithmetic_expansion() -> bool {
    expect_output(
        "parameter_and_arithmetic_expansion",
        b"unset u\nset=v\n\
          echo ${u:-fallback} ${set:-fallback}\n\
          echo ${u:+alt} ${set:+alt}\n\
          echo ${#set}\n\
          p=a/b/c.tar.gz\n\
          echo ${p##*/} ${p#*/} ${p%%.*} ${p%.*}\n\
          echo $((2 + 3 * 4)) $(((2 + 3) * 4)) $((7 / 2)) $((1 << 5))\n\
          n=6\necho $((n % 4)) $((n > 2)) $((n == 6 ? 10 : 20)) $((0 && 1/0 + 2))\n",
        b"fallback v\nalt\n1\nc.tar.gz b/c.tar.gz a/b/c a/b/c.tar\n14 20 3 32\n2 1 10 0\n",
    )
}

/// Quoting decides field splitting; `"$@"` keeps one field per parameter where
/// `"$*"` is one field; an empty expansion is a field only when quoted; and a
/// split that *produces* an empty field keeps it.
fn field_splitting() -> bool {
    expect_output(
        "field_splitting",
        b"count() { echo $#; }\n\
          set -- 'a b' c\ncount \"$@\"\ncount $@\ncount \"$*\"\n\
          v='x y'\ncount $v\ncount \"$v\"\n\
          e=\ncount $e\ncount \"$e\"\n\
          set -- ''\ncount \"$@\"\nset --\ncount \"$@\"\n\
          unset u\ncount ${u:-a b}\ncount \"${u:-a b}\"\n\
          IFS=:\nr=:\ncount $r\nr=a::b\ncount $r\n",
        b"2\n3\n1\n2\n1\n0\n1\n1\n0\n2\n1\n1\n3\n",
    )
}

// ---------------------------------------------------------------------------
// Here-documents and globbing
// ---------------------------------------------------------------------------

/// A quoted delimiter suppresses expansion; `<<-` strips leading tabs; two on
/// one line take their bodies in operator order; and a backslash is special
/// only before `$`, `` ` `` and `\`, exactly as inside double quotes.
fn heredocs() -> bool {
    expect_output(
        "heredocs",
        b"v=VAL\ncat <<END\nsaw $v\nEND\ncat <<'END'\nsaw $v\nEND\n\
          cat <<-END\n\tindented\n\tEND\n\
          cat <<A; cat <<B\nfirst\nA\nsecond\nB\n\
          cat <<END\na\\b \\$v \\\\ $v\nEND\n",
        b"saw VAL\nsaw $v\nindented\nfirst\nsecond\na\\b $v \\ VAL\n",
    )
}

/// An unmatched pattern is left exactly as written — there is no nullglob —
/// a quoted pattern never globs, a wildcard does not match a leading dot, and
/// a generated pathname is emitted only if it names an existing file.
fn globbing() -> bool {
    expect_output(
        "globbing",
        b"rm -rf /tmp/gt\nmkdir -p /tmp/gt/d1 /tmp/gt/d2\ncd /tmp/gt\n\
          : > a.txt\n: > b.txt\n: > c.log\n: > .hidden\n: > d2/there\n\
          echo *.txt\necho *.none\necho \"*.txt\"\necho ?.log\n\
          echo .h*\necho */there\necho */nowhere\n",
        b"a.txt b.txt\n*.none\n*.txt\nc.log\n.hidden\nd2/there\n*/nowhere\n",
    )
}

// ---------------------------------------------------------------------------
// The structural caps that used to exist
// ---------------------------------------------------------------------------

/// Twelve stages where eight was the ceiling, and a thousand-byte variable
/// where 256 was — a `CFLAGS` or a long `PATH` would have hit the latter.
fn structural_caps() -> bool {
    let mut script =
        b"echo deep | cat | cat | cat | cat | cat | cat | cat | cat | cat | cat | cat\nv=".to_vec();
    script.extend(core::iter::repeat_n(b'x', 1000));
    script.extend_from_slice(b"\necho ${#v}\n");
    expect_output("structural_caps", &script, b"deep\n1000\n")
}

/// A hundred arguments, where sixty-four was the ceiling. `cargo rustc` lines
/// routinely pass more.
fn a_command_takes_past_sixty_four_words() -> bool {
    let mut script = b"echo".to_vec();
    let mut want = Vec::new();
    for i in 0u32..100 {
        script.extend_from_slice(b" w");
        script.extend_from_slice(alloc_decimal(i).as_slice());
        if i > 0 {
            want.push(b' ');
        }
        want.extend_from_slice(b"w");
        want.extend_from_slice(alloc_decimal(i).as_slice());
    }
    script.push(b'\n');
    want.push(b'\n');
    expect_output("a_command_takes_past_sixty_four_words", &script, &want)
}

fn alloc_decimal(value: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut n = value;
    let mut digits = Vec::new();
    loop {
        digits.push(b'0' + (n % 10) as u8);
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend(digits.iter().rev());
    out
}

// ---------------------------------------------------------------------------
// Scripting builtins and descriptors
// ---------------------------------------------------------------------------

/// `read` must consume its line and not one byte more, or a loop reading a
/// file would lose every other line. `.` runs a file in this shell, so what it
/// defines survives.
fn scripting_builtins() -> bool {
    expect_output(
        "scripting_builtins",
        b"set -- a b c\nshift\necho $1 $#\nshift 2\necho $#\n\
          cmd='echo evaluated'\neval $cmd\n\
          while read a b; do echo \"[$a][$b]\"; done <<END\n1 2 3\nx y\nEND\n\
          cat > /tmp/sourced.sh <<'END'\nSOURCED=yes\nhelper() { echo helped; }\nEND\n\
          . /tmp/sourced.sh\necho $SOURCED\nhelper\n",
        b"b 2\n0\nevaluated\n[1][2 3]\n[x][y]\nyes\nhelped\n",
    )
}

/// The kernel hands out the lowest free descriptor, so the open for `3>` lands
/// on fd 3 itself — a `dup2(3, 3)` followed by a close leaves the command with
/// fd 3 shut rather than redirected. And closing a descriptor that is not open
/// is not an error, in the forked path as in the in-shell one.
fn redirection_descriptors() -> bool {
    expect_output(
        "redirection_descriptors",
        b"rm -f /tmp/fd3.txt\n{ echo viaThree >&3; } 3>/tmp/fd3.txt\ncat /tmp/fd3.txt\n\
          /bin/echo external 3>&-\necho builtin 3>&-\n",
        b"viaThree\nexternal\nbuiltin\n",
    )
}

/// An exported variable reaches a child; a plain shell variable does not. And
/// `unset NAME` names a variable, so a function of that name survives until no
/// such variable exists.
fn variables_and_unset() -> bool {
    expect_output(
        "variables_and_unset",
        b"PLAIN=p\nexport SHIPPED=s\nenv | grep -c '^PLAIN='\nenv | grep -c '^SHIPPED='\n\
          helper() { echo helper; }\nhelper=v\nunset helper\necho ${helper:-gone}\nhelper\n\
          unset helper\nhelper 2>/dev/null || echo really-gone\n",
        b"0\n1\ngone\nhelper\nreally-gone\n",
    )
}

/// `set -e` ends the script at the first failure, and does not fire on a
/// command whose status is being tested: a condition, any command of an
/// and-or list but the last, or a `!` pipeline.
fn errexit_stops_at_the_first_failure() -> bool {
    expect_output(
        "errexit_stops_at_the_first_failure",
        b"set -e\nif false; then echo no; fi\nfalse || echo tolerated\nfalse && echo no\n! true\necho survived\ntrue && false\necho unreachable\n",
        b"tolerated\nsurvived\n",
    )
}

/// `set -o NAME` is how a script both sets and clears an option.
fn set_o_names_the_same_options_as_the_letters() -> bool {
    expect_output(
        "set_o_names_the_same_options_as_the_letters",
        b"set -o noglob\necho *\nset +o noglob\nset -o errexit\nfalse\necho unreachable\n",
        b"*\n",
    )
}

/// A syntax error is diagnosed and the script goes on to the next command,
/// rather than the shell running a truncated reading of it.
fn a_syntax_error_does_not_run_anything() -> bool {
    expect_output(
        "a_syntax_error_does_not_run_anything",
        b"for; do echo no; done\necho after\n",
        b"after\n",
    )
}

// ---------------------------------------------------------------------------
// The interactive path
// ---------------------------------------------------------------------------

/// Zero-progress reads on the master that mean the shell stopped producing.
/// Bounded so a regressed shell fails rather than wedging the harness.
const PTY_IDLE_READS: usize = 20_000;

/// Type an unfinished `if` at an interactive shell, finish it on the next
/// lines, then run an external command.
///
/// `spanned` proves the continuation — `if true` alone would have failed and
/// `then echo spanned` alone is a syntax error — the PS2 prompt is what tells
/// the user it is waiting, and the absence of a stop report proves the forked
/// child could claim the terminal.
///
/// The shell is spawned from a child that first becomes the slave's session
/// and foreground group, as `/bin/terminal` does. That topology *is* the test:
/// a shell taking the terminal for itself instead of joining the session that
/// owns it leaves every command it forks in a background group.
fn the_interactive_prompt_continues_an_unfinished_command() -> bool {
    let Ok((master, _slave_num)) = process::openpty() else {
        eprintln!("shell_script_test: openpty failed");
        return false;
    };
    let master = master.into_raw();
    let Ok(slave) = fs::ioctl_tiocgptpeer(master) else {
        eprintln!("shell_script_test: tiocgptpeer failed");
        let _ = fs::close_fd_raw(master);
        return false;
    };
    let slave = slave.into_raw();

    let tid = process::fork();
    if tid < 0 {
        eprintln!("shell_script_test: fork for the tty owner failed");
        let _ = fs::close_fd_raw(master);
        let _ = fs::close_fd_raw(slave);
        return false;
    }
    if tid == 0 {
        let _ = fs::close_fd_raw(master);
        process::ignore_signal(slopos_abi::signal::SIGTTOU);
        let _ = process::setsid();
        let _ = fs::tiocsctty(slave);
        let _ = process::setpgid(0, 0);
        let pgid = process::getpgid(0);
        if pgid > 0 {
            let _ = fs::tcsetpgrp(slave, pgid as u32);
        }
        // fd 0/1/2 all on the slave is what makes the shell decide it is
        // interactive, which is the path under test.
        let actions = [
            process::clone_fd(slave, 0),
            process::clone_fd(slave, 1),
            process::clone_fd(slave, 2),
        ];
        let shell = process::spawn_path_with_actions(
            b"/bin/shell",
            &[],
            TaskPriority::Normal,
            TASK_FLAG_USER_MODE,
            &actions,
            0,
        );
        if shell <= 0 {
            sys_core::exit_with_code(1);
        }
        sys_core::exit_with_code(process::wait_exit_code(shell as u32));
    }
    let _ = fs::close_fd_raw(slave);

    let _ = fs::set_fd_nonblocking(master);
    let script: &[u8] = b"if true\nthen echo spanned\nfi\n/bin/echo external\nexit\n";
    let mut fed = 0usize;
    let mut seen = Vec::new();
    let mut idle = 0usize;
    let mut chunk = [0u8; 256];

    while idle < PTY_IDLE_READS {
        let mut progress = false;
        if fed < script.len() {
            match fs::write_slice(master, &script[fed..]) {
                Ok(n) if n > 0 => {
                    fed += n;
                    progress = true;
                }
                _ => {}
            }
        }
        match fs::read_slice(master, &mut chunk) {
            Ok(n) if n > 0 => {
                seen.extend_from_slice(&chunk[..n]);
                progress = true;
            }
            Err(SyscallError::EAGAIN) | Ok(_) => {}
            Err(_) => break,
        }
        if contains(&seen, b"external") && process::wait_exit_code_nohang(tid as u32).is_some() {
            break;
        }
        if progress {
            idle = 0;
        } else {
            idle += 1;
            sys_core::yield_now();
        }
    }

    let _ = process::kill(tid as u32, slopos_abi::signal::SIGKILL);
    let _ = fs::close_fd_raw(master);

    for marker in [b"spanned".as_slice(), b"external"] {
        if !contains(&seen, marker) {
            eprintln!(
                "shell_script_test: no {:?} in the interactive output; saw {:?}",
                String::from_utf8_lossy(marker),
                String::from_utf8_lossy(&seen)
            );
            return false;
        }
    }
    // Decisive: the command's echo and the job-table line for a stopped one
    // both contain `external`, so only the missing stop report says it ran.
    if contains(&seen, b"Stopped") {
        eprintln!(
            "shell_script_test: a foreground child was stopped; saw {:?}",
            String::from_utf8_lossy(&seen)
        );
        return false;
    }
    if !contains(&seen, b"> ") {
        eprintln!("shell_script_test: no PS2 prompt in the interactive output");
        return false;
    }
    true
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("script_output_is_exact", script_output_is_exact),
    (
        "every_line_runs_once_in_order",
        every_line_runs_once_in_order,
    ),
    (
        "no_overread_leaves_stdin_for_the_child",
        no_overread_leaves_stdin_for_the_child,
    ),
    ("exit_status_is_last_command", exit_status_is_last_command),
    ("diagnostics_go_to_stderr", diagnostics_go_to_stderr),
    (
        "over_long_line_is_diagnosed_not_truncated",
        over_long_line_is_diagnosed_not_truncated,
    ),
    ("comments_are_ignored", comments_are_ignored),
    ("crlf_script_lines", crlf_script_lines),
    ("blank_lines_are_skipped", blank_lines_are_skipped),
    (
        "exit_builtin_terminates_with_status",
        exit_builtin_terminates_with_status,
    ),
    ("sequence_and_shortcircuit", sequence_and_shortcircuit),
    ("stderr_redirection", stderr_redirection),
    ("assignments_scope_correctly", assignments_scope_correctly),
    ("dash_c_runs_the_string", dash_c_runs_the_string),
    ("branches", branches),
    ("loops", loops),
    ("case_patterns", case_patterns),
    ("functions_and_command", functions_and_command),
    (
        "subshells_groups_and_negation",
        subshells_groups_and_negation,
    ),
    ("command_substitution", command_substitution),
    (
        "parameter_and_arithmetic_expansion",
        parameter_and_arithmetic_expansion,
    ),
    ("field_splitting", field_splitting),
    ("heredocs", heredocs),
    ("globbing", globbing),
    ("structural_caps", structural_caps),
    (
        "a_command_takes_past_sixty_four_words",
        a_command_takes_past_sixty_four_words,
    ),
    ("scripting_builtins", scripting_builtins),
    ("redirection_descriptors", redirection_descriptors),
    ("variables_and_unset", variables_and_unset),
    (
        "errexit_stops_at_the_first_failure",
        errexit_stops_at_the_first_failure,
    ),
    (
        "set_o_names_the_same_options_as_the_letters",
        set_o_names_the_same_options_as_the_letters,
    ),
    (
        "a_syntax_error_does_not_run_anything",
        a_syntax_error_does_not_run_anything,
    ),
    (
        "the_interactive_prompt_continues_an_unfinished_command",
        the_interactive_prompt_continues_an_unfinished_command,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}

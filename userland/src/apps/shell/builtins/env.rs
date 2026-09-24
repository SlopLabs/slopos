use super::super::NL;
use super::super::display::{COLOR_ERROR_RED, shell_write, shell_write_idx};
use super::super::{env, funcs};

fn find_eq(data: &[u8]) -> Option<usize> {
    data.iter().position(|&b| b == b'=')
}

pub fn cmd_export(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        env::for_each_exported(|key, value| {
            shell_write(b"export ");
            shell_write(key);
            shell_write(b"=");
            shell_write(value);
            shell_write(NL.as_bytes());
        });
        return 0;
    }
    for bytes in argv.iter().take(argc as usize).skip(1) {
        if bytes.is_empty() {
            continue;
        }
        match find_eq(bytes) {
            Some(eq) => {
                let (key, value) = (&bytes[..eq], &bytes[eq + 1..]);
                if !env::is_name(key) {
                    shell_write_idx(b"export: invalid identifier\n", COLOR_ERROR_RED);
                    return 1;
                }
                env::set_exported(key, value);
            }
            // POSIX: this is how a later assignment's value gets exported.
            None => {
                if !env::is_name(bytes) {
                    shell_write_idx(b"export: invalid identifier\n", COLOR_ERROR_RED);
                    return 1;
                }
                env::export(bytes);
            }
        }
    }
    0
}

pub fn cmd_unset(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(b"unset: missing name\n", COLOR_ERROR_RED);
        return 1;
    }
    let mut functions_only = false;
    let mut variables_only = false;
    let mut first = 1usize;
    while first < argc as usize {
        match argv[first] {
            b"-f" => functions_only = true,
            b"-v" => variables_only = true,
            _ => break,
        }
        first += 1;
    }
    for bytes in argv.iter().take(argc as usize).skip(first) {
        if bytes.is_empty() {
            continue;
        }
        // With neither `-f` nor `-v` the name is a *variable*: `unset status`
        // must not take a helper function of that name with it.
        let removed = !functions_only && env::unset(bytes);
        if !variables_only && (functions_only || !removed) {
            funcs::undefine(bytes);
        }
    }
    0
}

pub fn cmd_env(_argc: i32, _argv: &[&[u8]]) -> i32 {
    env::for_each_exported(|key, value| {
        shell_write(key);
        shell_write(b"=");
        shell_write(value);
        shell_write(NL.as_bytes());
    });
    0
}

/// `set` — options, positional parameters, or a listing.
///
/// `set NAME=VALUE` is not POSIX and never was; it is kept because the shell
/// accepted it before this and a script in the tree may use it.
pub fn cmd_set(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        env::for_each(|key, value| {
            shell_write(key);
            shell_write(b"=");
            shell_write(value);
            shell_write(NL.as_bytes());
        });
        return 0;
    }

    let mut index = 1usize;
    let argc = argc as usize;
    while index < argc {
        let arg = argv[index];
        match arg.first() {
            Some(b'-') | Some(b'+') if arg.len() > 1 => {}
            _ => break,
        }
        // `set --` replaces the positional parameters with what follows, even
        // when that is nothing.
        if arg == b"--" {
            index += 1;
            let rest: Vec<Vec<u8>> = argv[index..argc].iter().map(|a| a.to_vec()).collect();
            super::super::args::replace_args(rest);
            return 0;
        }
        let on = arg[0] == b'-';
        // `-o NAME` / `+o NAME`, and a bare `-o` that lists the options: a
        // script probes the shell with both before it relies on either.
        if arg.len() == 2 && arg[1] == b'o' {
            index += 1;
            match argv.get(index) {
                Some(name) => {
                    if !set_option_by_name(name, on) {
                        shell_write_idx(b"set: unknown option name\n", COLOR_ERROR_RED);
                        return 2;
                    }
                    index += 1;
                }
                None => {
                    list_options();
                    return 0;
                }
            }
            continue;
        }
        for &letter in &arg[1..] {
            match letter {
                b'e' => funcs::set_errexit(on),
                b'u' => funcs::set_nounset(on),
                b'x' => funcs::set_xtrace(on),
                b'f' => funcs::set_noglob(on),
                _ => {
                    shell_write_idx(b"set: unknown option\n", COLOR_ERROR_RED);
                    return 2;
                }
            }
        }
        index += 1;
    }

    if index >= argc {
        return 0;
    }

    // A first operand that is an assignment keeps the shell's own historical
    // behaviour; anything else is a positional parameter list.
    if find_eq(argv[index]).is_some() {
        for bytes in argv.iter().take(argc).skip(index) {
            match find_eq(bytes) {
                Some(eq) if env::is_name(&bytes[..eq]) => env::set(&bytes[..eq], &bytes[eq + 1..]),
                _ => {
                    shell_write_idx(b"set: invalid identifier\n", COLOR_ERROR_RED);
                    return 1;
                }
            }
        }
        return 0;
    }

    let rest: Vec<Vec<u8>> = argv[index..argc].iter().map(|a| a.to_vec()).collect();
    super::super::args::replace_args(rest);
    0
}

/// The four options this shell has, by their long names.
const OPTION_NAMES: &[(&[u8], fn(bool), fn() -> bool)] = &[
    (b"errexit", funcs::set_errexit, funcs::errexit),
    (b"nounset", funcs::set_nounset, funcs::nounset),
    (b"xtrace", funcs::set_xtrace, funcs::xtrace),
    (b"noglob", funcs::set_noglob, funcs::noglob),
];

fn set_option_by_name(name: &[u8], on: bool) -> bool {
    match OPTION_NAMES.iter().find(|(n, _, _)| *n == name) {
        Some((_, set, _)) => {
            set(on);
            true
        }
        None => false,
    }
}

fn list_options() {
    for (name, _, get) in OPTION_NAMES {
        shell_write(name);
        shell_write(if get() { b"\ton\n" } else { b"\toff\n" });
    }
}

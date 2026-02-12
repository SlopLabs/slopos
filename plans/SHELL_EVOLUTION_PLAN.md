# SlopOS Shell Evolution Plan

> **Status**: Phase 1 Complete
> **Target**: Transform the shell from a command dispatcher into a real POSIX-inspired shell
> **Current**: `userland/src/apps/shell/` — modular directory (10 files), 12 commands, no history, no line editing, no pipes

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State Assessment](#2-current-state-assessment)
3. [Phase 0: Module Split](#3-phase-0-module-split)
4. [Phase 1: Core Shell Experience](#4-phase-1-core-shell-experience)
5. [Phase 2: Process Control & Pipes](#5-phase-2-process-control--pipes)
6. [Phase 3: Environment & Variables](#6-phase-3-environment--variables)
7. [Phase 4: New Builtins & File Tools](#7-phase-4-new-builtins--file-tools)
8. [Phase 5: Polish & Color](#8-phase-5-polish--color)
9. [Phase 6: Kernel-Side Unblocks](#9-phase-6-kernel-side-unblocks)
10. [Phase 7: Advanced Features](#10-phase-7-advanced-features)
11. [Blocked Features Reference](#11-blocked-features-reference)
12. [Syscall Inventory](#12-syscall-inventory)
13. [Architecture Reference](#13-architecture-reference)

---

## 1. Executive Summary

The SlopOS shell is currently a **command dispatcher with a text display** — not a shell. It has 12 hardcoded builtins, no history, no line editing beyond backspace, no pipes, no environment variables, no job control, and no way to run external programs with arguments.

Meanwhile the kernel already implements **pipes, fork, exec, signals, process groups, dup2, poll, lseek, kill** and more. The shell simply doesn't use them yet.

This plan has **7 phases**:
- **Phase 0**: Split the monolith into modules (prerequisite for everything)
- **Phase 1**: Core shell experience (history, line editing, tab completion, cd/pwd)
- **Phase 2**: Process control (fork+exec, pipes, redirection, jobs, signals)
- **Phase 3**: Environment variables, PATH resolution, variable expansion
- **Phase 4**: New builtins (file tools, system info, utilities)
- **Phase 5**: Visual polish (colors, prompt customization, better ls)
- **Phase 6**: Kernel-side unblocks (extend exec with argv, add chdir/getcwd, rename)
- **Phase 7**: Advanced features (scripting, globbing, aliases)

Phase 0–1 are pure userland. Phase 2 uses existing syscalls. Phase 6 requires kernel changes.

---

## 2. Current State Assessment

### What the Shell Has

| Capability | Details |
|-----------|---------|
| **Commands** | 12 builtins: `help`, `echo`, `clear`, `shutdown`, `reboot`, `info`, `sysinfo`, `ls`, `cat`, `write`, `mkdir`, `rm` |
| **Input** | Character-by-character via `tty::read_char()`, backspace only |
| **Output** | Dual-path: serial TTY + 640×480 compositor surface |
| **Scrollback** | 256 lines × 160 columns, circular buffer, Page Up/Down |
| **Parsing** | Whitespace-split, max 16 tokens × 64 bytes, no quoting |
| **Display** | Cell-based state, `SyncUnsafeCell` wrappers, single-threaded assumption |

### What the Shell Lacks

| Missing Feature | Impact |
|----------------|--------|
| Command history | Must retype every command |
| Line editing (arrows) | Only backspace works |
| Tab completion | Must type full paths |
| Working directory (cd/pwd) | All paths must be absolute |
| Pipes | Can't chain commands |
| Redirection | Can't redirect I/O to files |
| Background jobs | All commands block |
| Environment variables | No $PATH, $HOME, etc. |
| External command execution | Can only spawn 1 hardcoded program (sysinfo) |
| Quoting / escaping | `echo "hello world"` doesn't work |
| Signal handling | No Ctrl+C |
| Colors | Fixed gray-on-dark monochrome |

### File Structure (Current)

```
userland/src/apps/shell/
├── mod.rs              ← ShellState, REPL loop, shared constants
├── display.rs          ← DisplayState, scrollback, console_*/draw_* functions
├── input.rs            ← read_command_line(), key handling, PageUp/PageDown
├── parser.rs           ← shell_parse_line, is_space, normalize_path, u_streq_slice
├── surface.rs          ← Surface wrapper (init, draw, present_full)
├── buffers.rs          ← LINE_BUF, TOKEN_STORAGE, PATH_BUF, LIST_ENTRIES
└── builtins/
    ├── mod.rs          ← BuiltinEntry, BUILTINS table, find_builtin, print_kv
    ├── fs.rs           ← cmd_ls, cmd_cat, cmd_write, cmd_mkdir, cmd_rm
    └── system.rs       ← cmd_help, cmd_echo, cmd_clear, cmd_info, cmd_sysinfo, cmd_shutdown, cmd_reboot
userland/src/bin/shell.rs           ← 4-line entry point (unchanged)
```

---

## 3. Phase 0: Module Split

> **Prerequisite for all other phases.**
> **Kernel changes required**: None
> **Difficulty**: Medium (refactor, no new logic)

Split `shell.rs` into a module directory following the compositor pattern (`userland/src/apps/compositor/`).

### Target Structure

```
userland/src/apps/shell/
├── mod.rs              ← ShellState struct, REPL loop orchestration
├── display.rs          ← DisplayState, scrollback buffer, rendering functions
├── input.rs            ← Key handling, line buffer, cursor management
├── parser.rs           ← Tokenizer, command line parsing
├── builtins/
│   ├── mod.rs          ← BuiltinEntry table, dispatch function
│   ├── fs.rs           ← ls, cat, write, mkdir, rm (+ future: cp, mv, stat, touch, head, tail)
│   └── system.rs       ← help, echo, clear, info, sysinfo, shutdown, reboot (+ future: uptime, cpuinfo, free)
└── surface.rs          ← Compositor surface wrapper (present from current surface module)
```

### Tasks

- [x] **0.1** Create `userland/src/apps/shell/` directory
- [x] **0.2** Create `mod.rs` — extract `shell_user_main()` REPL loop + new `ShellState` struct holding all state
- [x] **0.3** Create `display.rs` — move `DisplayState`, `DISPLAY` static, all `console_*` functions, all `draw_*` functions, scrollback module
- [x] **0.4** Create `input.rs` — move keyboard handling logic (the inner input loop from `shell_user_main`), line buffer management, `KEY_PAGE_UP`/`KEY_PAGE_DOWN` handling
- [x] **0.5** Create `parser.rs` — move `shell_parse_line()`, `is_space()`, `normalize_path()`, `u_streq_slice()`, token constants (`SHELL_MAX_TOKENS`, etc.)
- [x] **0.6** Create `builtins/mod.rs` — move `BuiltinEntry` struct, `BUILTINS` table, `find_builtin()`, `BuiltinFn` type alias, `print_kv()` helper
- [x] **0.7** Create `builtins/fs.rs` — move `cmd_ls`, `cmd_cat`, `cmd_write`, `cmd_mkdir`, `cmd_rm`
- [x] **0.8** Create `builtins/system.rs` — move `cmd_help`, `cmd_echo`, `cmd_clear`, `cmd_info`, `cmd_sysinfo`, `cmd_shutdown`, `cmd_reboot`
- [x] **0.9** Create `surface.rs` — move the `surface` module (Surface wrapper, `init`, `draw`, `present_full`)
- [x] **0.10** Create `buffers.rs` — move the `buffers` module (LINE_BUF, TOKEN_STORAGE, PATH_BUF, LIST_ENTRIES)
- [x] **0.11** Update `userland/src/apps/mod.rs` to reference `shell` as a module directory instead of a single file
- [x] **0.12** Verify: `make build` compiles cleanly
- [x] **0.13** Verify: `make test` passes (384/384)
- [x] **0.14** Verify: `make boot VIDEO=1` — shell boots and all 12 commands still work

### Design Notes

The `ShellState` struct in `mod.rs` should own or reference all sub-module state:

```rust
pub struct ShellState {
    // Phase 0: just wiring
    // Phase 1: add history, input_state
    // Phase 2: add job_table
    // Phase 3: add env
}
```

Keep the existing `static DISPLAY: DisplayState` pattern for now — it can be refactored into `ShellState` later when the Cell-based approach is revisited.

---

## 4. Phase 1: Core Shell Experience

> **Makes the shell actually usable for daily interaction.**
> **Kernel changes required**: None (pure userland)
> **Difficulty**: Medium
> **Depends on**: Phase 0

### 1A: Command History

Store last N commands in a ring buffer. Up/Down arrows navigate.

- [x] **1A.1** Create `history.rs` module with `History` struct
  - Ring buffer: `[u8; MAX_HISTORY_ENTRIES * MAX_LINE_LENGTH]` (e.g., 64 entries × 256 bytes = 16KB)
  - `push(line: &[u8])` — add command to history (skip empty, skip duplicates of last entry)
  - `get(index: usize) -> Option<&[u8]>` — retrieve by index (0 = most recent)
  - `cursor: usize` — current browsing position
  - `navigate_up() -> Option<&[u8]>` — move cursor back, return line
  - `navigate_down() -> Option<&[u8]>` — move cursor forward, return line (or empty if at bottom)
  - `reset_cursor()` — reset to bottom on command submit
- [x] **1A.2** Integrate history into `input.rs` — detect Up/Down arrow key codes (0x82/0x83 from extended PS/2 scancodes)
- [x] **1A.3** On Up: replace current input line with history entry, redraw
- [x] **1A.4** On Down: navigate forward in history or restore original input
- [x] **1A.5** On Enter: push submitted line to history, reset cursor
- [x] **1A.6** Verify: type several commands, press Up/Down to navigate, submit historical command

### 1B: Line Editing

Support cursor movement within the input line. Currently only backspace works.

- [x] **1B.1** Add `cursor_pos: usize` to input state (currently `len` serves as implicit cursor-at-end)
- [x] **1B.2** Detect arrow key codes in `input.rs` (extended PS/2 scancodes mapped to 0x84/0x85/0x86/0x87):
  - Left: KEY_LEFT (0x84) → move cursor left
  - Right: KEY_RIGHT (0x85) → move cursor right
  - Home: KEY_HOME (0x86) → cursor to start
  - End: KEY_END (0x87) → cursor to end
- [x] **1B.3** Implement insert mode: typing a character at cursor_pos shifts remaining chars right
- [x] **1B.4** Implement delete-at-cursor: Backspace deletes char before cursor, Delete (KEY_DELETE 0x88) deletes char at cursor
- [x] **1B.5** Implement Ctrl shortcuts:
  - `Ctrl+A` (0x01) → cursor to beginning of line
  - `Ctrl+E` (0x05) → cursor to end of line
  - `Ctrl+K` (0x0B) → kill from cursor to end of line
  - `Ctrl+U` (0x15) → kill from cursor to beginning of line
  - `Ctrl+W` (0x17) → kill previous word
  - `Ctrl+L` (0x0C) → clear screen (same as `clear` command)
  - `Ctrl+C` (0x03) → cancel current input line (print new prompt)
  - `Ctrl+D` (0x04) → on empty line: exit. Otherwise: delete char at cursor
- [x] **1B.6** Update `shell_redraw_input()` to render cursor position visually (inverted fg/bg block cursor)
- [x] **1B.7** Verify: type `hello`, press Left×2, type `XX` → shows `helXXlo`, press Home, type `Y` → `Yhelxxlo`

### 1C: Working Directory (cd / pwd)

Track current working directory in shell state. Resolve relative paths against it.

- [x] **1C.1** Add `cwd: [u8; 256]` as global static in `mod.rs`, initialize to `b"/\0"`
- [x] **1C.2** Implement `cmd_cd` builtin:
  - `cd` (no arg) → go to `/` (no home directory concept yet)
  - `cd /abs/path` → set cwd directly (verify it exists with `fs::stat_path()`)
  - `cd relative` → resolve against cwd
  - `cd ..` → strip last path component
  - Error if target doesn't exist or isn't a directory
- [x] **1C.3** Implement `cmd_pwd` builtin: print current `cwd`
- [x] **1C.4** Update `normalize_path()` in `parser.rs`: if path doesn't start with `/`, prepend `cwd`
- [x] **1C.5** Update all FS commands (`ls`, `cat`, `write`, `mkdir`, `rm`) to resolve relative paths through updated `normalize_path()`
- [x] **1C.6** Update prompt to show cwd: `[/current/path] $ ` instead of just `$ `
- [x] **1C.7** Register `cd` and `pwd` in the builtins table
- [x] **1C.8** Verify: `cd /dev`, `pwd` → prints `/dev`, `ls` → lists `/dev` contents, `cd ..`, `pwd` → prints `/`

### 1D: Tab Completion

Complete file paths and command names on Tab press.

- [x] **1D.1** Detect Tab key (0x09) in input handler
- [x] **1D.2** Determine completion context:
  - If cursor is on the first token → complete against builtin names
  - If cursor is on subsequent tokens → complete against file/directory names
- [x] **1D.3** Extract prefix (text from last space/start to cursor)
- [x] **1D.4** For file completion:
  - Split prefix into directory part + filename prefix
  - Call `fs::list_dir()` on the directory
  - Filter entries that start with filename prefix
  - If exactly one match → insert the completion
  - If multiple matches → insert common prefix, show all matches on next line
  - Append `/` for directories, ` ` for files
- [x] **1D.5** For command completion:
  - Match against all `BUILTINS` names
  - (Program registry matching deferred to Phase 2 when external commands are supported)
- [x] **1D.6** Implement `insert_text()` + redraw — insert completion text and redraw input line
- [x] **1D.7** Verify: type `ca` + Tab → completes to `cat `, type `ls /d` + Tab → completes to `ls /dev`

### Phase 1 Gate

- [x] **GATE**: All 14 commands still work (12 original + cd + pwd)
- [x] **GATE**: History (Up/Down) implemented
- [x] **GATE**: Line editing (Left/Right/Home/End/Delete/Ctrl+A/E/K/U/W/L/C/D) implemented
- [x] **GATE**: cd/pwd works, prompt shows cwd
- [x] **GATE**: Tab completion works for builtins and file paths
- [x] **GATE**: `make test` passes (384/384)
- [x] **GATE**: `make build` compiles cleanly

---

## 5. Phase 2: Process Control & Pipes

> **The heart of a shell — running external programs and connecting them.**
> **Kernel changes required**: None (fork, exec, pipe, dup2, kill, waitpid, setpgid ALL exist)
> **Difficulty**: High
> **Depends on**: Phase 0, partially Phase 1
>
> **⚠️ KEY LIMITATION**: `SYSCALL_EXEC` (70) currently only accepts a path pointer — no argv/envp from userland.
> The kernel *has* the argv/envp stack-building code in `core/src/exec/mod.rs`, but the syscall interface doesn't
> expose it. This means external programs can be *launched* but can't receive command-line arguments until Phase 6
> extends the syscall. Pipes and redirection still work because they operate on file descriptors, not arguments.

### 2A: External Command Execution

Run programs not in the builtins table.

- [ ] **2A.1** Create `exec.rs` module with `execute_external(command: &[u8], args: &[&[u8]]) -> i32`
- [ ] **2A.2** Implement lookup order: builtins first → program registry → absolute path → error
- [ ] **2A.3** For external commands: `fork()` → in child: `exec(path)` → in parent: `waitpid(child_pid)`
- [ ] **2A.4** Handle exec failure in child (print error, `exit_with_code(127)`)
- [ ] **2A.5** Return child's exit code to shell
- [ ] **2A.6** Update main REPL dispatch: if `find_builtin()` returns None, try `execute_external()`
- [ ] **2A.7** Verify: type `file_manager` → spawns file manager window, shell waits for it (or returns immediately if it detaches)
- [ ] **2A.8** Note: without argv extension (Phase 6), external commands can't receive arguments

### 2B: I/O Redirection

Support `>`, `>>`, `<` operators.

- [ ] **2B.1** Extend `parser.rs` to recognize redirect operators:
  - `> file` → redirect stdout to file (truncate/create)
  - `>> file` → redirect stdout to file (append)
  - `< file` → redirect stdin from file
  - `2> file` → redirect stderr to file (future, when stderr exists)
- [ ] **2B.2** Create `Redirect` struct: `{ kind: RedirectKind, fd: i32, target_path: [u8; 128] }`
- [ ] **2B.3** Parser produces `ParsedCommand { tokens, redirects: [Redirect; 4] }`
- [ ] **2B.4** Before executing command:
  - Save original fds with `dup()`
  - Open redirect targets with `fs::open_path()`
  - Use `dup2()` to replace stdin/stdout
- [ ] **2B.5** After command completes: restore original fds
- [ ] **2B.6** For builtins: redirect `shell_write()` to write to the redirect fd instead of TTY+display
- [ ] **2B.7** For externals: set up redirects in child process before `exec()`
- [ ] **2B.8** Verify: `ls > /tmp/listing`, `cat /tmp/listing` → shows ls output. `echo hello >> /tmp/listing` appends.

### 2C: Pipes

Support `cmd1 | cmd2 | cmd3`.

- [ ] **2C.1** Extend parser to recognize `|` as pipe operator
- [ ] **2C.2** Parse pipeline: `Pipeline { commands: [ParsedCommand; MAX_PIPELINE_DEPTH] }` (max 4–8 stages)
- [ ] **2C.3** Implement `execute_pipeline()`:
  ```
  For N commands in pipeline:
    Create N-1 pipes via pipe()
    For each command i:
      fork()
      In child:
        If not first: dup2(pipe[i-1].read, STDIN)
        If not last:  dup2(pipe[i].write, STDOUT)
        Close all pipe fds
        exec(command) or run_builtin(command)
      In parent:
        Close pipe ends that parent doesn't need
    waitpid() for all children
  ```
- [ ] **2C.4** Handle pipeline of builtins: fork even for builtins when they're part of a pipeline (so their output goes through the pipe)
- [ ] **2C.5** Collect exit code from last command in pipeline
- [ ] **2C.6** Verify: `ls | cat` works, `echo hello | cat` works

### 2D: Job Control

Support background processes and job management.

- [ ] **2D.1** Create `jobs.rs` module with `JobTable` struct:
  ```rust
  struct Job {
      job_id: u16,
      pid: u32,
      pgid: u32,
      command: [u8; 128],  // original command text
      state: JobState,     // Running, Stopped, Done
  }
  struct JobTable {
      jobs: [Option<Job>; 16],
      next_id: u16,
  }
  ```
- [ ] **2D.2** Detect `&` at end of command line → launch as background job
- [ ] **2D.3** For background jobs: `fork()` + `exec()` but don't `waitpid()` — add to job table instead
- [ ] **2D.4** Use `setpgid()` to put background jobs in their own process group
- [ ] **2D.5** Implement `cmd_jobs` builtin: list all jobs with state (`[1] Running  ls &`, `[2] Done  sleep 1000`)
- [ ] **2D.6** Implement `cmd_fg` builtin: bring background job to foreground (`waitpid()` on it)
- [ ] **2D.7** Implement `cmd_bg` builtin: send SIGCONT to stopped job (future — needs signal infrastructure)
- [ ] **2D.8** On each prompt display: check for completed background jobs via non-blocking `waitpid()`, print `[N] Done  command`
- [ ] **2D.9** Implement `cmd_kill` builtin: `kill <pid>` or `kill %<job_id>` — uses `SYSCALL_KILL` (104) or `SYSCALL_TERMINATE_TASK` (69)
- [ ] **2D.10** Verify: `sysinfo &` → prints `[1] <pid>`, `jobs` → shows it, `kill %1` → terminates

### 2E: Signal Handling

Handle Ctrl+C and Ctrl+Z in the shell.

- [ ] **2E.1** Install SIGINT handler via `SYSCALL_RT_SIGACTION` (102)
- [ ] **2E.2** Ctrl+C (`0x03`) behavior:
  - If a foreground job is running: send SIGINT to its process group via `kill(pgid, SIGINT)`
  - If at prompt: cancel current input line, print fresh prompt
- [ ] **2E.3** Ctrl+Z (`0x1A`) behavior (future — needs SIGTSTP/SIGCONT support):
  - Suspend foreground job, add to job table as Stopped
- [ ] **2E.4** Ctrl+D (`0x04`): on empty line → exit shell. Otherwise → delete char at cursor (done in 1B.5)
- [ ] **2E.5** Verify: run a long operation, Ctrl+C interrupts it, shell shows new prompt

### 2F: Process Status Command

- [ ] **2F.1** Implement `cmd_ps` builtin:
  - Use `SYSCALL_SYS_INFO` for task counts
  - Use `SYSCALL_ENUMERATE_WINDOWS` to list windowed tasks with names
  - Show PID, state, name for each visible process
- [ ] **2F.2** Implement `cmd_wait` builtin: `wait <pid>` — block on `waitpid(pid)`
- [ ] **2F.3** Implement `cmd_exec` builtin: `exec <path>` — replace shell with program via `SYSCALL_EXEC`

### Phase 2 Gate

- [ ] **GATE**: External programs can be launched from shell
- [ ] **GATE**: `>` and `<` redirection works
- [ ] **GATE**: `|` pipes work between at least 2 commands
- [ ] **GATE**: `&` launches background jobs, `jobs` lists them, `kill` terminates them
- [ ] **GATE**: Ctrl+C cancels current input or signals foreground job
- [ ] **GATE**: `make test` passes

---

## 6. Phase 3: Environment & Variables

> **Give the shell a memory — variables, PATH, and expansion.**
> **Kernel changes required**: None (envp already passed on stack, just not parsed by shell)
> **Difficulty**: Medium
> **Depends on**: Phase 0

### 3A: Environment Variable Store

- [ ] **3A.1** Create `env.rs` module with `Environment` struct:
  ```rust
  struct EnvEntry {
      key: [u8; 64],
      value: [u8; 256],
      key_len: u8,
      value_len: u16,
      active: bool,
  }
  struct Environment {
      entries: [EnvEntry; 64],
      count: usize,
  }
  ```
- [ ] **3A.2** Implement `get(key: &[u8]) -> Option<&[u8]>`, `set(key: &[u8], value: &[u8])`, `unset(key: &[u8])`
- [ ] **3A.3** Initialize default variables on shell start:
  - `PATH=/bin:/sbin`
  - `SHELL=/bin/shell`
  - `HOME=/`
  - `USER=root`
  - `PS1=[\w] $ ` (or similar)
  - `TERM=slopos`
- [ ] **3A.4** If kernel passes envp on stack (via crt0), parse and import those entries
- [ ] **3A.5** Implement `cmd_export` builtin: `export KEY=VALUE` — set variable
- [ ] **3A.6** Implement `cmd_unset` builtin: `unset KEY` — remove variable
- [ ] **3A.7** Implement `cmd_env` builtin: list all environment variables
- [ ] **3A.8** Implement `cmd_set` builtin: alias for env (show all), or `set KEY=VALUE` (local variable)
- [ ] **3A.9** Verify: `export FOO=bar`, `env` → shows FOO=bar, `unset FOO`, `env` → FOO gone

### 3B: PATH Resolution

Look up commands in PATH directories instead of requiring absolute paths.

- [ ] **3B.1** Implement `resolve_command(name: &[u8], env: &Environment) -> Option<[u8; 256]>`:
  - If name contains `/` → use as-is (absolute or relative path)
  - Otherwise: split `$PATH` by `:`, for each directory:
    - Construct `dir/name`
    - Check if file exists via `fs::stat_path()` or `fs::open_path()`
    - If found → return full path
  - Fall back to program registry lookup
- [ ] **3B.2** Integrate into command dispatch: builtin lookup → PATH resolution → program registry → error
- [ ] **3B.3** Verify: with `PATH=/bin`, type `shell` → resolves to `/bin/shell`, type `nonexistent` → `command not found`

### 3C: Variable Expansion

Expand `$VAR` and `${VAR}` in command lines before parsing.

- [ ] **3C.1** Implement expansion pass in parser (runs before tokenization):
  - `$VAR` → replaced with env value (variable name = alphanumeric + underscore, terminated by non-alnum)
  - `${VAR}` → explicit delimiters for variable name
  - `$?` → exit code of last command
  - `$$` → shell's own PID (via `SYSCALL_GETPID`)
  - `$!` → PID of last background job
  - `\\$` → literal `$` (escaped)
- [ ] **3C.2** Handle undefined variables: expand to empty string (like bash default)
- [ ] **3C.3** Implement `last_exit_code: i32` in `ShellState` — updated after every command
- [ ] **3C.4** Verify: `export X=hello`, `echo $X` → prints `hello`, `echo ${X}world` → prints `helloworld`

### 3D: Quoting

Support double and single quotes in command arguments.

- [ ] **3D.1** Update parser to handle:
  - `"double quotes"` → preserves spaces, expands variables
  - `'single quotes'` → preserves spaces, NO variable expansion (literal)
  - `\"` → escaped double quote inside double quotes
  - `\\` → escaped backslash
- [ ] **3D.2** Update `shell_parse_line()` to be a state machine: `Normal | InDoubleQuote | InSingleQuote`
- [ ] **3D.3** Verify: `echo "hello world"` → prints `hello world` (one arg), `echo 'no $expansion'` → prints `no $expansion`

### Phase 3 Gate

- [ ] **GATE**: `export`, `unset`, `env` commands work
- [ ] **GATE**: PATH resolution works (type program name without absolute path)
- [ ] **GATE**: `$VAR` expansion works in commands
- [ ] **GATE**: `$?` shows last exit code
- [ ] **GATE**: Double and single quoting works
- [ ] **GATE**: `make test` passes

---

## 7. Phase 4: New Builtins & File Tools

> **Expand the command set to cover common operations.**
> **Kernel changes required**: None
> **Difficulty**: Low per command
> **Depends on**: Phase 0 (some commands benefit from Phase 1 cwd, Phase 3 env)

### 4A: File System Commands

- [ ] **4A.1** `stat <path>` — show file type, size, inode. Uses `SYSCALL_FS_STAT` (18)
- [ ] **4A.2** `touch <path>` — create empty file. Uses `fs::open_path()` with `CREAT`, then `close()`
- [ ] **4A.3** `cp <src> <dst>` — copy file. Open src (read), open dst (write+creat), read/write loop, close both
- [ ] **4A.4** `mv <src> <dst>` — move file. Copy + rm source (no atomic rename syscall yet, see Phase 6)
- [ ] **4A.5** `head <file> [n]` — show first N lines (default 10). Read file, count newlines
- [ ] **4A.6** `tail <file> [n]` — show last N lines. Read file, buffer last N lines
- [ ] **4A.7** `wc <file>` — count lines, words, characters
- [ ] **4A.8** `cat` enhancement: support multiple files (`cat file1 file2`), handle `-` as stdin (future)
- [ ] **4A.9** `ls` enhancement: show file sizes, mark directories with `/`, sort alphabetically
- [ ] **4A.10** `hexdump <file> [n]` — show first N bytes as hex + ASCII. Read file, format output
- [ ] **4A.11** `tee <file>` — read stdin, write to both stdout and file (useful with pipes, Phase 2)
- [ ] **4A.12** `diff <file1> <file2>` — basic line-by-line comparison (stretch goal)

### 4B: System Information Commands

- [ ] **4B.1** `uptime` — show system uptime. Uses `SYSCALL_GET_TIME_MS` (39), format as hours:minutes:seconds
- [ ] **4B.2** `cpuinfo` — show CPU count, current CPU. Uses `SYSCALL_GET_CPU_COUNT` (80) + `SYSCALL_GET_CURRENT_CPU` (81)
- [ ] **4B.3** `free` — show memory stats (total/free/allocated pages, convert to KB/MB). Uses `SYSCALL_SYS_INFO` (22)
- [ ] **4B.4** `time <command>` — execute command and print elapsed time. Wraps any command with `get_time_ms()` before/after
- [ ] **4B.5** `date` — print current uptime as date-ish format (no RTC, so relative time)
- [ ] **4B.6** `uname` — print system name (SlopOS), version, architecture (x86_64)
- [ ] **4B.7** `whoami` — print `root` (uses `SYSCALL_GETUID`, always 0)

### 4C: Utility Commands

- [ ] **4C.1** `sleep <ms>` — sleep for N milliseconds. Uses `SYSCALL_SLEEP_MS` (5)
- [ ] **4C.2** `true` — always returns exit code 0
- [ ] **4C.3** `false` — always returns exit code 1
- [ ] **4C.4** `seq <start> <end>` — print numbers from start to end
- [ ] **4C.5** `yes [string]` — repeatedly print string (default "y") until killed
- [ ] **4C.6** `random [max]` — print random number (0..max). Uses `SYSCALL_RANDOM_NEXT` (12)
- [ ] **4C.7** `roulette` — spin the Wheel of Fate from the command line! Uses `SYSCALL_ROULETTE` (4). Award W/L accordingly
- [ ] **4C.8** `wl` — show current W/L balance (ties into the W/L currency system)

### Phase 4 Gate

- [ ] **GATE**: At least 10 new commands implemented and working
- [ ] **GATE**: `stat`, `touch`, `cp`, `uptime`, `free` work correctly
- [ ] **GATE**: `make test` passes

---

## 8. Phase 5: Polish & Color

> **Make the shell visually appealing and comfortable to use.**
> **Kernel changes required**: None (pure rendering changes)
> **Difficulty**: Low-Medium
> **Depends on**: Phase 0, Phase 1

### 5A: ANSI Color Support

- [ ] **5A.1** Define color palette constants in display module:
  - Directory blue (`Color32(0x5C9E_D6FF)`)
  - Executable green (`Color32(0x98C3_79FF)`)
  - Error red (`Color32(0xE06C_75FF)`)
  - Warning yellow (`Color32(0xE5C0_7BFF)`)
  - Prompt accent (`Color32(0xC678_DDFF)`)
  - Comment gray (`Color32(0x5C63_70FF)`)
- [ ] **5A.2** Extend scrollback to store per-character color (add color attribute array parallel to text array)
  - Option A: 4-bit color index (16 colors, 1 byte per char) → +40KB
  - Option B: fg color index only (8 colors, 3 bits) packed into high bits of char byte
- [ ] **5A.3** Update `draw_row_from_scrollback()` to use per-character color
- [ ] **5A.4** Add `shell_write_colored(text: &[u8], fg: Color32)` function

### 5B: Colored Output

- [ ] **5B.1** Colored `ls`: directories in blue, executables in green (check file type from `UserFsEntry`)
- [ ] **5B.2** Colored prompt: `[path]` in blue, `$` in accent color, `#` if root
- [ ] **5B.3** Colored error messages: `No such file or directory` in red
- [ ] **5B.4** Colored `help`: command names in green, descriptions in default color
- [ ] **5B.5** Colored `info`/`free`: labels in gray, values in white, warnings in yellow

### 5C: Prompt Customization

- [ ] **5C.1** Parse `$PS1` environment variable for prompt format:
  - `\w` → current working directory
  - `\u` → username (always `root`)
  - `\h` → hostname (always `sloptopia`)
  - `\$` → `$` for normal user, `#` for root
  - `\t` → current time
  - `\n` → newline
- [ ] **5C.2** Default PS1: `\u@\h:\w\$ ` → `root@sloptopia:/path$ `
- [ ] **5C.3** Verify: `export PS1="[\w] # "` → changes prompt

### 5D: Visual Improvements

- [ ] **5D.1** Blinking cursor (toggle cursor char on timer tick)
- [ ] **5D.2** Selection highlighting (Shift+Arrow to select text — stretch goal)
- [ ] **5D.3** Smoother scrollback scroll (render partial lines at boundaries)
- [ ] **5D.4** Welcome banner: SlopOS ASCII art + version + W/L balance on shell start

### Phase 5 Gate

- [ ] **GATE**: `ls` output shows directories in color
- [ ] **GATE**: Prompt shows cwd with color
- [ ] **GATE**: Error messages are red
- [ ] **GATE**: `make test` passes

---

## 9. Phase 6: Kernel-Side Unblocks

> **Extend kernel syscalls to remove blockers for shell features.**
> **Kernel changes required**: YES — these are kernel patches
> **Difficulty**: Medium-High
> **Depends on**: Phase 2 (to demonstrate the need)

### 6A: Extend SYSCALL_EXEC with argv/envp

> **This is the single most impactful kernel change for the shell.**

The kernel already builds the user stack with argc/argv/envp in `core/src/exec/mod.rs`, and `crt0.rs` already parses them. The syscall just doesn't accept them from userland.

- [ ] **6A.1** Extend `SYSCALL_EXEC` ABI:
  - `rdi` (arg0): path pointer
  - `rsi` (arg1): argv array pointer (null-terminated array of null-terminated strings)
  - `rdx` (arg2): envp array pointer (null-terminated array of `KEY=VALUE\0` strings)
  - Backward compat: if argv == 0 and envp == 0, behave as today (path only)
- [ ] **6A.2** Update kernel handler in `core/src/syscall/` to read argv/envp from user memory
- [ ] **6A.3** Update `core/src/exec/mod.rs` to use provided argv/envp when building user stack
- [ ] **6A.4** Add userland wrapper: `pub fn execve(path: &[u8], argv: &[*const u8], envp: &[*const u8]) -> !`
- [ ] **6A.5** Update shell's `exec.rs` to pass parsed tokens as argv
- [ ] **6A.6** Verify: `echo hello world` as external program receives `argv = ["echo", "hello", "world"]`

### 6B: Extend SYSCALL_SPAWN_PATH with argv

Same treatment for `SYSCALL_SPAWN_PATH` (64):

- [ ] **6B.1** Add argv pointer + count to spawn syscall arguments
- [ ] **6B.2** Update kernel spawn handler to pass argv to new process
- [ ] **6B.3** Update userland wrapper
- [ ] **6B.4** Verify: programs spawned from shell receive command-line arguments

### 6C: Add SYSCALL_CHDIR / SYSCALL_GETCWD

Kernel-managed working directory so child processes inherit it.

- [ ] **6C.1** Add `cwd: [u8; 256]` to task struct (or per-process FS context)
- [ ] **6C.2** Implement `SYSCALL_CHDIR` — validate path, update task cwd
- [ ] **6C.3** Implement `SYSCALL_GETCWD` — copy task cwd to user buffer
- [ ] **6C.4** Update `fs::open_path()` kernel side to resolve relative paths against task cwd
- [ ] **6C.5** Child processes inherit parent's cwd on fork/spawn
- [ ] **6C.6** Add userland wrappers and update shell's `cmd_cd` to use kernel `chdir()` instead of tracking in userland
- [ ] **6C.7** Verify: `cd /dev`, spawn child process, child sees cwd as `/dev`

### 6D: Add SYSCALL_RENAME

Atomic file rename for `mv` command.

- [ ] **6D.1** Define `SYSCALL_RENAME` in ABI (pick next available number)
- [ ] **6D.2** Implement in VFS layer: `vfs_rename(old_path, new_path)`
- [ ] **6D.3** Implement in ext2 driver: unlink old entry, create new entry pointing to same inode
- [ ] **6D.4** Implement in ramfs
- [ ] **6D.5** Add userland wrapper + update `cmd_mv` to use rename instead of cp+rm
- [ ] **6D.6** Verify: `mv /tmp/a /tmp/b` renames atomically

### Phase 6 Gate

- [ ] **GATE**: External programs receive argv
- [ ] **GATE**: `cd` works at kernel level, children inherit cwd
- [ ] **GATE**: `mv` uses rename syscall
- [ ] **GATE**: `make test` passes

---

## 10. Phase 7: Advanced Features

> **Long-term enhancements that make SlopOS shell approach a real Unix shell.**
> **Kernel changes required**: Some (shebang support)
> **Difficulty**: High
> **Depends on**: Phases 1–3 minimum

### 7A: Aliases

- [ ] **7A.1** Add alias table to ShellState: `aliases: [(key: [u8; 32], value: [u8; 256]); 32]`
- [ ] **7A.2** Implement `cmd_alias`: `alias ll="ls -l"`, `alias` (list all)
- [ ] **7A.3** Implement `cmd_unalias`: `unalias ll`
- [ ] **7A.4** Expand aliases before parsing (first token substitution)
- [ ] **7A.5** Prevent infinite alias recursion (max 10 expansions)

### 7B: Globbing

- [ ] **7B.1** Implement `*` wildcard expansion: `ls *.txt` → list dir, match pattern, expand to matching filenames
- [ ] **7B.2** Implement `?` single-char wildcard
- [ ] **7B.3** Expansion runs after variable expansion, before tokenization
- [ ] **7B.4** If no match: pass pattern literally (like bash default)
- [ ] **7B.5** Verify: create `/tmp/a.txt`, `/tmp/b.txt`, `ls /tmp/*.txt` → lists both

### 7C: Command Chaining

- [ ] **7C.1** `cmd1 && cmd2` — run cmd2 only if cmd1 succeeds (exit code 0)
- [ ] **7C.2** `cmd1 || cmd2` — run cmd2 only if cmd1 fails (exit code != 0)
- [ ] **7C.3** `cmd1 ; cmd2` — run both regardless of exit codes
- [ ] **7C.4** Parser recognizes `&&`, `||`, `;` as command separators

### 7D: Here Documents & Here Strings

- [ ] **7D.1** `cmd << EOF ... EOF` — redirect multi-line input to command (stretch goal)
- [ ] **7D.2** `cmd <<< "string"` — redirect string as stdin (stretch goal)

### 7E: Shell Scripting

- [ ] **7E.1** `source <file>` / `. <file>` builtin: read file, execute each line as a command
- [ ] **7E.2** Basic conditionals: `if cmd; then ...; fi` (stretch — needs significant parser work)
- [ ] **7E.3** Basic loops: `for x in a b c; do echo $x; done` (stretch)
- [ ] **7E.4** Shebang support in kernel: if `exec()` encounters `#!/bin/shell`, re-exec with shell as interpreter

### 7F: History Persistence

- [ ] **7F.1** On shell exit: write history to `/home/.shell_history` via `fs::open_path()` + `fs::write_slice()`
- [ ] **7F.2** On shell start: read history file and populate ring buffer
- [ ] **7F.3** `history` command: list all history entries with numbers
- [ ] **7F.4** `!N` — execute history entry N
- [ ] **7F.5** `!!` — execute last command
- [ ] **7F.6** Ctrl+R — reverse search history (stretch)

### Phase 7 Gate

- [ ] **GATE**: `alias` / `unalias` work
- [ ] **GATE**: `*.txt` globbing expands correctly
- [ ] **GATE**: `&&` and `||` chaining works
- [ ] **GATE**: `source` command reads and executes files
- [ ] **GATE**: History persists across shell restarts
- [ ] **GATE**: `make test` passes

---

## 11. Blocked Features Reference

Features that **cannot be implemented** without significant new kernel work beyond Phase 6:

| Feature | Blocker | Kernel Work Required |
|---------|---------|---------------------|
| Pseudo-terminals (PTY) | No PTY driver | Implement PTY master/slave device pair in `drivers/` |
| Symlinks | No symlink support in VFS/ext2 | Add symlink inode type, readlink, follow logic |
| File permissions | No chmod/chown/access syscalls | Add permission model, check in VFS |
| Networking (curl, ping, etc.) | No socket syscalls | Entire TCP/IP stack + socket layer |
| strace / debugging | No ptrace syscall | Implement ptrace infrastructure |
| Resource limits (ulimit) | No rlimit syscalls | Add per-process resource accounting |
| Multiple users (su, login) | No user database, no /etc/passwd | User management subsystem |
| Terminal multiplexer (screen/tmux) | Needs PTY support | Depends on PTY driver |

---

## 12. Syscall Inventory

Syscalls the shell currently uses vs. syscalls it should use after all phases:

### Currently Used (7 syscalls)

| Syscall | Used By |
|---------|---------|
| `SYSCALL_WRITE` (2) | `tty::write()` — serial output |
| `SYSCALL_READ_CHAR` (25) | `tty::read_char()` — keyboard input |
| `SYSCALL_YIELD` (0) | Yield on empty input |
| `SYSCALL_SYS_INFO` (22) | `info` command |
| `SYSCALL_FS_*` (14-21) | `ls`, `cat`, `write`, `mkdir`, `rm` |
| `SYSCALL_SPAWN_PATH` (64) | `sysinfo` command |
| `SYSCALL_HALT`/`REBOOT` (23/85) | `shutdown`, `reboot` |

### After All Phases (~35 syscalls)

| Syscall | New Usage |
|---------|-----------|
| `SYSCALL_FORK` (72) | Pipe chains, external commands |
| `SYSCALL_EXEC` (70) | External command execution |
| `SYSCALL_WAITPID` (68) | Wait for children |
| `SYSCALL_PIPE` (110) | Pipe operator |
| `SYSCALL_PIPE2` (111) | Pipe with flags |
| `SYSCALL_DUP` (95) | Save/restore fds |
| `SYSCALL_DUP2` (96) | I/O redirection |
| `SYSCALL_KILL` (104) | Signal processes, job control |
| `SYSCALL_RT_SIGACTION` (102) | Signal handling |
| `SYSCALL_RT_SIGPROCMASK` (103) | Signal masking |
| `SYSCALL_SETPGID` (113) | Job control |
| `SYSCALL_GETPGID` (114) | Job control |
| `SYSCALL_GETPID` (86) | `$$` variable |
| `SYSCALL_GETPPID` (87) | Process info |
| `SYSCALL_TERMINATE_TASK` (69) | Kill command |
| `SYSCALL_GET_TIME_MS` (39) | `time`, `uptime` |
| `SYSCALL_GET_CPU_COUNT` (80) | `cpuinfo` |
| `SYSCALL_GET_CURRENT_CPU` (81) | `cpuinfo` |
| `SYSCALL_RANDOM_NEXT` (12) | `random`, `roulette` |
| `SYSCALL_ROULETTE` (4) | `roulette` command |
| `SYSCALL_SLEEP_MS` (5) | `sleep` command |
| `SYSCALL_FS_STAT` (18) | `stat`, PATH resolution |
| `SYSCALL_LSEEK` (99) | `head`, `tail`, large file reads |
| `SYSCALL_POLL` (108) | Event-driven input loop (replace busy-yield) |
| `SYSCALL_ENUMERATE_WINDOWS` (30) | `ps` command |
| `SYSCALL_EXIT` (1) | Ctrl+D, `exit` command |
| `SYSCALL_GETUID` (88) | `whoami` |
| `SYSCALL_CHDIR` (new) | `cd` — Phase 6 |
| `SYSCALL_GETCWD` (new) | `pwd` — Phase 6 |
| `SYSCALL_RENAME` (new) | `mv` — Phase 6 |

---

## 13. Architecture Reference

### Module Dependency Graph (After Split)

```
mod.rs (ShellState, REPL loop)
├── display.rs          (DisplayState, scrollback, rendering)
│   └── surface.rs      (compositor surface wrapper)
├── input.rs            (line editing, key handling)
│   └── history.rs      (command history ring buffer)
├── parser.rs           (tokenizer, quoting, pipes/redirects)
├── exec.rs             (external command execution, pipes, redirects)
│   └── jobs.rs         (job table, background process tracking)
├── env.rs              (environment variables, PATH resolution)
├── buffers.rs          (static buffer management)
└── builtins/
    ├── mod.rs           (dispatch table)
    ├── fs.rs            (file commands)
    ├── system.rs        (system commands)
    ├── process.rs       (job/process commands)
    └── env.rs           (environment commands)
```

### Data Flow

```
Keyboard → input.rs (line editing, history)
                ↓
         parser.rs (tokenize, expand variables, handle quotes)
                ↓
         mod.rs (dispatch: builtin? external? pipeline?)
           ↓                    ↓
    builtins/*.rs         exec.rs (fork/exec/pipe)
           ↓                    ↓
    display.rs ←── output ──→ jobs.rs (track background)
           ↓
    surface.rs → compositor
```

### Relation to Compositor Pattern

| Compositor Module | Shell Equivalent | Responsibility |
|-------------------|-----------------|----------------|
| `mod.rs` (WindowManager) | `mod.rs` (ShellState) | Central orchestrator + state |
| `renderer.rs` | `display.rs` | All visual output |
| `input.rs` | `input.rs` | Event handling + state |
| `output.rs` | `surface.rs` | Buffer abstraction |
| `surface_cache.rs` | `buffers.rs` | Resource management |
| `taskbar.rs` | `builtins/` | Feature implementations |
| `hover.rs` | `history.rs` | Stateful tracking |

---

## Progress Tracking

| Phase | Status | Tasks | Done | Blocked |
|-------|--------|-------|------|---------|
| **Phase 0**: Module Split | **Complete** | 14 | 14 | — |
| **Phase 1**: Core Shell | **Complete** | 28 | 28 | — |
| **Phase 2**: Process Control | Not Started | 30 | 0 | — |
| **Phase 3**: Environment | Not Started | 17 | 0 | — |
| **Phase 4**: New Builtins | Not Started | 23 | 0 | — |
| **Phase 5**: Polish & Color | Not Started | 13 | 0 | Phase 1 |
| **Phase 6**: Kernel Unblocks | Not Started | 18 | 0 | Phase 2 |
| **Phase 7**: Advanced | Not Started | 20 | 0 | Phases 1-3 |
| **Total** | | **163** | **14** | |

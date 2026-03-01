# SlopOS Netcat (`nc`) Implementation Plan

> **Status**: ✅ Phase A complete — Phase B blocked on TCP stack (networking Phase 5)
> **Target**: Real, long-term `nc` command that grows with the networking stack
> **Scope**: UDP client/listener (now), TCP (Phase 5), bidirectional I/O (Phase 6), scanning (Phase 6+)
> **Design principle**: Build the argument parser, config model, and output helpers once; add protocol modes as the stack evolves

---

## Table of Contents

1. [Motivation](#motivation)
2. [Available Primitives](#available-primitives)
3. [Architecture](#architecture)
4. [Phase A: UDP Netcat](#phase-a-udp-netcat-current-capabilities)
5. [Phase B: TCP Mode](#phase-b-tcp-mode-after-networking-phase-5)
6. [Phase C: Bidirectional I/O and Scanning](#phase-c-bidirectional-io-and-scanning-after-networking-phase-6)
7. [Testing Strategy](#testing-strategy)
8. [File Layout and Registration](#file-layout-and-registration)

---

## Motivation

Phases 1–4 of the networking evolution plan are complete. The entire socket framework (PacketBuf, NetDev, ARP neighbor cache, routing table, loopback, socket table, UDP demux, setsockopt/getsockopt, shutdown) has been built — but **no userland app exercises the socket path**. `ifconfig` reads static config via `net_info()`. `nmap` does ARP scanning via `net_scan()`. Neither calls `socket()`, `bind()`, `sendto()`, or `recvfrom()`.

`nc` (netcat) is the standard Unix networking Swiss army knife. It's the right first tool because:

- It exercises the full socket lifecycle: `socket()` → `bind()` → `sendto()`/`recvfrom()` → `shutdown()` → close
- It has a natural UDP mode (`nc -u`) that maps exactly to current capabilities
- TCP mode (`nc` without `-u`) will validate Phase 5 when it lands
- `poll()`-based bidirectional I/O will validate Phase 6
- It's a tool every developer knows and expects

The plan is structured so Phase A is implementable today with zero stack changes, and Phases B/C extend `nc` as the stack evolves — no rewrites, just new branches in the mode dispatch.

---

## Available Primitives

What userland can do today (relevant to `nc`):

| Primitive | Syscall/Wrapper | Notes |
|---|---|---|
| Create UDP socket | `net::socket(AF_INET, SOCK_DGRAM, 0)` | Returns fd |
| Bind to port | `net::bind(fd, &addr)` / `net::bind_any(fd, port)` | Works for UDP |
| Send UDP datagram | `net::sendto(fd, data, 0, &addr)` | Full path to wire |
| Receive UDP datagram | `net::recvfrom(fd, buf, 0, &mut src)` | Returns sender addr |
| Set non-blocking | `net::set_nonblocking(fd)` | Via fcntl F_SETFL |
| Set reuse addr | `net::set_reuse_addr(fd)` | Via setsockopt |
| Resolve hostname | `net::resolve(hostname)` | Kernel DNS, returns `[u8; 4]` |
| Read keyboard (non-blocking) | `tty::try_read_char()` | Returns -1 if no input |
| Read keyboard (blocking) | `tty::read_char()` | Blocks until keypress |
| Read from stdin fd | `fs::read_slice(0, buf)` | Blocks if fd 0 is a TTY |
| Write to stdout fd | `fs::write_slice(1, buf)` | Captured by shell pipes |
| Sleep | `core::sleep_ms(ms)` | Yield CPU for N ms |
| Yield CPU | `core::yield_now()` | Cooperative yield |
| Monotonic time | `core::get_time_ms()` | For timeout tracking |
| High-res time | `core::clock_gettime_ns()` | For RTT measurement |
| Exit | `core::exit_with_code(code)` | Process exit |
| Shutdown socket | `net::shutdown(fd, how)` | SHUT_RD/WR/RDWR |

**Key limitation (pre-Phase 6):** No `poll()`. Cannot simultaneously wait on stdin + socket. I/O must be either half-duplex or use a non-blocking polling loop with `sleep_ms()`.

---

## Architecture

### Config Model (stable across all phases)

```rust
/// Parsed command-line configuration — built once, never mutated.
struct NcConfig {
    mode: NcMode,
    protocol: NcProtocol,
    remote_addr: [u8; 4],       // Resolved IPv4 (0 for listen mode)
    remote_port: u16,           // 0 for listen mode
    local_port: u16,            // 0 = ephemeral (client), required for listen
    verbose: bool,
    timeout_ms: u32,            // 0 = no timeout
}

enum NcMode {
    Client,                     // Connect to remote, send/receive
    Listen,                     // Bind locally, accept/receive
    // Scan,                    // Phase C: port scanning
}

enum NcProtocol {
    Udp,
    // Tcp,                     // Phase B
}
```

### Dispatch Model

```
nc_main(argv)
  → parse_args(argv) → NcConfig
  → match (config.protocol, config.mode):
      (Udp, Client) → udp_client(&config)
      (Udp, Listen) → udp_listen(&config)
      // (Tcp, Client) → tcp_client(&config)    Phase B
      // (Tcp, Listen) → tcp_listen(&config)    Phase B
```

Each mode function owns its I/O loop. This is intentional — the I/O pattern differs between UDP (datagram) and TCP (stream), and between pre-poll (Phase A/B: half-duplex) and post-poll (Phase C: full-duplex). Trying to unify them prematurely would create worse code.

### Output Convention

Follow the existing app pattern (`ifconfig.rs`, `nmap.rs`):
- Write to fd 1 via `fs::write_slice(1, buf)` with `tty::write()` fallback
- Hand-format numbers/IPs into stack buffers (no heap, no format macros)
- Verbose messages prefixed with `nc: ` to distinguish from data

---

## Phase A: UDP Netcat (Current Capabilities)

> **Depends on**: Nothing (current stack is sufficient)
> **Difficulty**: Medium
> **Estimated size**: ~400–500 lines

### A.1: Argument Parser

- [x] **A.1.1** Implement `parse_args(argc, argv) -> Result<NcConfig, NcError>`:
  - Walk argv tokens comparing against known flags
  - `-u` → `NcProtocol::Udp` (required in Phase A; Phase B makes it optional, TCP default)
  - `-l` → `NcMode::Listen`
  - `-v` → `verbose = true`
  - `-p <port>` → `local_port = port` (source port for client, ignored for listen)
  - `-w <seconds>` → `timeout_ms = seconds * 1000`
  - Positional args: `<host> <port>` for client mode, `<port>` for listen mode
  - Validate: listen mode requires port; client mode requires host + port; `-u` is required in Phase A
  - Return clear error messages for missing/invalid arguments

- [x] **A.1.2** Implement hostname/IP resolution in argument parsing:
  - Try parsing as dotted-quad first (e.g., `10.0.2.2` → `[10, 0, 2, 2]`)
  - If not a dotted-quad: call `net::resolve(hostname)` for DNS lookup
  - On failure: print error, exit 1

- [x] **A.1.3** Implement `print_usage()`:
  - Display: `usage: nc [-ulv] [-p port] [-w timeout] [host] port`
  - Triggered by no args, `-h`, or parse errors
  - Brief description of each flag

### A.2: Output Helpers

- [x] **A.2.1** Implement output utility functions:
  - `write_out(buf: &[u8])` — write to fd 1 with tty fallback (standard pattern)
  - `write_ipv4(ip: [u8; 4], out: &mut [u8], idx: &mut usize)` — format IP as dotted quad
  - `write_u16_dec(val: u16, out: &mut [u8], idx: &mut usize)` — format port number
  - `write_u8_dec(val: u8, out: &mut [u8], idx: &mut usize)` — format octet
  - `verbose_msg(config: &NcConfig, msg: &[u8])` — print only if `-v` is set, prefixed with `nc: `

### A.3: UDP Client Mode

- [x] **A.3.1** Implement `udp_client(config: &NcConfig)`:
  - `socket(AF_INET, SOCK_DGRAM, 0)` — create UDP socket
  - If `local_port != 0`: `bind_addr(fd, [0,0,0,0], local_port)` or `bind_any(fd, local_port)`
  - `set_nonblocking(fd)` — make socket non-blocking for receive polling
  - If verbose: print `nc: connected to <ip>:<port> (udp)`

- [x] **A.3.2** Implement client I/O loop (half-duplex, pre-poll):
  - **Send phase**: Read one line from stdin using `read_line_from_stdin()` (blocking tty read, line-buffered)
  - Build `SockAddrIn` for remote, call `sendto(fd, line, 0, &addr)`
  - If verbose: print bytes sent
  - **Receive phase**: Poll `recvfrom(fd, buf, 0, &mut src)` in a brief loop:
    - Try up to ~50 iterations with `sleep_ms(10)` between (500ms total receive window)
    - If `timeout_ms` is set: poll until timeout expires (tracked via `get_time_ms()`)
    - On data: write received bytes to stdout, print sender if verbose
    - On `WouldBlock`: continue polling until timeout
  - **Loop**: Return to send phase for next line
  - **Exit**: On EOF from stdin (read returns 0), shutdown socket, exit 0
  - **Interrupt**: Check `tty::try_read_char()` for Ctrl+C (0x03) → clean exit

- [x] **A.3.3** Implement `read_line_from_stdin(buf, max_len) -> usize`:
  - Read bytes from fd 0 via `fs::read_slice(0, &mut buf[pos..pos+1])` one byte at a time
  - Accumulate until `\n` or buffer full
  - Return line length (excluding newline)
  - Return 0 on EOF

### A.4: UDP Listen Mode

- [x] **A.4.1** Implement `udp_listen(config: &NcConfig)`:
  - `socket(AF_INET, SOCK_DGRAM, 0)` — create UDP socket
  - `set_reuse_addr(fd)` — allow quick rebind during development
  - `bind_any(fd, config.local_port)` — bind to specified port on all interfaces
  - `set_nonblocking(fd)` — non-blocking for polling
  - If verbose: print `nc: listening on 0.0.0.0:<port> (udp)`

- [x] **A.4.2** Implement listener receive loop:
  - Loop: call `recvfrom(fd, buf, 0, &mut src_addr)`
  - On data: write received bytes to stdout
  - If verbose: print `nc: received <N> bytes from <ip>:<port>`
  - On `WouldBlock`: `sleep_ms(10)`, continue loop
  - **Timeout**: If `-w` is set and no data received for `timeout_ms`: print timeout, exit 1
  - **Interrupt**: Poll `tty::try_read_char()` for Ctrl+C every iteration → clean exit
  - **Reply**: After receiving data, read one line from stdin and send it back to the sender's address (basic request-response pattern)
  - If stdin is EOF: receive-only mode (just print incoming packets)

### A.5: Error Handling

- [x] **A.5.1** Define `NcError` enum for argument parse errors:
  - `MissingHost`, `MissingPort`, `InvalidPort`, `ResolveFailed`, `UdpRequired` (Phase A only)
  - Map each to a human-readable error message

- [x] **A.5.2** Handle socket operation errors:
  - `socket()` fails → `nc: socket creation failed`, exit 1
  - `bind()` fails → `nc: bind failed (port in use?)`, exit 1
  - `sendto()` fails → `nc: send failed`, print error, continue (don't exit on transient send failure)
  - DNS resolution fails → `nc: cannot resolve '<hostname>'`, exit 1

### Phase A Gate

- [x] **GATE**: `nc -u 10.0.2.2 7` (QEMU SLIRP echo port) sends a line and receives the echo
- [x] **GATE**: `nc -u -l 12345` receives UDP packets sent from the host into QEMU
- [x] **GATE**: `-v` flag prints connection info and per-packet metadata
- [x] **GATE**: `-w 5` exits after 5 seconds of no data
- [x] **GATE**: `-p 54321` sets the source port correctly (visible in packet capture)
- [x] **GATE**: Invalid arguments print usage and exit 1
- [x] **GATE**: Hostname resolution works via kernel DNS: `nc -u gateway 53`
- [x] **GATE**: Ctrl+C exits cleanly (no hang, no panic)

---

## Phase B: TCP Mode (After Networking Phase 5)

> **Depends on**: Networking Phase 5 (TCP Stream Sockets v1)
> **Difficulty**: Medium
> **Estimated additions**: ~200 lines

### B.1: TCP Client

- [ ] **B.1.1** Add `NcProtocol::Tcp` variant; make TCP the default (no flag = TCP, `-u` = UDP)
- [ ] **B.1.2** Implement `tcp_client(config: &NcConfig)`:
  - `socket(AF_INET, SOCK_STREAM, 0)` → `connect(fd, &addr)` → stream I/O loop
  - I/O loop: same half-duplex pattern as UDP client but using `send()`/`recv()` instead of `sendto()`/`recvfrom()`
  - On `recv()` returning 0: connection closed by remote, print if verbose, exit 0
  - On send error (broken pipe): print error, exit 1

### B.2: TCP Listener

- [ ] **B.2.1** Implement `tcp_listen(config: &NcConfig)`:
  - `socket(AF_INET, SOCK_STREAM, 0)` → `set_reuse_addr(fd)` → `bind()` → `listen(fd, 1)` → `accept(fd, &mut peer)`
  - After accept: I/O loop on the accepted fd
  - Print peer address if verbose: `nc: connection from <ip>:<port>`
  - After client disconnects: exit (single-shot, like real `nc -l`)

- [ ] **B.2.2** Add `-k` flag (keep-listening): after one client disconnects, accept the next one

### B.3: Argument Parser Update

- [ ] **B.3.1** Remove the "UDP required" check from `parse_args()`
- [ ] **B.3.2** Default protocol becomes `Tcp` when `-u` is not specified
- [ ] **B.3.3** Update usage: `usage: nc [-ulvk] [-p port] [-w timeout] [host] port`

### Phase B Gate

- [ ] **GATE**: `nc 10.0.2.2 80` connects via TCP (3-way handshake completes)
- [ ] **GATE**: `nc -l 8080` accepts one TCP connection
- [ ] **GATE**: Data flows in both directions (half-duplex)
- [ ] **GATE**: FIN handling: remote close → `recv()` returns 0 → clean exit
- [ ] **GATE**: `-k` flag accepts multiple sequential connections

---

## Phase C: Bidirectional I/O and Scanning (After Networking Phase 6)

> **Depends on**: Networking Phase 6 (Blocking/Nonblocking + poll/select)
> **Difficulty**: Medium-High
> **Estimated additions**: ~300 lines

### C.1: Full-Duplex I/O via poll()

- [ ] **C.1.1** Replace the half-duplex I/O loops with `poll()`-based full-duplex:
  - `poll([{fd: stdin, events: POLLIN}, {fd: socket, events: POLLIN}], timeout)`
  - On stdin POLLIN: read, send to socket
  - On socket POLLIN: recv, write to stdout
  - On socket POLLHUP/POLLERR: print if verbose, exit
  - On timeout: exit if `-w` is set

- [ ] **C.1.2** Update both UDP and TCP modes to use the new I/O loop
  - The `poll()` loop is the same structure for both protocols; only send/recv calls differ

### C.2: Port Scanning

- [ ] **C.2.1** Add `-z` flag (zero-I/O scan mode):
  - `nc -z host port` — attempt TCP connect, report open/closed, no data transfer
  - `nc -z host 1-1024` — scan a range of ports (parse `start-end` from port argument)
  - For each port: `connect()` with timeout, report result

- [ ] **C.2.2** Add `-z` to UDP mode:
  - Send empty datagram, wait for ICMP unreachable (requires Phase 8 ICMP)
  - Report open (no ICMP unreachable) or closed (unreachable received)

### C.3: Proper Timeout Handling

- [ ] **C.3.1** Replace `get_time_ms()` polling loops with `poll()` timeout parameter
- [ ] **C.3.2** `-w` timeout applies to the `poll()` call directly — no more spin-polling

### Phase C Gate

- [ ] **GATE**: Full-duplex: data flows both directions simultaneously (not alternating)
- [ ] **GATE**: `nc -z host 1-100` scans 100 ports and reports open/closed
- [ ] **GATE**: `poll()` timeout replaces all `sleep_ms()` polling loops
- [ ] **GATE**: No busy-wait remains anywhere in `nc`

---

## Testing Strategy

### QEMU SLIRP Test Targets

QEMU's SLIRP user-mode networking provides built-in services useful for testing:

| Service | Address | Port | Protocol | Use |
|---|---|---|---|---|
| Gateway/DNS | `10.0.2.2` | 53 | UDP | DNS query test |
| Host forwarding | configurable | any | TCP/UDP | Port forwarding for custom targets |

For UDP echo testing, add `-net user,hostfwd=udp::12345-:12345` to QEMU args and run a host-side echo server, or use the DNS port (send a DNS query, receive a response — validates full round-trip).

### Phase A Smoke Tests (Manual)

1. **Basic send**: `nc -u -v 10.0.2.2 53` → type a DNS query payload → verify response received
2. **Listen mode**: `nc -u -v -l 12345` → send UDP from host into QEMU → verify display
3. **Timeout**: `nc -u -l 12345 -w 3` → wait 3s → verify clean exit
4. **Bad args**: `nc`, `nc -l`, `nc -u`, `nc -u 10.0.2.2` → verify usage printed
5. **DNS resolve**: `nc -u -v gateway 53` → verify `gateway` resolves to `10.0.2.2`

### Integration Test (Automatable via `just test`)

A future shell script test could:
1. Spawn `nc -u -l 12345 -w 2` in background
2. Send a UDP packet to 127.0.0.1:12345 (via loopback)
3. Verify received output
4. Verify process exits after timeout

This requires loopback UDP to work end-to-end (Phase 3 loopback device exists).

---

## File Layout and Registration

### New Files

```
userland/src/apps/nc.rs         — All nc logic (single file for Phase A, ~900 lines)
userland/src/bin/nc.rs          — Entry point (naked _start with argc/argv extraction)
```

When Phase B adds TCP (~200 lines), consider extracting to a module:
```
userland/src/apps/nc/
├── mod.rs                      — Entry, arg parsing, dispatch
├── udp.rs                      — UDP modes
├── tcp.rs                      — TCP modes (Phase B)
└── io.rs                       — Shared I/O loop (Phase C)
```

### Registration Changes

**`userland/src/apps/mod.rs`** — ✅ `pub mod nc;` added

**`userland/src/program_registry.rs`** — ✅ ProgramSpec entry added:
```rust
ProgramSpec {
    name: b"nc",
    path: b"/bin/nc",
    priority: 5,
    flags: TASK_FLAG_USER_MODE,
    desc: b"Network Swiss army knife",
    gui: false,
},
```

**`userland/src/bin/nc.rs`** — ✅ Uses naked `_start` to extract argc/argv from user stack:
```rust
#[unsafe(naked)]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "mov rdi, [rsp]",       // argc
        "lea rsi, [rsp + 8]",   // argv
        "and rsp, -16",         // 16-byte alignment
        "call {entry}",
        "ud2",
        entry = sym nc_entry,
    );
}
```

### Build System

✅ nc is included in `justfile` (`userland_bins`) and `scripts/build_userland.sh` (`BINS`).
The binary appears at `/bin/nc` in the ext2 filesystem image.

---

## Implementation Notes

### Entry Point: naked _start for argc/argv

Unlike other SlopOS userland programs that use the `entry!()` macro (which passes `null_mut()` as the arg),
`nc` needs command-line arguments. The kernel's exec handler places argc/argv on the user stack in standard
SysV ABI layout (`[rsp] = argc, [rsp+8] = argv[0], ...`). The naked `_start` function in `bin/nc.rs`
extracts these before calling into the Rust entry point. This pattern can be reused by future argument-aware programs.

### Half-Duplex I/O Model

Phase A uses half-duplex I/O (send → receive → repeat) because `poll()` is not yet available.
The receive phase uses a non-blocking socket with `sleep_ms(10)` polling. The default receive
window is 500ms; `-w` overrides with a longer timeout. Phase C will replace this with `poll()`.

### Tests

Unit tests for parsing/formatting are in `#[cfg(test)]` within `nc.rs`. These use the standard
Rust test harness and cannot run in the no_std kernel target. They serve as regression documentation
and can be extracted to a host-side test crate if needed.

---

## Dependency on Networking Evolution Plan

| nc Phase | Networking Phase Required | What It Unlocks |
|---|---|---|
| **A** (UDP) | Phase 4 ✅ (complete) | First userland socket test; UDP client + listener |
| **B** (TCP) | Phase 5 (pending) | TCP connect/listen; stream I/O |
| **C** (poll + scan) | Phase 6 (pending) | Full-duplex I/O; port scanning; no spin-polling |
Phase A is implemented. Phase B is blocked on TCP stack (networking Phase 5).

### Shell Output Streaming Fix

During Phase A testing, a shell bug was discovered: the `execute_registry_spawn()` function
in `userland/src/apps/shell/exec.rs` used a wait-then-drain pattern — it blocked on `waitpid()`
until the child exited, then drained the pipe. This hid all output for interactive programs
like `nc -u -l 12345` until Ctrl+C killed them.

**Fix**: Replaced with a streaming `poll()`/`waitpid_nohang()` loop that interleaves pipe
reads (10ms poll timeout) with non-blocking child-exit checks. Output now appears in
real-time. This also sidesteps the lost-wakeup race documented in the original code.

Research into Linux/GNU and Redox OS confirmed the long-term solution is a PTY subsystem
(both use pseudo-terminals where child processes inherit the terminal fd directly).
The poll-based loop is the correct intermediate architecture for SlopOS until PTY support
is implemented.

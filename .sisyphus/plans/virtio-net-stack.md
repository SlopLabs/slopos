# Virtio-Net Driver + Network Stack + Socket API + Webget

## TL;DR

> **Quick Summary**: Implement a complete networking subsystem for SlopOS — from virtio-net hardware driver through DHCP/ARP network discovery, a minimal TCP/IP stack, socket syscalls integrated into the existing FD system, and a `webget` userland binary for HTTP GET requests. Priority: be a networking device first, then build upward.
> 
> **Deliverables**:
> - Virtio-net PCI driver with MAC address read, packet TX/RX
> - DHCP client (real IP acquisition) + ARP neighbor table (Linux-inspired on-demand resolution)
> - Minimal UDP (for DHCP), IPv4, TCP (client-only) network stack
> - Socket syscalls (SYSCALL_SOCKET=120, SYSCALL_CONNECT=121) returning FDs
> - Socket FDs work with existing `SYSCALL_FS_READ`/`SYSCALL_FS_WRITE`
> - `webget` userland binary for HTTP GET
> - QEMU virtio-net-pci configuration in justfile/qemu_run.sh
> - AGENTS.md updated with networking + I/O + virtio documentation
> 
> **Estimated Effort**: Large
> **Parallel Execution**: YES — 8 waves
> **Critical Path**: Queue.rs extension → Virtio-net driver → ARP/Ethernet → DHCP → TCP → Socket syscalls → Webget

---

## Context

### Original Request
Implement a virtio-net driver, adjust the justfile for QEMU networking, add socket syscalls using the same read/write API as stdout/stderr, implement kernel buffering, and write a "webget" userland binary for HTTP GET requests. User clarified: DHCP for real IP discovery (not static), on-demand ARP neighbor discovery (Linux pattern), and **be a networking device first** before building higher-level protocols.

### Interview Summary
**Key Discussions**:
- **FD integration**: Sockets return FDs and reuse `SYSCALL_FS_READ(16)`/`SYSCALL_FS_WRITE(17)` — same API as files/pipes
- **DHCP over static**: User chose real DHCP discovery (DISCOVER→OFFER→REQUEST→ACK) over hardcoded 10.0.2.15
- **Neighbor discovery**: Linux-style on-demand ARP (not proactive scanning), passively learn from traffic
- **Priority order**: Be a device → get on network → discover neighbors → then sockets/TCP/webget
- **QA strategy**: QEMU boot verification via `just boot-log`, no unit test infrastructure
- **AGENTS.md**: Must be updated with networking docs + general syscall/IO/virtio findings

**Research Findings**:
- Existing virtio infrastructure fully reusable (queue.rs, pci.rs, mod.rs) but needs extension for multi-buffer RX
- FD system has 32 FDs/process, `FileDescriptor` struct with pipe routing pattern — sockets follow same model
- Linux neighbor subsystem: on-demand ARP, state machine (INCOMPLETE→REACHABLE→STALE), timer-based expiry
- Linux in-kernel DHCP: `net/ipv4/ipconfig.c`, raw UDP over IPv4, BOOTP frame format with DHCP options
- QEMU user-mode networking: gateway 10.0.2.2, DNS 10.0.2.3, DHCP server built-in, NAT for guest TCP

### Metis Review
**Identified Gaps** (addressed):
- **Virtqueue API mismatch**: Current queue.rs designed for synchronous single-request (virtio-blk). Net needs multi-buffer RX with descriptor allocator → Added as Wave 1 task
- **Async RX path**: Pre-populate RX buffers immediately after `set_driver_ok()` to avoid missing first packets → Added to driver probe task
- **QEMU args mangling**: `QEMU_PCI_DEVICES` env var splits incorrectly for `-netdev` args → Hardcode in qemu_run.sh
- **FileDescriptor bloat**: Socket state doesn't belong in FD struct → Use PipeSlot pattern (socket_id index into separate table)
- **Device ID ambiguity**: Match both legacy 0x1000 and modern 0x1041 in match function
- **No MRG_RXBUF**: Don't negotiate, keeps virtio-net header fixed at 10 bytes

---

## Work Objectives

### Core Objective
Make SlopOS a functioning network device: acquire an IP via DHCP, resolve neighbors via ARP, communicate over TCP, and expose this to userland through socket FDs — culminating in an HTTP GET client.

### Concrete Deliverables
- `drivers/src/virtio_net.rs` — Virtio-net PCI driver
- `drivers/src/net/` — Network stack modules (ethernet, arp, ipv4, udp, tcp, dhcp, socket)
- `core/src/syscall/net_handlers.rs` — Socket syscall kernel handlers
- `userland/src/syscall/net.rs` — Userland socket syscall wrappers
- `userland/src/bin/webget.rs` + `userland/src/apps/webget.rs` — HTTP GET client
- Modified: `drivers/src/virtio/queue.rs` (descriptor allocator), `fs/src/fileio.rs` (socket FD routing), `abi/src/syscall.rs` (new constants), `scripts/qemu_run.sh` (net device), `justfile` (userland_bins), `AGENTS.md`

### Definition of Done
- [ ] `just boot-log` shows: virtio-net ready with MAC, DHCP lease acquired, ARP gateway resolved
- [ ] `just build` succeeds with zero errors
- [ ] `just test` passes (no virtio-blk regression from queue.rs changes)
- [ ] webget binary successfully fetches HTTP response from QEMU host-forwarded port

### Must Have
- Virtio-net driver that reads MAC and can TX/RX raw Ethernet frames
- DHCP client that acquires real IP address
- ARP neighbor table with on-demand resolution
- Socket syscalls (create, connect) returning FDs
- FS_READ/FS_WRITE work on socket FDs (same API as files)
- webget binary prints HTTP response to serial
- W/L currency integration (win on connect, loss on timeout)

### Must NOT Have (Guardrails)
- ❌ No checksum offload, TSO/GSO, hardware offload features — zero-init virtio-net header
- ❌ No `bind()`, `listen()`, `accept()`, `setsockopt()` — only socket + connect
- ❌ No IP routing table — all packets go through single gateway
- ❌ No DNS resolver — webget takes IP address directly
- ❌ No abstraction layers (trait NetworkDevice, trait Protocol) — direct function calls
- ❌ No separate kernel threads for network processing — all in syscall context
- ❌ No IPv6, TLS, HTTPS, ICMP, IP fragmentation
- ❌ No `VIRTIO_NET_F_MRG_RXBUF` negotiation (header stays 10 bytes)
- ❌ No extensive error categorization — ECONNREFUSED, ETIMEDOUT, EAGAIN are sufficient

---

## Verification Strategy

> **ZERO HUMAN INTERVENTION** — ALL verification is agent-executed. No exceptions.

### Test Decision
- **Infrastructure exists**: YES (interrupt test harness via `just test`)
- **Automated tests**: No unit tests for drivers (none exist in project)
- **Framework**: QEMU boot verification via `just boot-log` + serial output grep
- **Regression**: `just test` verifies virtio-blk still works after queue.rs changes

### QA Policy
Every task includes agent-executed QA scenarios verified via QEMU serial output.
Evidence saved to `.sisyphus/evidence/task-{N}-{scenario-slug}.{ext}`.

- **Driver/Network**: Use Bash (`just boot-log`) — boot QEMU, grep serial for expected log lines
- **Build verification**: Use Bash (`just build`) — exit code 0
- **Regression**: Use Bash (`just test`) — "Interrupt tests passed."
- **Userland (webget)**: Use Bash (`just boot-log BOOT_LOG_TIMEOUT=30`) — grep for HTTP response

---

## Execution Strategy

### Parallel Execution Waves

```
Wave 1 (Foundation — start immediately, all independent):
├── Task 1: Extend queue.rs with descriptor allocator + pop_used [deep]
├── Task 2: Add QEMU virtio-net-pci args to qemu_run.sh [quick]
├── Task 3: Add socket syscall constants to abi/src/syscall.rs [quick]
├── Task 4: Add net module skeleton in drivers/src/net/ [quick]
└── Task 5: Bump SYSCALL_TABLE_SIZE to 256 in abi [quick]

Wave 2 (Virtio-net driver — depends on Task 1):
├── Task 6: virtio_net.rs: PCI match/probe, MAC read, device init [deep]
├── Task 7: virtio_net.rs: TX path (send raw Ethernet frame) [unspecified-high]
└── Task 8: virtio_net.rs: RX path (poll-based receive, pre-populated buffers) [unspecified-high]

Wave 3 (Ethernet + ARP — depends on Tasks 6-8):
├── Task 9: Ethernet frame builder/parser module [unspecified-high]
├── Task 10: ARP protocol + neighbor table (on-demand + passive) [deep]
└── Task 11: Register virtio-net driver at boot + integration smoke test [quick]

Wave 4 (Get on the network — depends on Tasks 9-10):
├── Task 12: IPv4 packet builder/parser [unspecified-high]
├── Task 13: UDP minimal (for DHCP only) [unspecified-high]
└── Task 14: DHCP client (DISCOVER→OFFER→REQUEST→ACK) [deep]

Wave 5 (TCP — depends on Tasks 12, 14):
├── Task 15: TCP client state machine (SYN→ESTABLISHED→FIN) [deep]
└── Task 16: TCP data send/receive with kernel ring buffers [deep]

Wave 6 (Socket syscalls — depends on Tasks 15-16):
├── Task 17: SocketSlot table + FileDescriptor socket_id extension [deep]
├── Task 18: Syscall handlers (socket, connect) + FD read/write routing [deep]
└── Task 19: Userland syscall wrappers (userland/src/syscall/net.rs) [quick]

Wave 7 (Webget + docs — depends on Tasks 17-19):
├── Task 20: webget userland binary (HTTP GET client) [unspecified-high]
├── Task 21: Update justfile userland_bins + build scripts [quick]
└── Task 22: Update AGENTS.md with networking + IO + virtio docs [writing]

Wave FINAL (Verification — after ALL tasks):
├── Task F1: Plan compliance audit [oracle]
├── Task F2: Code quality review [unspecified-high]
├── Task F3: Full integration QA [unspecified-high]
└── Task F4: Scope fidelity check [deep]

Critical Path: T1 → T6 → T8 → T10 → T14 → T15 → T17 → T18 → T20 → F1-F4
Parallel Speedup: ~60% faster than sequential
Max Concurrent: 5 (Wave 1)
```

### Dependency Matrix

| Task | Depends On | Blocks | Wave |
|------|-----------|--------|------|
| 1 | — | 6,7,8 | 1 |
| 2 | — | 11 | 1 |
| 3 | — | 18,19 | 1 |
| 4 | — | 9,10,12,13,14,15 | 1 |
| 5 | — | 18 | 1 |
| 6 | 1 | 7,8,11 | 2 |
| 7 | 1,6 | 9,10 | 2 |
| 8 | 1,6 | 9,10 | 2 |
| 9 | 4,7,8 | 10,12 | 3 |
| 10 | 4,8,9 | 14 | 3 |
| 11 | 2,6 | — | 3 |
| 12 | 4,9 | 13,15 | 4 |
| 13 | 4,12 | 14 | 4 |
| 14 | 10,12,13 | 15 | 4 |
| 15 | 12,14 | 16 | 5 |
| 16 | 15 | 17,18 | 5 |
| 17 | 16 | 18 | 6 |
| 18 | 3,5,17 | 19,20 | 6 |
| 19 | 3,18 | 20 | 6 |
| 20 | 18,19 | 21 | 7 |
| 21 | 20 | F1-F4 | 7 |
| 22 | — | F1 | 7 |

### Agent Dispatch Summary

- **Wave 1**: 5 tasks — T1→`deep`, T2→`quick`, T3→`quick`, T4→`quick`, T5→`quick`
- **Wave 2**: 3 tasks — T6→`deep`, T7→`unspecified-high`, T8→`unspecified-high`
- **Wave 3**: 3 tasks — T9→`unspecified-high`, T10→`deep`, T11→`quick`
- **Wave 4**: 3 tasks — T12→`unspecified-high`, T13→`unspecified-high`, T14→`deep`
- **Wave 5**: 2 tasks — T15→`deep`, T16→`deep`
- **Wave 6**: 3 tasks — T17→`deep`, T18→`deep`, T19→`quick`
- **Wave 7**: 3 tasks — T20→`unspecified-high`, T21→`quick`, T22→`writing`
- **FINAL**: 4 tasks — F1→`oracle`, F2→`unspecified-high`, F3→`unspecified-high`, F4→`deep`

---

## TODOs

---

## Final Verification Wave

- [ ] F1. **Plan Compliance Audit** — `oracle`
  Read the plan end-to-end. For each "Must Have": verify implementation exists (read file, run command). For each "Must NOT Have": search codebase for forbidden patterns — reject with file:line if found. Check evidence files exist in .sisyphus/evidence/. Compare deliverables against plan.
  Output: `Must Have [N/N] | Must NOT Have [N/N] | Tasks [N/N] | VERDICT: APPROVE/REJECT`

- [ ] F2. **Code Quality Review** — `unspecified-high`
  Run `just build`. Review all new/changed files for: `as any`/`@ts-ignore`, empty catches, unnecessary `unsafe`, commented-out code, unused imports. Check AI slop: excessive comments, over-abstraction, generic names. Verify all network headers use `to_be_bytes()`/`from_be_bytes()` for endianness.
  Output: `Build [PASS/FAIL] | Files [N clean/N issues] | VERDICT`

- [ ] F3. **Full Integration QA** — `unspecified-high`
  Start from clean `just build`. Run `just boot-log BOOT_LOG_TIMEOUT=30`. Verify ALL of: virtio-net ready log, MAC address printed, DHCP lease acquired log, ARP gateway resolved log, webget HTTP response visible. Run `just test` for regression. Save all output to `.sisyphus/evidence/final-qa/`.
  Output: `Scenarios [N/N pass] | Regression [PASS/FAIL] | VERDICT`

- [ ] F4. **Scope Fidelity Check** — `deep`
  For each task: read "What to do", read actual code. Verify 1:1 — everything in spec was built, nothing beyond spec was built. Check "Must NOT do" compliance. Detect scope creep (unnecessary abstractions, extra protocols). Flag unaccounted changes.
  Output: `Tasks [N/N compliant] | Scope [CLEAN/N issues] | VERDICT`

---

## Commit Strategy

- After Wave 1: `feat(virtio): extend virtqueue with descriptor allocator for multi-buffer support`
- After Wave 2: `feat(drivers): add virtio-net PCI driver with MAC read and raw packet TX/RX`
- After Wave 3: `feat(net): add ethernet framing and ARP neighbor table`
- After Wave 4: `feat(net): add DHCP client for automatic IP configuration`
- After Wave 5: `feat(net): add minimal TCP client with kernel ring buffers`
- After Wave 6: `feat(core): add socket syscalls with FD integration`
- After Wave 7: `feat(userland): add webget HTTP GET client + docs update`

---

## Success Criteria

### Verification Commands
```bash
just build              # Expected: exit code 0
just test               # Expected: "Interrupt tests passed." (no regression)
just boot-log BOOT_LOG_TIMEOUT=30 2>&1 | grep "virtio-net: ready"   # Expected: MAC printed
just boot-log BOOT_LOG_TIMEOUT=30 2>&1 | grep -i "dhcp"             # Expected: DHCP lease log
just boot-log BOOT_LOG_TIMEOUT=30 2>&1 | grep -i "arp.*gateway"     # Expected: gateway MAC
just boot-log BOOT_LOG_TIMEOUT=30 2>&1 | grep -i "HTTP/"            # Expected: HTTP response
```

### Final Checklist
- [ ] All "Must Have" present
- [ ] All "Must NOT Have" absent
- [ ] `just build` succeeds
- [ ] `just test` passes (virtio-blk regression check)
- [ ] DHCP lease acquired in boot log
- [ ] ARP gateway MAC discovered in boot log
- [ ] webget prints HTTP response to serial
- [ ] AGENTS.md updated with networking documentation

# SlopOS Networking Evolution Plan

> **Status**: In progress — Phase 1A–1D complete, Phase 2 pending
> **Target**: Evolve SlopOS networking from functional prototype to architecturally sound, BSD-socket-compatible TCP/IP stack
> **Scope**: Buffer pools, netdev abstraction, ARP, routing, BSD sockets (UDP+TCP), I/O multiplexing, userspace DNS, IPv4 hardening, multi-NIC, packet filtering
> **Design principles**: Linux-informed architecture, smoltcp-inspired Rust idioms, zero technical debt in foundational abstractions

---

## Table of Contents

1. [Executive Summary](#executive-summary)
2. [Current State Assessment](#current-state-assessment)
3. [Critical Architectural Decisions](#critical-architectural-decisions)
4. [Phase 1: Net Core Contracts (PacketBuf + NetDev + Type Foundation)](#1-phase-1-net-core-contracts-packetbuf--netdev--type-foundation)
5. [Phase 2: Timer Infrastructure and ARP Neighbor Cache v1](#2-phase-2-timer-infrastructure-and-arp-neighbor-cache-v1)
6. [Phase 3: L3 Config, Loopback, and Routing v1](#3-phase-3-l3-config-loopback-and-routing-v1)
7. [Phase 4: Socket Framework and UDP Sockets v1](#4-phase-4-socket-framework-and-udp-sockets-v1)
8. [Phase 5: TCP Stream Sockets v1](#5-phase-5-tcp-stream-sockets-v1)
9. [Phase 6: Blocking/Nonblocking and poll/select](#6-phase-6-blockingnonblocking-and-pollselect)
10. [Phase 7: Userspace DNS Resolver](#7-phase-7-userspace-dns-resolver)
11. [Phase 8: IPv4 Robustness and TCP Hardening](#8-phase-8-ipv4-robustness-and-tcp-hardening)
12. [Phase 9: Multi-NIC, Packet Filter, Raw Sockets](#9-phase-9-multi-nic-packet-filter-raw-sockets)
13. [Dependency Graph](#dependency-graph)
14. [Future Considerations](#future-considerations)

---

## Executive Summary

SlopOS networking began as a hardcoded dispatch loop inside the VirtIO-net driver. It works, in the same way a campfire works as a heating system: functional under ideal conditions, catastrophic when you need to scale. The stack has grown organically — TCP state machine, DNS cache, DHCP, NAPI polling — but without the foundational abstractions that make a network stack maintainable, extensible, or safe.

This plan charts nine phases from the current prototype to a clean, BSD-socket-compatible IPv4 stack with proper driver separation, dynamic ARP, a routing table, blocking/nonblocking sockets, userspace DNS, and multi-NIC support. Each phase builds on the last. Nothing is optional.

The plan is informed by three reference architectures: **Linux** (battle-tested patterns for sk_buff, neighbor subsystem, two-queue TCP listen, FIB routing), **smoltcp** (Rust-idiomatic zero-allocation device traits, token-based buffer lending), and **Redox OS** (userspace networking via schemes, clean driver–stack separation). Where these diverge, the plan favors Linux's proven architecture with smoltcp's Rust idioms — leveraging ownership, newtypes, enum dispatch, and `Drop` semantics to eliminate entire classes of bugs at compile time.

| Current Component | Status | Future State | Phase |
|---|---|---|---|
| `drivers/src/net/mod.rs` — Ethernet/IP parsing | Functional, no abstraction | Replaced by PacketBuf + demux pipeline | 1 |
| `drivers/src/virtio_net.rs` — VirtIO driver | Hardcoded dispatch, no trait | Implements `NetDevice` trait, pool-backed buffers | 1 |
| No type-safe network types | Raw u32/u16 everywhere | Newtype wrappers with byte-order safety | 1 |
| ARP table in `virtio_net.rs` | Static, hardcoded | Dynamic neighbor cache with state machine | 2 |
| No timer infrastructure | Ad-hoc delays | Data-driven timer wheel with typed dispatch | 2 |
| IPv4 config | Implicit, DHCP-only | Per-interface config struct, routing table | 3 |
| Loopback | Absent | `lo` netdev, 127.0.0.1/8, local delivery | 3 |
| `drivers/src/net/socket.rs` — UDP | Functional, fragile demux | Clean socket layer with setsockopt framework | 4 |
| No setsockopt/getsockopt | N/A | Full SO_REUSEADDR, SO_RCVBUF, SO_SNDBUF | 4 |
| No shutdown() | N/A | POSIX shutdown(SHUT_RD/WR/RDWR) | 4 |
| `drivers/src/net/tcp.rs` — TCP | RFC 793 state machine, no socket integration | Two-queue listen, PCB-to-socket mapping, wakeups | 5 |
| Blocking I/O | Absent | O_NONBLOCK, SO_RCVTIMEO, poll() syscall | 6 |
| `drivers/src/net/dns.rs` — DNS | In-kernel LRU cache | Userspace resolver using UDP sockets | 7 |
| IPv4 fragmentation | Absent | TX fragmentation, RX reassembly with limits | 8 |
| ICMP | Absent | Error handling (unreachable, frag needed) | 8 |
| Multi-NIC | Absent | Multiple netdev instances, per-interface ARP | 9 |
| Packet filtering | Absent | 5-tuple filter hooks at PREROUTING/INPUT/OUTPUT | 9 |
| Raw sockets | Absent | Capability-gated raw sockets for ping/traceroute | 9 |

---

## Current State Assessment

The networking subsystem lives across four main files: `drivers/src/net/mod.rs` (Ethernet/IP constants and checksum helpers), `drivers/src/net/socket.rs` (socket table and UDP machinery), `drivers/src/net/tcp.rs` (TCP state machine), and `drivers/src/virtio_net.rs` (the VirtIO driver that does everything else).

The VirtIO driver is the real problem. `dispatch_rx_frame()` contains hardcoded protocol dispatch: it parses Ethernet, decides if it's ARP or IPv4, handles ARP inline, and routes IPv4 to UDP or TCP. The ARP table is a static array inside the driver. There is no `NetDevice` trait, so adding a second NIC means duplicating the entire dispatch path. There is no `PacketBuf` abstraction, so every layer receives a raw byte slice and re-parses headers from scratch.

The socket layer (`socket.rs`) implements `KernelSocket`, `UdpReceiveQueue`, and `SocketTable` with a `MAX_SOCKETS=64` limit. The syscall handlers in `core/src/syscall/net_handlers.rs` cover all eleven socket+DNS operations. The ABI in `abi/src/net.rs` defines `AF_INET`, `SOCK_STREAM`, `SOCK_DGRAM`, and `SockAddrIn`. The userland wrappers in `userland/src/syscall/net.rs` are present. Shell apps `ifconfig.rs` and `nmap.rs` exist.

What's missing: a pool-backed `PacketBuf` type with headroom, a `NetDevice` trait, a dynamic ARP cache, a routing table, a loopback device, `O_NONBLOCK`, `poll()`/`select()`, ICMP error handling, IPv4 fragmentation/reassembly, multi-NIC routing, packet filtering, `setsockopt`/`getsockopt`, `shutdown()`, `SO_REUSEADDR`, proper timer infrastructure, type-safe network primitives, and a proper two-queue TCP listen model. The DNS resolver lives in the kernel when it should live in userspace.

---

## Critical Architectural Decisions

These six decisions have rewrite-danger if made wrong. They must be settled before or during Phase 1, and subsequent phases must not contradict them.

### CAD-1: PacketBuf and Buffer Pool Model

**Decision**: `PacketBuf` is backed by a **fixed-size buffer pool** (slab allocator), not per-packet heap allocation. The pool pre-allocates N buffers of `PACKET_BUF_SIZE` (2048 bytes, covering max Ethernet frame + headroom). Each buffer is handed out via `PacketPool::alloc()` and automatically returned on `Drop`. An oversized fallback path uses `alloc::vec::Vec<u8>` only for reassembly buffers exceeding the standard size. All header access goes through offset fields (`l2_offset`, `l3_offset`, `l4_offset`) stored in the `PacketBuf`. Ownership is move-only — no `Arc<PacketBuf>` in the fast path.

```rust
pub struct PacketBuf {
    inner: PacketBufInner,
    head: u16,
    tail: u16,
    l2_offset: u16,
    l3_offset: u16,
    l4_offset: u16,
}

enum PacketBufInner {
    Pooled { pool: &'static PacketPool, slot: u16 },
    Oversized { data: Vec<u8> },
}

impl Drop for PacketBuf {
    fn drop(&mut self) {
        if let PacketBufInner::Pooled { pool, slot } = &self.inner {
            pool.release(*slot); // O(1) return to freelist
        }
    }
}
```

**Rationale**: Linux uses `kmem_cache` (slab) for `sk_buff` allocation because per-packet `kmalloc` is too slow and causes heap fragmentation under load. A pool gives O(1) alloc/free, predictable memory usage, and cache-friendly layout. The `Drop` impl leverages Rust's ownership system — buffers are automatically returned when they leave scope, preventing leaks without manual free calls. The oversized fallback prevents the pool from constraining reassembly (Phase 8) while keeping the fast path allocation-free. smoltcp avoids allocation entirely via token-based buffer lending; we rejected that model because SlopOS needs to store packets in queues across async boundaries (socket receive queues, ARP pending queues), which requires owned buffers.

**DMA alignment**: The pool allocates each slot at `CACHE_LINE_SIZE` (64-byte) alignment. VirtIO RX buffers are DMA-mapped by the driver; the driver copies received data into a `PacketBuf` from the pool. This conscious copy is acceptable at hobby-OS throughput levels. A future zero-copy path could have the driver "donate" its DMA buffer as a `PacketBufInner::Dma` variant.

**Headroom contract**: Every pooled buffer has `HEADROOM` (128 bytes) reserved. `PacketBuf::alloc()` sets `head = HEADROOM`, `tail = HEADROOM`. This allows prepending up to 128 bytes of headers (Ethernet 14 + IP 20 + TCP 60 + options) without reallocation.

**Risk**: If we ever need zero-copy RX from VirtIO, we add a `PacketBufInner::Dma` variant. The abstraction supports this without breaking callers.

### CAD-2: NetDevice Boundary and Fast-Path Decoupling

**Decision**: `NetDevice` is a trait with `tx()`, `poll_rx()`, `set_up()`, `set_down()`, `mtu()`, `mac()`, `stats()`, and `features()`. The driver owns its rings and DMA buffers. The stack owns routing and demux. Only `PacketBuf` crosses this boundary.

The `NetDeviceRegistry` stores devices behind a spinlock but is accessed **only on the control plane** (registration, enumeration, configuration). The **data plane** (NAPI poll loops, TX from socket layer) operates through `DeviceHandle` — a stable reference obtained once during setup that does not require taking the registry lock.

```rust
pub struct DeviceHandle {
    dev: *mut dyn NetDevice,  // Stable pointer, valid for device lifetime
    index: DevIndex,
    tx_lock: SpinLock<()>,    // Per-device TX serialization
}

impl DeviceHandle {
    pub fn tx(&self, pkt: PacketBuf) -> Result<(), NetError> { ... }
    pub fn poll_rx(&self, budget: usize) -> SmallVec<[PacketBuf; 8]> { ... }
}
```

**Rationale**: Linux uses RCU for lockless `dev_get_by_index()` reads. SlopOS doesn't have RCU, but we can achieve the same effect: the NAPI loop obtains a `DeviceHandle` once at registration and holds it for the device's lifetime. No per-packet lock acquisition. TX uses a per-device lock (not global) because multiple sockets may transmit to the same device concurrently. The `SmallVec<[PacketBuf; 8]>` for `poll_rx` avoids heap allocation for typical poll batches.

**Risk**: The `*mut dyn NetDevice` raw pointer requires careful lifetime management. Devices must not be unregistered while a `DeviceHandle` exists. This is enforced by making `set_down()` drain the NAPI loop before invalidating the handle.

### CAD-3: Socket Layer, FD Integration, and Socket Options

**Decision**: `Socket` is a struct with an `inner: SocketInner` enum (not `dyn` vtable), state, flags, receive queue, wait queue, and socket options. File descriptors map to sockets through the existing FD table. Copy-in/copy-out for all userspace data. `NetError` enum for all internal error paths, converted to errno at the syscall boundary.

```rust
pub enum SocketInner {
    Udp(UdpSocket),
    Tcp(TcpSocket),
    Raw(RawSocket),  // Phase 9
}

pub struct Socket {
    inner: SocketInner,
    state: SocketState,
    flags: SocketFlags,          // O_NONBLOCK, etc.
    options: SocketOptions,      // SO_REUSEADDR, SO_RCVBUF, timeouts
    local_addr: Option<SockAddr>,
    remote_addr: Option<SockAddr>,
    recv_queue: BoundedQueue<PacketBuf>,
    wait_queue: WaitQueue,
}

pub struct SocketOptions {
    pub reuse_addr: bool,
    pub recv_buf_size: usize,    // Default 16384
    pub send_buf_size: usize,    // Default 16384
    pub recv_timeout: Option<u64>,  // milliseconds
    pub send_timeout: Option<u64>,
    pub keepalive: bool,
    pub tcp_nodelay: bool,
}
```

**Enum dispatch vs dyn vtable**: We use enum dispatch instead of `Box<dyn SocketOps>` because there are exactly three protocol types (UDP, TCP, Raw). Enum dispatch gives: no heap allocation for the vtable, exhaustive `match` (compiler catches missing protocol handling), slightly faster dispatch (no pointer indirection), and more idiomatic Rust. Adding a fourth protocol means adding one enum variant — acceptable for a hobby OS.

**setsockopt/getsockopt framework**: A `SYSCALL_SETSOCKOPT` and `SYSCALL_GETSOCKOPT` syscall pair is the single entry point for all socket configuration. Levels: `SOL_SOCKET` for `SO_*` options, `IPPROTO_TCP` for `TCP_*` options. This is introduced in Phase 4 and used by every subsequent phase.

**shutdown() semantics**: `SYSCALL_SHUTDOWN(fd, how)` is distinct from `close()`. `SHUT_WR` sends FIN and disallows further sends while the socket remains open for reading. `SHUT_RD` discards further received data. `close()` calls `shutdown(RDWR)` + drops the socket. This is critical for correct TCP half-close (HTTP/1.0 pattern: client sends request, shuts down write side, reads response).

**SO_REUSEADDR**: Enabled via `setsockopt(SOL_SOCKET, SO_REUSEADDR, 1)`. Allows `bind()` to succeed for a listening socket even if a previous connection on that port is in `TIME_WAIT`. Without this, restarting a TCP server during development fails with `EADDRINUSE` for the entire TIME_WAIT period — making development extremely frustrating.

**Growable socket table**: The `SocketTable` uses a slab allocator (freelist over `Vec<Option<Socket>>`) instead of a fixed `MAX_SOCKETS=64` array. Initial capacity is 64, grows on demand. The socket table maps FD → Socket. Protocol-specific demux tables (UDP port map, TCP 4-tuple map) are **separate** data structures — they serve different lookup patterns and scale independently.

**Internal IoSlice abstraction**: All internal send/recv APIs accept `&[IoSlice<'_>]` (a slice of byte slices) instead of a single contiguous buffer. This allows vectored I/O and means a future zero-copy VM mapping path can plug in at the syscall boundary without rewriting protocol code.

```rust
pub struct IoSlice<'a> {
    buf: &'a [u8],
}

// Internal protocol API
fn udp_send(sock: &Socket, slices: &[IoSlice<'_>], dst: SockAddr) -> Result<usize, NetError>;
```

**Rationale**: Linux's socket layer uses `proto_ops` (function pointer table) for protocol dispatch, `setsockopt`/`getsockopt` for all configuration, `iovec` for vectored I/O, and separate hash tables for port/4-tuple demux. We follow the same architecture with Rust-idiomatic substitutions: enum for vtable, `NetError` for errno, `IoSlice` for iovec.

**Risk**: The enum dispatch means adding a new protocol (e.g., SCTP) requires modifying the enum. For a hobby OS, this is a feature (compiler forces you to handle the new protocol everywhere) not a bug.

### CAD-4: Timer Infrastructure

**Decision**: All network timers (ARP aging, TCP retransmit, TCP delayed ACK, TCP keepalive, TCP TIME_WAIT, reassembly timeout) use a **data-driven timer wheel** with typed dispatch. No bare `fn()` callbacks — timers carry a `TimerKind` discriminant and a `key` that identifies the specific resource (ARP entry ID, TCP connection ID, reassembly group ID).

```rust
pub enum TimerKind {
    ArpExpire,
    ArpRetransmit,
    TcpRetransmit,
    TcpDelayedAck,
    TcpTimeWait,
    TcpKeepalive,
    ReassemblyTimeout,
}

pub struct TimerEntry {
    deadline_tick: u64,
    kind: TimerKind,
    key: u32,               // Connection ID, neighbor entry ID, etc.
    token: TimerToken,       // For cancellation
    cancelled: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TimerToken(u64);  // Opaque, monotonically increasing
```

On each tick, the wheel fires all expired entries and dispatches:

```rust
match entry.kind {
    TimerKind::ArpExpire => neighbor_cache.on_expire(entry.key),
    TimerKind::ArpRetransmit => neighbor_cache.on_retransmit(entry.key),
    TimerKind::TcpRetransmit => tcp_engine.on_retransmit(entry.key),
    TimerKind::TcpDelayedAck => tcp_engine.on_delayed_ack(entry.key),
    TimerKind::TcpTimeWait => tcp_engine.on_time_wait_expire(entry.key),
    TimerKind::TcpKeepalive => tcp_engine.on_keepalive(entry.key),
    TimerKind::ReassemblyTimeout => reassembly.on_timeout(entry.key),
}
```

**Rationale**: Bare `fn()` callbacks cannot carry state — they have no way to know which ARP entry or TCP connection triggered them. Linux solves this by embedding `timer_list` in the owning struct and using `container_of()` to reach the parent. Rust doesn't have `container_of()`, and closures (`Box<dyn FnOnce()>`) would require heap allocation per timer. The data-driven approach is both zero-allocation and type-safe: the `match` is exhaustive, so adding a new `TimerKind` forces handling it. The `TimerToken` enables O(1) cancellation (mark entry as cancelled; `tick()` skips it) which is critical for TCP: when an ACK arrives, the retransmit timer must be cancelled immediately.

**Wheel structure**: 256 slots, driven by the existing timer interrupt. Long delays (>256 ticks) use multiple rotations tracked by the absolute `deadline_tick`. Per-tick work is bounded: if more than `MAX_TIMERS_PER_TICK` (32) entries expire in one slot, the remainder are deferred to the next tick to prevent interrupt-context stalls.

**Risk**: The `key: u32` is an opaque index. Each subsystem must validate the key still refers to a live resource (the ARP entry / TCP connection may have been closed before the timer fires). This is the timer-cancellation race: always check liveness before acting on a timer callback.

### CAD-5: Routing and Neighbor Cache Scoping

**Decision**: ARP caches and routing tables are per-interface from Phase 2 onward. No global singletons. `route_lookup(dst_ip)` returns `(DeviceHandle, next_hop_ip)`. The neighbor cache is keyed by `(DevIndex, Ipv4Addr)`.

**Rationale**: Global singletons are the reason multi-NIC is hard to add later. Per-interface scoping from day one means Phase 9 is an extension, not a rewrite. Linux learned this lesson when adding network namespaces — the entire routing/neighbor subsystem had to be refactored for per-namespace scoping.

**Routing table structure**: Routes are stored in a `Vec<RouteEntry>` **bucketed by prefix length**. Lookup iterates from longest prefix (/32) to shortest (/0), checking only routes at each prefix length. This is O(32) worst case (one check per possible prefix length) regardless of route count, compared to O(n) for a flat linear scan. For a hobby OS with <100 routes, this is sufficient. A trie is not justified.

```rust
pub struct RouteTable {
    /// Routes bucketed by prefix length (index 0 = /0, index 32 = /32)
    buckets: [SmallVec<[RouteEntry; 4]>; 33],
}

impl RouteTable {
    pub fn lookup(&self, dst: Ipv4Addr) -> Option<(DeviceHandle, Ipv4Addr)> {
        for prefix_len in (0..=32).rev() {
            for route in &self.buckets[prefix_len] {
                if route.matches(dst) {
                    return Some((route.dev_handle.clone(), route.next_hop(dst)));
                }
            }
        }
        None
    }
}
```

**Risk**: Slightly more complex lookup in Phase 2-3 when there's only one interface. Worth it.

### CAD-6: Rust Type Safety Patterns

**Decision**: The networking stack leverages Rust's type system to eliminate entire classes of bugs at compile time. This is not cosmetic — byte-order bugs, address/port mixups, and state-machine violations are the top three sources of networking bugs in C kernels.

**Newtype wrappers** (zero-cost at runtime via `#[repr(transparent)]`):

```rust
/// IPv4 address in network byte order. Cannot be confused with a raw u32.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    pub const UNSPECIFIED: Self = Self([0, 0, 0, 0]);
    pub const BROADCAST: Self = Self([255, 255, 255, 255]);
    pub const LOCALHOST: Self = Self([127, 0, 0, 1]);

    pub fn from_u32_be(val: u32) -> Self { Self(val.to_be_bytes()) }
    pub fn to_u32_be(self) -> u32 { u32::from_be_bytes(self.0) }
    pub fn is_loopback(&self) -> bool { self.0[0] == 127 }
    pub fn is_broadcast(&self) -> bool { self == &Self::BROADCAST }
    pub fn is_multicast(&self) -> bool { self.0[0] >= 224 && self.0[0] <= 239 }
}

/// Port number in host byte order. Separate type prevents mixing with raw u16.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Port(pub u16);

impl Port {
    pub const fn new(val: u16) -> Self { Self(val) }
    pub fn to_network_bytes(self) -> [u8; 2] { self.0.to_be_bytes() }
    pub fn from_network_bytes(bytes: [u8; 2]) -> Self { Self(u16::from_be_bytes(bytes)) }
    pub fn is_ephemeral(&self) -> bool { self.0 >= 49152 }
    pub fn is_privileged(&self) -> bool { self.0 < 1024 }
}

/// MAC address. Distinct type prevents confusion with other 6-byte arrays.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    pub const BROADCAST: Self = Self([0xff; 6]);
    pub const ZERO: Self = Self([0; 6]);
    pub fn is_broadcast(&self) -> bool { self == &Self::BROADCAST }
    pub fn is_multicast(&self) -> bool { self.0[0] & 0x01 != 0 }
}

/// Device index. Cannot be confused with a socket index or connection ID.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DevIndex(pub usize);
```

**Network byte order safety**: Instead of remembering to call `htons()`/`ntohs()`, the type system enforces correct byte ordering. `Ipv4Addr` is always in network byte order (stored as `[u8; 4]`). `Port` is always in host byte order. Conversion is explicit and named. You literally cannot pass a host-order value where network-order is expected — the compiler catches it.

**Comprehensive error type** (not raw errno):

```rust
pub enum NetError {
    WouldBlock,            // EAGAIN/EWOULDBLOCK
    ConnectionRefused,     // ECONNREFUSED
    ConnectionReset,       // ECONNRESET
    ConnectionAborted,     // ECONNABORTED
    TimedOut,              // ETIMEDOUT
    AddressInUse,          // EADDRINUSE
    AddressNotAvailable,   // EADDRNOTAVAIL
    NotConnected,          // ENOTCONN
    AlreadyConnected,      // EISCONN
    NetworkUnreachable,    // ENETUNREACH
    HostUnreachable,       // EHOSTUNREACH
    PermissionDenied,      // EPERM
    InvalidArgument,       // EINVAL
    NoBufferSpace,         // ENOBUFS
    ProtocolNotSupported,  // EPROTONOSUPPORT
    AddressFamilyNotSupported, // EAFNOSUPPORT
    SocketNotBound,        // EINVAL (bind not called)
    InProgress,            // EINPROGRESS (nonblocking connect)
    OperationNotSupported, // EOPNOTSUPP
    Shutdown,              // EPIPE (write after shutdown)
}

impl NetError {
    /// Convert to POSIX errno at the syscall boundary. Internal code uses NetError.
    pub fn to_errno(&self) -> i32 { ... }
}
```

**Socket address abstraction**:

```rust
pub struct SockAddr {
    pub ip: Ipv4Addr,
    pub port: Port,
}

impl SockAddr {
    /// Parse from raw userspace SockAddrIn bytes with validation.
    pub fn from_user(raw: &SockAddrIn) -> Result<Self, NetError> { ... }
    /// Serialize to userspace-visible SockAddrIn layout.
    pub fn to_user(&self) -> SockAddrIn { ... }
}
```

**Rationale**: Every `u32` that's really an IP address, every `u16` that's really a port, and every `i32` that's really an error code is a bug waiting to happen. Rust's newtype pattern eliminates these bugs at zero runtime cost. Linux can't do this because C doesn't have newtypes. smoltcp uses similar patterns (`wire::Ipv4Address`, `wire::EthernetAddress`). We follow smoltcp's lead here.

---

## 1. Phase 1: Net Core Contracts (PacketBuf + NetDev + Type Foundation)

> **Establishes the three foundational abstractions that every subsequent phase builds on: type-safe network primitives, pool-backed packet buffers, and the device trait.**
> **Kernel changes required**: Yes — new `drivers/src/net/types.rs`, `drivers/src/net/pool.rs`, `drivers/src/net/packetbuf.rs`, `drivers/src/net/netdev.rs`, refactor `drivers/src/virtio_net.rs`
> **Difficulty**: Medium-High
> **Depends on**: Nothing

### Background

Right now, the VirtIO driver is the network stack. It allocates raw byte buffers from its DMA ring, parses Ethernet headers inline, dispatches ARP and IPv4 by hand, and calls into TCP/UDP directly. There is no separation between "driver that moves bytes" and "stack that understands protocols."

Phase 1 introduces type-safe network primitives (CAD-6), a pool-backed `PacketBuf` (CAD-1), the `NetDevice` trait with decoupled registry (CAD-2), and a single ingress demux path. It then refactors VirtIO-net to implement `NetDevice` and wires up `net_rx(dev, pkt)`. Nothing else changes — no new protocol support, no new syscalls. Just the contracts.

### 1A: Network Type Foundation

Define the type-safe primitives that all networking code will use.

- [x] **1A.1** Create `drivers/src/net/types.rs` with newtype wrappers:
  - `Ipv4Addr([u8; 4])` — IPv4 address in network byte order with associated constants (`UNSPECIFIED`, `BROADCAST`, `LOCALHOST`) and methods (`is_loopback()`, `is_broadcast()`, `is_multicast()`, `is_unspecified()`, `in_subnet(addr, mask) -> bool`)
  - `Port(u16)` — port number in host byte order with methods (`to_network_bytes()`, `from_network_bytes()`, `is_ephemeral()`, `is_privileged()`)
  - `MacAddr([u8; 6])` — MAC address with constants (`BROADCAST`, `ZERO`) and methods (`is_broadcast()`, `is_multicast()`)
  - `DevIndex(usize)` — device index newtype
  - All types: derive `Clone, Copy, PartialEq, Eq, Hash`; implement `Debug` with human-readable formatting (e.g., `192.168.1.1` not `Ipv4Addr([192, 168, 1, 1])`)
- [x] **1A.2** Create `NetError` enum in `drivers/src/net/types.rs`:
  - All variants listed in CAD-6 above
  - `to_errno(&self) -> i32` conversion method for the syscall boundary
  - `impl core::fmt::Display` for human-readable error messages in logs
  - Document which POSIX errno each variant maps to
- [x] **1A.3** Create `SockAddr` struct in `drivers/src/net/types.rs`:
  - Fields: `ip: Ipv4Addr`, `port: Port`
  - `from_user(raw: &SockAddrIn) -> Result<Self, NetError>` — validates `sin_family == AF_INET`, converts byte order
  - `to_user(&self) -> SockAddrIn` — serializes to userspace-visible layout
  - This is the single conversion point between kernel and userspace address representations
- [x] **1A.4** Create `EtherType` and `IpProtocol` enums in `drivers/src/net/types.rs`:
  - `EtherType { Ipv4 = 0x0800, Arp = 0x0806, Ipv6 = 0x86DD }` — with `from_u16()` that returns `Option`
  - `IpProtocol { Icmp = 1, Tcp = 6, Udp = 17 }` — with `from_u8()` that returns `Option`
  - Pattern matching on these enums replaces raw numeric comparisons throughout the stack

### 1B: Packet Buffer Pool

Implement the pool allocator and `PacketBuf` type.

- [x] **1B.1** Create `drivers/src/net/pool.rs` with `PacketPool`:
  - Pre-allocates `POOL_SIZE` (256 default) buffers of `BUF_SIZE` (2048) bytes each at `CACHE_LINE_ALIGN` (64-byte) alignment
  - `alloc() -> Option<PacketBuf>` — pops a free slot from the freelist, returns `None` if pool exhausted
  - `release(slot: u16)` — pushes slot back onto the freelist (called by `PacketBuf::drop`)
  - Freelist is a lock-free stack (atomic CAS on a head index) to allow alloc/release from interrupt context
  - `available() -> usize` — number of free buffers (for diagnostics)
  - The pool is a `static` global, initialized once at kernel boot before networking starts
- [x] **1B.2** Create `drivers/src/net/packetbuf.rs` with `PacketBuf` struct:
  - Fields as specified in CAD-1: `inner: PacketBufInner`, `head: u16`, `tail: u16`, `l2_offset: u16`, `l3_offset: u16`, `l4_offset: u16`
  - `PacketBufInner::Pooled { pool: &'static PacketPool, slot: u16 }` for normal packets
  - `PacketBufInner::Oversized { data: Vec<u8> }` for reassembly (Phase 8 only)
  - `HEADROOM` constant: 128 bytes (Ethernet 14 + IP 20 + TCP 60 + 34 spare)
  - Implement `Drop` to return pooled buffers automatically
  - Do NOT derive `Clone` — packets are move-only
  - Implement `Debug` manually to show metadata without dumping buffer contents
- [x] **1B.3** Implement `PacketBuf` constructors:
  - `PacketBuf::alloc() -> Option<Self>` — allocates from pool, sets `head = HEADROOM`, `tail = HEADROOM`
  - `PacketBuf::from_raw_copy(data: &[u8]) -> Option<Self>` — allocates from pool, copies data, sets `head = 0`, `tail = data.len()` — used by RX path when copying from DMA buffer
  - `PacketBuf::oversized(capacity: usize) -> Self` — allocates from heap for reassembly buffers only
- [x] **1B.4** Implement header push/pull methods:
  - `push_header(&mut self, len: usize) -> Result<&mut [u8], NetError>` — extends head backward into headroom, returns `Err(NoBufferSpace)` if insufficient headroom
  - `pull_header(&mut self, len: usize) -> Result<&[u8], NetError>` — advances head forward, returns consumed bytes, returns `Err(InvalidArgument)` if `len > self.len()`
  - `payload(&self) -> &[u8]` — returns `data[head..tail]`
  - `payload_mut(&mut self) -> &mut [u8]` — mutable variant
  - `len(&self) -> usize` — returns `tail - head`
  - `append(&mut self, data: &[u8]) -> Result<(), NetError>` — extends tail, copies data
- [x] **1B.5** Implement layer offset helpers:
  - `set_l2(&mut self, offset: u16)`, `l2_header(&self) -> &[u8]` — slice from l2_offset to l3_offset
  - `set_l3(&mut self, offset: u16)`, `l3_header(&self) -> &[u8]` — slice from l3_offset to l4_offset
  - `set_l4(&mut self, offset: u16)`, `l4_header(&self) -> &[u8]` — slice from l4_offset to tail
  - Each returns `&[]` if offsets are not yet set (offset == 0 and not the first layer)
- [x] **1B.6** Implement checksum helpers on `PacketBuf`:
  - `compute_ipv4_checksum(&self) -> u16` — computes over L3 header bytes
  - `compute_tcp_checksum(&self, src: Ipv4Addr, dst: Ipv4Addr) -> u16` — pseudo-header + L4
  - `compute_udp_checksum(&self, src: Ipv4Addr, dst: Ipv4Addr) -> u16` — pseudo-header + L4
  - Document: software checksum is always computed. If `NetDeviceFeatures::CHECKSUM_TX` is set, the driver may offload but the stack does not skip computation (simplicity over performance for now)

### 1C: NetDevice Trait and Registry

- [x] **1C.1** Create `drivers/src/net/netdev.rs` with the `NetDevice` trait:
  - `fn tx(&mut self, pkt: PacketBuf) -> Result<(), NetError>` — transmit one packet
  - `fn poll_rx(&mut self, budget: usize, pool: &'static PacketPool) -> SmallVec<[PacketBuf; 8]>` — drain up to `budget` received packets, allocating from `pool`
  - `fn set_up(&mut self)` and `fn set_down(&mut self)` — link state control
  - `fn mtu(&self) -> u16` — maximum transmission unit
  - `fn mac(&self) -> MacAddr` — hardware MAC address
  - `fn stats(&self) -> NetDeviceStats` — read-only stats snapshot
  - `fn features(&self) -> NetDeviceFeatures` — capability flags
- [x] **1C.2** Add `NetDeviceStats` struct:
  - Fields: `rx_packets: u64`, `tx_packets: u64`, `rx_bytes: u64`, `tx_bytes: u64`, `rx_errors: u64`, `tx_errors: u64`, `rx_dropped: u64`, `tx_dropped: u64`
- [x] **1C.3** Add `NetDeviceFeatures` bitflags:
  - `CHECKSUM_TX` — driver can compute TX checksums
  - `CHECKSUM_RX` — driver has verified RX checksums
  - `TSO` — TCP segmentation offload (reserved, not implemented)
  - `VLAN_TAG` — driver strips/inserts VLAN tags (reserved)
- [x] **1C.4** Create `NetDeviceRegistry` and `DeviceHandle` in `drivers/src/net/netdev.rs`:
  - `NetDeviceRegistry`: spinlock-protected `Vec<Option<Box<dyn NetDevice>>>` — control plane only
  - `register(dev: Box<dyn NetDevice>) -> DeviceHandle` — assigns index, returns stable handle
  - `unregister(index: DevIndex)` — calls `set_down()`, invalidates handle, frees slot
  - `enumerate() -> Vec<(DevIndex, MacAddr, bool)>` — for `ifconfig` display
  - `DeviceHandle` as specified in CAD-2: stable pointer + per-device TX lock
  - `DeviceHandle::tx()` acquires only the per-device TX lock, not the registry lock
  - `DeviceHandle::poll_rx()` requires no lock (single consumer: NAPI loop)

### 1D: VirtIO-Net Refactor and Ingress Pipeline

Wire VirtIO-net to implement `NetDevice` and build the single ingress demux path.

- [x] **1D.1** Refactor `drivers/src/virtio_net.rs` to implement `NetDevice`:
  - Move DMA ring management, feature negotiation, and MSI-X setup into the struct
  - Implement `tx()` by enqueuing a `PacketBuf` into the TX virtqueue
  - Implement `poll_rx()` by draining the RX virtqueue: for each DMA buffer, call `PacketBuf::from_raw_copy(dma_data)` using the provided pool
  - Remove all protocol parsing from the driver — it becomes a pure byte mover
  - Negotiate `VIRTIO_NET_F_CSUM` and `VIRTIO_NET_F_GUEST_CSUM` features; map to `NetDeviceFeatures`
- [x] **1D.2** Create `drivers/src/net/ingress.rs` with `net_rx(handle: &DeviceHandle, pkt: PacketBuf)`:
  - Parse Ethernet header using `EtherType::from_u16()`, set `l2_offset`
  - Check destination MAC: accept if matches our MAC, broadcast, or multicast
  - Dispatch: `EtherType::Arp` to `arp::handle_rx()`, `EtherType::Ipv4` to `ipv4::handle_rx()`
  - Drop unknown ethertypes with a stat increment, no panic
  - This function is the single entry point for all received packets
- [x] **1D.3** Create `drivers/src/net/ipv4.rs` with `ipv4::handle_rx(dev: DevIndex, pkt: PacketBuf)`:
  - Validate IP version (must be 4), header length (≥20), total length (≤ packet size)
  - Verify IP header checksum (unless `CHECKSUM_RX` feature is set)
  - Set `l3_offset`, extract protocol field using `IpProtocol::from_u8()`
  - Dispatch: `IpProtocol::Udp` to `udp::handle_rx()`, `IpProtocol::Tcp` to `tcp::handle_rx()`, `IpProtocol::Icmp` to `icmp::handle_rx()` (stub in Phase 1)
  - Drop unknown protocols, increment stats
  - Drop packets with TTL=0 (do not forward yet)
- [x] **1D.4** Move existing ARP handling from `virtio_net.rs` into `drivers/src/net/arp.rs`:
  - `arp::handle_rx(dev: DevIndex, pkt: PacketBuf)` — stub that logs and drops for now
  - `arp::send_request(dev: DevIndex, target_ip: Ipv4Addr)` — stub
  - Phase 2 will fill these in; Phase 1 just establishes the module boundary
- [x] **1D.5** Update the NAPI poll loop in `drivers/src/net/napi.rs`:
  - Obtain `DeviceHandle` once at registration, hold it for the device's lifetime
  - Call `handle.poll_rx(budget, &PACKET_POOL)` to get packets
  - Feed each packet to `net_rx(handle, pkt)`
  - Remove the old inline dispatch that called into TCP/UDP directly
- [x] **1D.6** Update `drivers/src/net/mod.rs` to declare all new submodules:
  - `pub mod types`, `pub mod pool`, `pub mod packetbuf`, `pub mod netdev`, `pub mod ingress`, `pub mod ipv4`, `pub mod arp`
  - Re-export key types: `Ipv4Addr`, `Port`, `MacAddr`, `DevIndex`, `NetError`, `SockAddr`, `PacketBuf`, `NetDevice`, `DeviceHandle`

### Phase 1 Test Coverage

- [x] **1.T1** Unit test `PacketPool::alloc` + `release`: alloc fills pool, release restores, alloc again succeeds
- [x] **1.T2** Unit test `PacketBuf::alloc`: verify `payload()` is empty, headroom is accessible
- [x] **1.T3** Unit test `push_header` / `pull_header`: push 14 bytes (Ethernet), verify offset and slice correctness
- [x] **1.T4** Unit test `PacketBuf::from_raw_copy`: verify `payload()` returns full buffer, offsets are zero
- [x] **1.T5** Unit test `PacketBuf` drop: allocate, drop, verify pool slot is returned (pool.available() increases)
- [x] **1.T6** Unit test `Ipv4Addr` methods: `is_loopback`, `is_broadcast`, `in_subnet`, byte conversions
- [x] **1.T7** Unit test `Port` byte-order conversions: `to_network_bytes()` round-trips correctly
- [x] **1.T8** Unit test `NetDeviceStats` accumulation: increment fields, verify reads
- [x] **1.T9** Integration test: boot with VirtIO-net refactored, verify DHCP still completes
- [x] **1.T10** Integration test: send a UDP packet from userland, verify it reaches the ingress pipeline
- [x] **1.T11** Verify `DeviceHandle::tx()` does not acquire the registry lock (code review / assert)

### Phase 1 Gate

- [x] **GATE**: `PacketBuf` compiles with no warnings under `#![no_std]`, pool alloc/free are O(1)
- [x] **GATE**: `NetDevice` trait is implemented by `VirtioNet` and passes all method calls
- [x] **GATE**: `net_rx()` ingress path handles ARP and IPv4 dispatch without panicking on malformed input
- [x] **GATE**: Existing DHCP flow still works end-to-end after VirtIO-net refactor
- [x] **GATE**: No protocol parsing remains in `drivers/src/virtio_net.rs`
- [x] **GATE**: All networking code uses `Ipv4Addr`, `Port`, `MacAddr` — no raw `u32`/`u16` for addresses

---

## 2. Phase 2: Timer Infrastructure and ARP Neighbor Cache v1

> **Introduces the data-driven timer wheel and replaces the static ARP table with a dynamic neighbor cache.**
> **Kernel changes required**: Yes — new `drivers/src/net/timer.rs`, new `drivers/src/net/neighbor.rs`, ARP state machine
> **Difficulty**: Medium
> **Depends on**: Phase 1

### Background

The current ARP table is a static array inside `virtio_net.rs`. A real neighbor cache tracks entry state: `Incomplete` (ARP request sent, waiting), `Reachable` (reply received, fresh), `Stale` (old but usable), `Failed` (no reply after retries). Entries age out and are retried. This requires a timer infrastructure that Phase 2 also introduces.

### 2A: Data-Driven Timer Wheel

- [ ] **2A.1** Create `drivers/src/net/timer.rs` with `NetTimerWheel`:
  - 256 slots, each slot is a `Vec<TimerEntry>` (not linked list — simpler, cache-friendly)
  - `TimerEntry` struct as specified in CAD-4: `deadline_tick`, `kind: TimerKind`, `key: u32`, `token: TimerToken`, `cancelled: bool`
  - `next_token: AtomicU64` — monotonically increasing token generator
  - `current_tick: u64` — current position in the wheel
- [ ] **2A.2** Implement `NetTimerWheel::schedule(delay_ticks: u64, kind: TimerKind, key: u32) -> TimerToken`:
  - Computes `deadline_tick = current_tick + delay_ticks`
  - Assigns a unique `TimerToken` from `next_token`
  - Inserts `TimerEntry` into slot `deadline_tick % 256`
  - Returns the token for cancellation
- [ ] **2A.3** Implement `NetTimerWheel::cancel(token: TimerToken) -> bool`:
  - Scans the entry's slot (O(slot_size), bounded by `MAX_ENTRIES_PER_SLOT`)
  - Marks the entry as `cancelled = true`
  - Returns `true` if found, `false` if already fired or not found
  - Alternative: maintain a `HashSet<TimerToken>` of cancelled tokens for O(1) cancel, checked on fire
- [ ] **2A.4** Implement `NetTimerWheel::tick(subsystems: &mut NetSubsystems)`:
  - Called from the timer interrupt handler, advances `current_tick`
  - Drains entries from `slots[current_tick % 256]` where `deadline_tick <= current_tick`
  - Skips entries with `cancelled == true`
  - Dispatches each entry via `match entry.kind { ... }` as specified in CAD-4
  - Bounds per-tick work: if more than `MAX_TIMERS_PER_TICK` (32) fire, defer remainder
  - Protect with spinlock; document constraint: dispatch handlers must not re-acquire the timer lock
- [ ] **2A.5** Wire `NetTimerWheel::tick()` into the existing timer interrupt handler:
  - The timer interrupt already fires at a known rate; add a call to `net_timer_wheel.tick()`
  - `NetSubsystems` is a struct holding `&mut NeighborCache`, `&mut TcpEngine`, `&mut ReassemblyBuffer` — passed by reference to avoid global state in timer callbacks

### 2B: Neighbor Cache State Machine

- [ ] **2B.1** Create `drivers/src/net/neighbor.rs` with `NeighborState` enum:
  - `Incomplete { retries: u8, pending: SmallVec<[PacketBuf; 4]> }` — ARP sent, packets queued (bounded at 4)
  - `Reachable { mac: MacAddr, confirmed_at: u64 }` — fresh entry
  - `Stale { mac: MacAddr, last_used: u64 }` — old but usable, will re-probe on next use
  - `Failed` — no reply after max retries, drop queued packets
- [ ] **2B.2** Implement `NeighborEntry` and `NeighborCache`:
  - `NeighborEntry { ip: Ipv4Addr, state: NeighborState, timer_token: Option<TimerToken> }`
  - `NeighborCache` keyed by `(DevIndex, Ipv4Addr)` — per-interface from day one (CAD-5)
  - Fixed capacity of 256 entries with LRU eviction (evict oldest `Stale` first, then oldest `Reachable`)
  - `lookup(dev: DevIndex, ip: Ipv4Addr) -> Option<MacAddr>` — returns MAC if `Reachable` or `Stale`
  - `insert_or_update(dev: DevIndex, ip: Ipv4Addr, mac: MacAddr)` — called when ARP reply arrives
  - Each entry has an `entry_id: u32` used as the timer `key`
- [ ] **2B.3** Implement `NeighborCache::resolve(dev: DevIndex, ip: Ipv4Addr, pkt: PacketBuf) -> Result<(), NetError>`:
  - If `Reachable` or `Stale`: set destination MAC in `pkt`, call `dev.tx(pkt)`, if `Stale` schedule re-probe
  - If `Incomplete`: push `pkt` onto pending queue (drop if queue full, award L), return `Ok(())`
  - If absent: create `Incomplete` entry, queue `pkt`, call `arp::send_request(dev, ip)`, schedule `ArpRetransmit` timer
  - If `Failed`: drop `pkt`, return `Err(HostUnreachable)`, award an L
- [ ] **2B.4** Implement timer-driven state transitions:
  - On `insert_or_update`: schedule `ArpExpire` timer for `REACHABLE_TIME` (30s), store token in entry
  - `on_expire(entry_id)`: transition `Reachable` to `Stale`, cancel old timer
  - On `Stale` entry use: schedule `ArpRetransmit` timer for `STALE_PROBE_TIME` (5s)
  - `on_retransmit(entry_id)`: if `Incomplete` and `retries < MAX_RETRIES` (3): resend ARP request, increment retries, reschedule. If `retries >= MAX_RETRIES`: transition to `Failed`, drop pending packets, award L for each

### 2C: ARP Protocol Handler

- [ ] **2C.1** Implement `arp::handle_rx(dev: DevIndex, pkt: PacketBuf)` in `drivers/src/net/arp.rs`:
  - Parse ARP header: validate `htype=1` (Ethernet), `ptype=0x0800` (IPv4), `hlen=6`, `plen=4`
  - On ARP reply (`oper=2`): call `NeighborCache::insert_or_update(dev, sender_ip, sender_mac)`, flush pending packets
  - On ARP request (`oper=1`) for our IP: send ARP reply with our MAC
  - On any ARP: opportunistically update cache if sender is already known (RFC 826)
  - Drop malformed ARP silently, increment error stat
- [ ] **2C.2** Implement `arp::send_request(dev: DevIndex, target_ip: Ipv4Addr)`:
  - Allocate `PacketBuf::alloc()`, fill Ethernet header: dst = `MacAddr::BROADCAST`, src = our MAC
  - Fill ARP header: opcode = REQUEST, sender = our IP/MAC, target = `target_ip`, target MAC = `MacAddr::ZERO`
  - Call `dev_handle.tx(pkt)`
- [ ] **2C.3** Wire `NeighborCache` into the IPv4 egress path:
  - `ipv4::send(dev: DevIndex, dst_ip: Ipv4Addr, pkt: PacketBuf)` calls `NeighborCache::resolve(dev, next_hop, pkt)`
  - Next-hop is `dst_ip` for now (routing table comes in Phase 3)

### Phase 2 Test Coverage

- [ ] **2.T1** Unit test `NeighborCache::lookup` on empty cache returns `None`
- [ ] **2.T2** Unit test `insert_or_update` followed by `lookup` returns correct MAC
- [ ] **2.T3** Unit test `Incomplete` state: queued packets are flushed when reply arrives
- [ ] **2.T4** Unit test `Failed` state: packets are dropped, W/L loss is awarded
- [ ] **2.T5** Unit test timer wheel: schedule a timer, advance ticks past deadline, verify dispatch fires with correct `kind` and `key`
- [ ] **2.T6** Unit test timer cancellation: cancel before deadline, verify dispatch does not fire
- [ ] **2.T7** Unit test timer `MAX_TIMERS_PER_TICK` bound: schedule 64 timers for same tick, verify only 32 fire per tick call
- [ ] **2.T8** Integration test: boot, send UDP to a new host, verify ARP request appears on wire, reply resolves the entry
- [ ] **2.T9** Integration test: ARP entry ages to `Stale` after timeout, re-probe is sent on next use

### Phase 2 Gate

- [ ] **GATE**: ARP request/reply cycle completes for a new host without manual cache population
- [ ] **GATE**: Packets queued during `Incomplete` state are transmitted after reply arrives
- [ ] **GATE**: Timer wheel fires entries within one tick of their deadline with correct `TimerKind`/`key`
- [ ] **GATE**: Timer cancellation prevents firing (no stale callbacks)
- [ ] **GATE**: No static ARP table remains in `drivers/src/virtio_net.rs`
- [ ] **GATE**: Neighbor cache is keyed by `(DevIndex, Ipv4Addr)`, not a global map

---

## 3. Phase 3: L3 Config, Loopback, and Routing v1

> **Gives each interface an explicit IPv4 configuration and adds a routing table with prefix-length-bucketed LPM, plus a loopback device.**
> **Kernel changes required**: Yes — new `drivers/src/net/route.rs`, `drivers/src/net/loopback.rs`, `drivers/src/net/netstack.rs`
> **Difficulty**: Medium
> **Depends on**: Phase 2

### 3A: Per-Interface IPv4 Configuration

- [ ] **3A.1** Create `drivers/src/net/netstack.rs` with `IfaceConfig` struct:
  - Fields: `dev_index: DevIndex`, `ipv4_addr: Ipv4Addr`, `netmask: Ipv4Addr`, `gateway: Ipv4Addr`, `dns: [Ipv4Addr; 2]`, `up: bool`
  - `broadcast(&self) -> Ipv4Addr` — computed from addr and netmask
  - `is_local(&self, ip: Ipv4Addr) -> bool` — true if ip is on the directly connected subnet
  - `prefix_len(&self) -> u8` — count leading ones in netmask
- [ ] **3A.2** Create `NetStack` struct in `drivers/src/net/netstack.rs`:
  - Owns a `Vec<IfaceConfig>` (one per registered netdev)
  - `configure(dev: DevIndex, addr: Ipv4Addr, netmask: Ipv4Addr, gateway: Ipv4Addr, dns: [Ipv4Addr; 2])` — called by DHCP on lease
  - `iface_for_dev(dev: DevIndex) -> Option<&IfaceConfig>` — lookup by device index
  - `our_ip(dev: DevIndex) -> Option<Ipv4Addr>` — convenience accessor
  - `is_our_addr(ip: Ipv4Addr) -> bool` — checks all interfaces (needed for RX path multi-NIC)
- [ ] **3A.3** Update `drivers/src/net/dhcp.rs` to call `NetStack::configure()` on ACK:
  - Remove any ad-hoc IP storage from the DHCP module
  - Pass parsed subnet, router, and DNS fields into `NetStack`
  - Trigger route table update: add connected route and default route after configuration

### 3B: Routing Table v1

- [ ] **3B.1** Create `drivers/src/net/route.rs` with `RouteEntry` struct:
  - Fields: `prefix: Ipv4Addr`, `prefix_len: u8`, `gateway: Ipv4Addr` (`UNSPECIFIED` = directly connected), `dev: DevIndex`, `metric: u32`
  - `matches(&self, dst: Ipv4Addr) -> bool` — checks if dst falls within prefix/prefix_len
  - `next_hop(&self, dst: Ipv4Addr) -> Ipv4Addr` — returns gateway if non-zero, else dst (directly connected)
- [ ] **3B.2** Implement `RouteTable` with prefix-length-bucketed LPM:
  - Structure: `buckets: [SmallVec<[RouteEntry; 4]>; 33]` — index 0 = /0, index 32 = /32
  - `add(entry: RouteEntry)` — inserts into `buckets[entry.prefix_len]`, sorted by metric within bucket
  - `remove(prefix: Ipv4Addr, prefix_len: u8)` — removes matching entry
  - `lookup(dst: Ipv4Addr) -> Option<(DeviceHandle, Ipv4Addr)>` — iterates from /32 to /0, returns first match
  - O(32) worst case per lookup regardless of route count (vs O(n) for flat scan)
- [ ] **3B.3** Wire `RouteTable::lookup()` into `ipv4::send()`:
  - Replace any hardcoded `dev_index = 0` with a route lookup
  - If no route found: drop packet, log warning, return `Err(NetworkUnreachable)`, award an L
  - Pass `(DeviceHandle, next_hop)` to `NeighborCache::resolve()`
  - Source IP selection: use the address of the outgoing interface from `NetStack::our_ip(dev)`

### 3C: Loopback Device

- [ ] **3C.1** Create `drivers/src/net/loopback.rs` with `LoopbackDev` implementing `NetDevice`:
  - `tx()` pushes the packet onto an internal `VecDeque<PacketBuf>` (bounded at 256)
  - `poll_rx()` drains up to `budget` packets from the queue
  - `mtu()` returns 65535, `mac()` returns `MacAddr::ZERO`
  - `features()` returns `CHECKSUM_TX | CHECKSUM_RX` (loopback never needs checksum computation)
- [ ] **3C.2** Register `LoopbackDev` at kernel init before VirtIO-net:
  - It gets `DevIndex(0)` by convention
  - Configure it with `127.0.0.1/8`, no gateway, no DNS
  - Add connected route `127.0.0.0/8 -> DevIndex(0)` to the route table
- [ ] **3C.3** Verify loopback delivery in `net_rx()`:
  - Packets received on the loopback device go through the same `net_rx()` ingress path
  - No special-casing needed if the ingress path is correct

### 3D: Internal IoSlice Abstraction

- [ ] **3D.1** Create `IoSlice` and `IoSliceMut` in `drivers/src/net/types.rs`:
  - `pub struct IoSlice<'a> { buf: &'a [u8] }` — immutable scatter slice
  - `pub struct IoSliceMut<'a> { buf: &'a mut [u8] }` — mutable scatter slice
  - All internal send/recv protocol APIs accept `&[IoSlice<'_>]` starting from Phase 4
  - This allows vectored I/O and prepares for future zero-copy paths without rewriting protocol code

### Phase 3 Test Coverage

- [ ] **3.T1** Unit test `RouteTable::lookup` with connected route: dst on subnet returns correct DevIndex
- [ ] **3.T2** Unit test `RouteTable::lookup` with default route: dst off subnet returns gateway
- [ ] **3.T3** Unit test `RouteTable::lookup` with no routes: returns `None`
- [ ] **3.T4** Unit test prefix-length bucketing: /24 beats /16 for a matching address
- [ ] **3.T5** Unit test metric tie-breaking: lower metric wins within same prefix length
- [ ] **3.T6** Integration test: send UDP to `127.0.0.1`, verify packet is delivered locally without hitting VirtIO
- [ ] **3.T7** Integration test: DHCP lease populates `NetStack` and route table correctly
- [ ] **3.T8** Integration test: `ifconfig` shell app reads `IfaceConfig` and displays correct IP/netmask/gateway

### Phase 3 Gate

- [ ] **GATE**: `RouteTable::lookup()` returns correct `(DeviceHandle, next_hop)` for all route types in O(32)
- [ ] **GATE**: Loopback device delivers packets to `127.0.0.1` without touching VirtIO-net
- [ ] **GATE**: DHCP lease populates `NetStack` and triggers route table update
- [ ] **GATE**: `ipv4::send()` uses route lookup, not hardcoded device index
- [ ] **GATE**: No IP address storage remains outside `NetStack`

---

## 4. Phase 4: Socket Framework and UDP Sockets v1

> **Builds the complete socket framework (setsockopt, getsockopt, shutdown, growable table, ephemeral ports) and implements UDP as the first protocol on top.**
> **Kernel changes required**: Yes — rewrite `drivers/src/net/socket.rs`, new `drivers/src/net/udp.rs`, update `core/src/syscall/net_handlers.rs`, update `abi/src/net.rs`
> **Difficulty**: Medium-High
> **Depends on**: Phase 3

### Background

The existing `socket.rs` has `KernelSocket`, `UdpReceiveQueue`, and `SocketTable`, but the demux logic is fragile and the socket abstraction doesn't cleanly separate UDP from TCP. There's no ephemeral port allocator, no `setsockopt`/`getsockopt`, no `shutdown()`, and no `SO_REUSEADDR`.

Phase 4 introduces the entire socket framework from CAD-3: enum-dispatched `SocketInner`, growable `SocketTable` (slab), separate protocol demux tables, `setsockopt`/`getsockopt` syscalls, `shutdown()` syscall, and `SO_REUSEADDR`. UDP gets a clean implementation on top.

### 4A: Socket Framework

- [ ] **4A.1** Rewrite `drivers/src/net/socket.rs` with the new `Socket` struct:
  - `inner: SocketInner` (enum: `Udp(UdpSocket)`, `Tcp(TcpSocket)` placeholder, `Raw(RawSocket)` placeholder)
  - `state: SocketState` enum: `Unbound`, `Bound`, `Connected`, `Listening`, `Closed`
  - `flags: SocketFlags` (bitflags: `O_NONBLOCK`)
  - `options: SocketOptions` struct as specified in CAD-3
  - `local_addr: Option<SockAddr>`, `remote_addr: Option<SockAddr>`
  - `recv_queue: BoundedQueue<(PacketBuf, SockAddr)>` — capacity from `options.recv_buf_size`
  - `wait_queue: WaitQueue` — stub for Phase 6 (just a `bool` flag for now)
- [ ] **4A.2** Implement `SocketTable` as a slab allocator:
  - `slots: Vec<Option<Socket>>` + `freelist: Vec<usize>` — O(1) alloc/free
  - Initial capacity 64, grows by doubling when freelist is empty (up to `MAX_SOCKETS=1024`)
  - `alloc(inner: SocketInner) -> Option<usize>` — finds free slot, initializes Socket, returns index
  - `get(idx: usize) -> Option<&Socket>` and `get_mut(idx: usize) -> Option<&mut Socket>`
  - `free(idx: usize)` — runs cleanup (release port, cancel timers), sets slot to `None`, pushes to freelist
  - Protect with spinlock; document that the lock must not be held across blocking operations
- [ ] **4A.3** Implement ephemeral port allocator in `drivers/src/net/socket.rs`:
  - Range: 49152..65535 (IANA dynamic/private ports) — 16384 ports
  - `alloc_ephemeral() -> Option<Port>` — round-robin scan using atomic `next_port` counter, skip ports in use
  - `release_port(port: Port)` — marks port as available
  - Track allocated ports in a 2048-byte bitmap (16384 bits)
  - Use `Port` newtype — not raw `u16`
- [ ] **4A.4** Implement `SocketOptions` defaults and validation:
  - `recv_buf_size`: default 16384, min 256, max 262144
  - `send_buf_size`: default 16384, min 256, max 262144
  - `reuse_addr`: default false
  - `recv_timeout` / `send_timeout`: default None (infinite block)
  - `keepalive`: default false
  - `tcp_nodelay`: default false

### 4B: setsockopt / getsockopt / shutdown Syscalls

- [ ] **4B.1** Add `SYSCALL_SETSOCKOPT` to `abi/src/syscall.rs`:
  - Signature: `setsockopt(fd: i32, level: i32, optname: i32, optval: *const u8, optlen: u32) -> i32`
  - Levels: `SOL_SOCKET = 1`, `IPPROTO_TCP = 6`
  - Socket-level options: `SO_REUSEADDR = 2`, `SO_RCVBUF = 8`, `SO_SNDBUF = 7`, `SO_RCVTIMEO = 20`, `SO_SNDTIMEO = 21`, `SO_KEEPALIVE = 9`
  - TCP-level options: `TCP_NODELAY = 1` (Phase 5+)
- [ ] **4B.2** Add `SYSCALL_GETSOCKOPT` to `abi/src/syscall.rs`:
  - Signature: `getsockopt(fd: i32, level: i32, optname: i32, optval: *mut u8, optlen: *mut u32) -> i32`
  - Returns the current value of the specified option
  - Add `SO_ERROR = 4` — retrieves and clears the pending socket error (used after async connect, ICMP errors)
- [ ] **4B.3** Add `SYSCALL_SHUTDOWN` to `abi/src/syscall.rs`:
  - Signature: `shutdown(fd: i32, how: i32) -> i32`
  - `SHUT_RD = 0` — disallow further receives; incoming data is discarded
  - `SHUT_WR = 1` — disallow further sends; for TCP, sends FIN
  - `SHUT_RDWR = 2` — both
  - Returns `Err(NotConnected)` if socket is not connected (UDP or unconnected TCP)
- [ ] **4B.4** Implement `handle_setsockopt()` in `core/src/syscall/net_handlers.rs`:
  - Copy `optval` from userspace with bounds checking
  - Dispatch on `(level, optname)` to modify `Socket.options`
  - Validate values: `SO_RCVBUF`/`SO_SNDBUF` must be in [min, max] range, resize `recv_queue`/`send_queue` capacity
  - Return `Err(InvalidArgument)` for unknown options (don't silently ignore)
- [ ] **4B.5** Implement `handle_getsockopt()` in `core/src/syscall/net_handlers.rs`:
  - Copy current option value to userspace
  - `SO_ERROR`: read and clear the pending error from `Socket.pending_error`
- [ ] **4B.6** Implement `handle_shutdown()` in `core/src/syscall/net_handlers.rs`:
  - For UDP: `SHUT_RD` clears recv queue and marks read-shutdown, `SHUT_WR` marks write-shutdown, `SHUT_RDWR` does both
  - For TCP: `SHUT_WR` initiates FIN sequence (Phase 5 fills in), `SHUT_RD` discards incoming data
  - Set appropriate `SocketState` flags
- [ ] **4B.7** Add userland wrappers in `userland/src/syscall/net.rs`:
  - `setsockopt(fd, level, optname, val: &[u8]) -> Result<(), i32>`
  - `getsockopt(fd, level, optname, buf: &mut [u8]) -> Result<usize, i32>`
  - `shutdown(fd, how: i32) -> Result<(), i32>`
  - Convenience: `set_reuse_addr(fd) -> Result<(), i32>`, `set_nonblocking(fd) -> Result<(), i32>`

### 4C: UDP Socket Implementation

- [ ] **4C.1** Create `drivers/src/net/udp.rs` with `UdpSocket` struct:
  - No internal state beyond what `Socket` provides (UDP is stateless at protocol level)
  - Implements protocol-specific methods called via enum dispatch on `SocketInner::Udp`
- [ ] **4C.2** Implement UDP bind:
  - `udp_bind(sock: &mut Socket, addr: SockAddr) -> Result<(), NetError>`
  - Validates address: `INADDR_ANY` or a local interface address
  - If port is 0: allocate ephemeral port
  - Check `SO_REUSEADDR` before rejecting `AddressInUse`
  - Register in `UdpDemuxTable`, transition state to `Bound`
- [ ] **4C.3** Implement `UdpDemuxTable` in `drivers/src/net/udp.rs`:
  - Maps `(Ipv4Addr, Port)` to socket index — separate from `SocketTable`
  - `register(local_ip: Ipv4Addr, local_port: Port, sock_idx: usize)` — called by `bind()`
  - `lookup(dst_ip: Ipv4Addr, dst_port: Port) -> Option<usize>` — called by `udp::handle_rx()`
  - Wildcard: `INADDR_ANY` matches any destination IP
  - Protected by its own spinlock (not the socket table lock)
- [ ] **4C.4** Implement `udp::handle_rx(dev: DevIndex, pkt: PacketBuf)`:
  - Parse UDP header: src port, dst port, length, checksum (all using `Port` newtype)
  - Validate checksum if `!dev.features().contains(CHECKSUM_RX)` and checksum != 0
  - Look up socket via `UdpDemuxTable::lookup()`
  - If no socket found: send ICMP port unreachable (stub until Phase 8), drop, award L
  - Push `(pkt, sender_addr)` onto `sock.recv_queue`; drop if queue full (award L, increment `rx_dropped`)
  - Wake the socket's wait queue with `READABLE` (effective in Phase 6)
- [ ] **4C.5** Implement `udp_sendto(sock: &Socket, slices: &[IoSlice<'_>], dst: SockAddr) -> Result<usize, NetError>`:
  - Allocate `PacketBuf`, compute total payload length from IoSlices
  - If total > MTU - IP_HEADER - UDP_HEADER: return `Err(InvalidArgument)` (fragmentation is Phase 8)
  - Fill UDP header, compute checksum (using `PacketBuf::compute_udp_checksum`)
  - Fill IP header, call `ipv4::send()` via routing
  - Return total bytes sent
- [ ] **4C.6** Implement `udp_recvfrom(sock: &mut Socket) -> Result<(PacketBuf, SockAddr), NetError>`:
  - Pop from `sock.recv_queue`
  - If queue empty and `O_NONBLOCK`: return `Err(WouldBlock)`
  - If queue empty and blocking: return `Err(WouldBlock)` (Phase 6 adds actual blocking)
  - Return `(packet, sender_address)`

### 4D: Syscall Integration

- [ ] **4D.1** Update `handle_socket()` in `core/src/syscall/net_handlers.rs`:
  - `socket(AF_INET, SOCK_DGRAM, 0)` allocates via `SocketTable::alloc(SocketInner::Udp(UdpSocket::new()))`
  - Returns socket index as file descriptor
  - Return `Err(AddressFamilyNotSupported)` for non-AF_INET, `Err(ProtocolNotSupported)` for unknown type
- [ ] **4D.2** Update `handle_bind()`, `handle_connect()`, `handle_sendto()`, `handle_recvfrom()`:
  - Copy `SockAddrIn` from userspace, validate via `SockAddr::from_user()`
  - Dispatch to protocol-specific methods via `match sock.inner { ... }`
  - Set `NetError::to_errno()` and return -1 on error; return bytes transferred on success
- [ ] **4D.3** Stabilize `SockAddrIn` layout in `abi/src/net.rs`:
  - `sin_family: u16`, `sin_port: u16` (network byte order), `sin_addr: u32` (network byte order), `sin_zero: [u8; 8]`
  - Total size: 16 bytes (matches POSIX)
  - Document: port and addr are in network byte order in the ABI, but `SockAddr` stores them in their newtype wrappers
- [ ] **4D.4** Update `userland/src/syscall/net.rs` wrappers:
  - Ensure `sendto` and `recvfrom` wrappers handle `SockAddrIn` ↔ `SockAddr` conversion
  - Add `udp_echo_test()` helper for integration testing
  - Add convenience wrappers: `bind_any(fd, port)`, `bind_addr(fd, ip, port)`

### Phase 4 Test Coverage

- [ ] **4.T1** Unit test slab allocator: alloc 100 sockets, free 50, alloc 50 more — verify no collisions
- [ ] **4.T2** Unit test ephemeral port allocator: allocate all ports, verify no duplicates, verify exhaustion returns `None`
- [ ] **4.T3** Unit test UDP demux: register two sockets on different ports, verify correct dispatch
- [ ] **4.T4** Unit test INADDR_ANY: socket bound to `0.0.0.0` receives packets on any local IP
- [ ] **4.T5** Unit test receive queue overflow: push beyond capacity, verify packet dropped (not panic)
- [ ] **4.T6** Unit test `SO_REUSEADDR`: bind to port in use without flag fails, with flag succeeds
- [ ] **4.T7** Unit test `SO_RCVBUF`: set to 256, verify queue capacity changes
- [ ] **4.T8** Unit test `shutdown(SHUT_RD)`: subsequent recv returns error, send still works
- [ ] **4.T9** Integration test: `socket(AF_INET, SOCK_DGRAM)` + `bind` + `recvfrom` receives UDP from QEMU SLIRP
- [ ] **4.T10** Integration test: `sendto` sends UDP, verify it appears on wire
- [ ] **4.T11** Integration test: `setsockopt(SO_REUSEADDR)` + `bind` succeeds after previous socket closed

### Phase 4 Gate

- [ ] **GATE**: `socket()` + `bind()` + `sendto()` + `recvfrom()` round-trip works end-to-end
- [ ] **GATE**: `setsockopt` and `getsockopt` modify and read all specified options correctly
- [ ] **GATE**: `shutdown()` disables the specified direction without closing the socket
- [ ] **GATE**: `SO_REUSEADDR` allows rebinding to a port with connections in TIME_WAIT
- [ ] **GATE**: Ephemeral port allocator never returns a duplicate within a single boot
- [ ] **GATE**: UDP demux correctly routes packets to the right socket via separate `UdpDemuxTable`
- [ ] **GATE**: `SocketTable` grows beyond initial capacity of 64 when needed
- [ ] **GATE**: `SockAddrIn` ABI is 16 bytes and stable

---

## 5. Phase 5: TCP Stream Sockets v1

> **Connects the existing TCP state machine to the socket layer with a two-queue listen model, proper shutdown(), and timer integration.**
> **Kernel changes required**: Yes — new `drivers/src/net/tcp_socket.rs`, update `drivers/src/net/tcp.rs`, update syscall handlers
> **Difficulty**: High
> **Depends on**: Phase 4

### Background

The TCP state machine in `drivers/src/net/tcp.rs` is a solid RFC 793 implementation. What it doesn't have is a clean mapping to the socket layer, a two-queue listen model, or integration with the socket options framework from Phase 4. Phase 5 builds the bridge.

### 5A: Two-Queue Listen Model

Linux uses a SYN queue (half-open connections) separate from the accept queue (fully established connections). This prevents SYN floods from blocking legitimate connections.

- [ ] **5A.1** Create `SynRecvEntry` struct in `drivers/src/net/tcp_socket.rs`:
  - Fields: `remote: SockAddr`, `local: SockAddr`, `iss: u32` (initial send seq), `irs: u32` (initial recv seq), `retries: u8`, `timer_token: TimerToken`, `timestamp: u64`
  - This represents a connection in `SYN_RECEIVED` state, not yet established
  - Bounded at `SYN_QUEUE_MAX` (128) entries — separate from the accept backlog
- [ ] **5A.2** Create `TcpListenState` struct:
  - `syn_queue: HashMap<(Ipv4Addr, Port, Ipv4Addr, Port), SynRecvEntry>` — keyed by 4-tuple
  - `accept_queue: VecDeque<usize>` — completed connection IDs, capacity = listen backlog
  - `backlog: usize` — maximum accept queue size from `listen()`
  - When a SYN arrives: create `SynRecvEntry`, send SYN-ACK, schedule `TcpRetransmit` timer
  - When the final ACK arrives (completing 3WHS): move to `accept_queue`, create `TcpConnection`
  - If `syn_queue` is full: drop SYN silently (do NOT send RST — that helps attackers)
  - If `accept_queue` is full: keep connection in `syn_queue` until space opens or timeout
- [ ] **5A.3** Implement SYN-ACK retransmission:
  - On `TcpRetransmit` timer for a SYN_RECEIVED entry: retransmit SYN-ACK, increment `retries`
  - After `SYN_RETRIES_MAX` (5): remove from `syn_queue`, no RST (silent drop)
  - Use exponential backoff: 1s, 2s, 4s, 8s, 16s

### 5B: TCP Socket and PCB Mapping

- [ ] **5B.1** Create `TcpSocket` struct in `drivers/src/net/tcp_socket.rs`:
  - Fields: `conn_id: Option<u32>` (index into `TcpConnection` table), `listen: Option<TcpListenState>`
  - Implements protocol-specific methods called via `SocketInner::Tcp`
- [ ] **5B.2** Add `socket_idx: Option<usize>` field to `TcpConnection` in `drivers/src/net/tcp.rs`:
  - Bidirectional link: socket → connection via `conn_id`, connection → socket via `socket_idx`
  - When connection transitions to `Established`, set `socket_idx`
  - When connection closes, clear `socket_idx` and wake the socket's wait queue
- [ ] **5B.3** Implement `TcpDemuxTable` in `drivers/src/net/tcp_socket.rs`:
  - Maps `(local_ip, local_port, remote_ip, remote_port)` to connection ID — 4-tuple lookup
  - Also maps `(local_ip, local_port)` to listening socket index — 2-tuple listener lookup
  - `lookup_established(4-tuple) -> Option<u32>` and `lookup_listener(2-tuple) -> Option<usize>` are separate
  - Separate from the `SocketTable` — this is protocol-level demux
- [ ] **5B.4** Update `tcp::handle_rx()` to use `TcpDemuxTable`:
  - First: `lookup_established()` for existing connections
  - Then: `lookup_listener()` for SYN on listening sockets → create SynRecvEntry
  - Fallback: send RST for unexpected segments (unless RST flag is already set)

### 5C: Listen, Accept, Connect

- [ ] **5C.1** Implement `tcp_bind(sock, addr)` and `tcp_listen(sock, backlog)`:
  - `bind()`: register in `TcpDemuxTable` as listener
  - `listen(backlog)`: create `TcpListenState` with specified backlog, transition to `Listening`
  - Minimum backlog: 1, maximum: 128
- [ ] **5C.2** Implement `tcp_accept(sock) -> Result<usize, NetError>`:
  - Dequeue a connection ID from `accept_queue`
  - Create new `Socket` with `TcpSocket` pointing to the dequeued connection
  - Allocate via `SocketTable::alloc()`, return new socket index
  - If queue empty: return `Err(WouldBlock)` (blocking in Phase 6)
- [ ] **5C.3** Implement `tcp_connect(sock, addr) -> Result<(), NetError>`:
  - Allocate ephemeral local port if not bound
  - Create `TcpConnection` in `SynSent` state, send SYN
  - Register in `TcpDemuxTable`
  - Return `Err(InProgress)` — connect is inherently asynchronous, blocking is Phase 6
- [ ] **5C.4** Wire completed 3WHS into accept queue:
  - When `TcpConnection` transitions to `Established` via SYN-ACK (client) or final ACK (server):
    - Server side: move `SynRecvEntry` to `accept_queue`, create `TcpConnection`, set `socket_idx`
    - Client side: transition connect socket to `Connected`, wake with `WRITABLE`
  - If accept queue full: keep in SYN queue (don't RST, don't drop established connections)

### 5D: Send, Recv, Close, Shutdown

- [ ] **5D.1** Implement `tcp_send(sock, slices: &[IoSlice<'_>]) -> Result<usize, NetError>`:
  - Check `shutdown` flags: if write-shutdown, return `Err(Shutdown)`
  - Copy data from IoSlices into `TcpConnection`'s send buffer
  - Call `tcp::try_send()` to push data onto the wire if window allows
  - Return bytes accepted (may be less than requested if buffer full)
  - If buffer full and `O_NONBLOCK`: return `Err(WouldBlock)` with partial count
- [ ] **5D.2** Implement `tcp_recv(sock, buf: &mut [u8]) -> Result<usize, NetError>`:
  - Check `shutdown` flags: if read-shutdown, return `Ok(0)` (EOF)
  - Copy data from `TcpConnection`'s receive buffer
  - Advance receive window, send ACK if window update is significant
  - Return 0 on FIN (EOF), `Err(WouldBlock)` if buffer empty and `O_NONBLOCK`
- [ ] **5D.3** Implement shutdown for TCP:
  - `shutdown(SHUT_WR)`: send FIN, transition to `FinWait1`, set write-shutdown flag. Socket remains open for reading.
  - `shutdown(SHUT_RD)`: set read-shutdown flag, discard any buffered received data, further received data is ACKed but discarded
  - `close()`: calls `shutdown(RDWR)`, then decrements ref count. If last ref, full connection teardown.
- [ ] **5D.4** Implement FIN handling:
  - Receiving FIN: transition to `CloseWait`, deliver buffered data, then `recv()` returns 0 (EOF)
  - Wake socket with `READABLE | HUP`
  - After userspace reads EOF and calls `close()`: send FIN (`LastAck` state)

### 5E: Timer Integration for TCP

- [ ] **5E.1** Wire TCP retransmit timer into `NetTimerWheel`:
  - On segment TX: `timer_wheel.schedule(rto_ticks, TcpRetransmit, conn_id)`
  - On ACK receipt: `timer_wheel.cancel(token)` using stored `TimerToken`
  - `on_retransmit(conn_id)`: validate connection still exists, retransmit unacknowledged segment, double RTO
- [ ] **5E.2** Wire TCP TIME_WAIT timer:
  - On entering `TimeWait`: `timer_wheel.schedule(2_MSL_TICKS, TcpTimeWait, conn_id)` (2*MSL = 4s)
  - `on_time_wait_expire(conn_id)`: transition to `Closed`, release connection slot, release port if `!SO_REUSEADDR`

### 5F: Syscall Updates

- [ ] **5F.1** Update `handle_socket(AF_INET, SOCK_STREAM)`:
  - Allocates `SocketTable::alloc(SocketInner::Tcp(TcpSocket::new()))`
- [ ] **5F.2** Add `handle_listen()`, `handle_accept()` syscall handlers:
  - `listen(fd, backlog)` → `tcp_listen(sock, backlog)`
  - `accept(fd) -> i32` → `tcp_accept(sock)`, returns new FD or error
- [ ] **5F.3** Update `handle_send()` and `handle_recv()`:
  - Stream path: no address argument (unlike `sendto`/`recvfrom`)
  - Dispatch to `tcp_send` / `tcp_recv` via `SocketInner::Tcp`
- [ ] **5F.4** Add userland wrappers: `listen()`, `accept()`, `send()`, `recv()` in `userland/src/syscall/net.rs`

### Phase 5 Test Coverage

- [ ] **5.T1** Unit test two-queue model: fill SYN queue, verify SYN is silently dropped (no RST)
- [ ] **5.T2** Unit test accept queue overflow: backlog=2, complete 3 connections, verify 3rd stays in SYN queue
- [ ] **5.T3** Unit test SYN-ACK retransmission: SYN received, no ACK, verify SYN-ACK retransmitted 5 times with backoff
- [ ] **5.T4** Unit test FIN handling: send FIN, verify `recv()` returns 0 after data drained
- [ ] **5.T5** Unit test `shutdown(SHUT_WR)`: sends FIN but `recv()` still works
- [ ] **5.T6** Unit test `shutdown(SHUT_RD)`: subsequent `recv()` returns 0, incoming data discarded
- [ ] **5.T7** Unit test retransmit timer: send segment, don't ACK, verify retransmit fires with correct `conn_id`
- [ ] **5.T8** Unit test TIME_WAIT: verify connection slot released after 2*MSL
- [ ] **5.T9** Integration test: TCP client connects to QEMU SLIRP, sends "hello", receives echo
- [ ] **5.T10** Integration test: TCP server `listen()` + `accept()` + `recv()` + `send()` round-trip
- [ ] **5.T11** Integration test: connection teardown via FIN, verify clean close on both sides

### Phase 5 Gate

- [ ] **GATE**: TCP 3-way handshake completes via `socket()` + `connect()` or `listen()` + `accept()`
- [ ] **GATE**: Two-queue model: SYN floods do not fill the accept queue
- [ ] **GATE**: `send()` and `recv()` move data through the TCP state machine correctly
- [ ] **GATE**: `shutdown(SHUT_WR)` sends FIN without closing the read side
- [ ] **GATE**: FIN delivers EOF to userspace, `recv()` returns 0
- [ ] **GATE**: Retransmit timer fires and retransmits unacknowledged segments
- [ ] **GATE**: TIME_WAIT releases connection slots after 2*MSL
- [ ] **GATE**: `SO_REUSEADDR` allows binding during TIME_WAIT

---

## 6. Phase 6: Blocking/Nonblocking and poll/select

> **Adds O_NONBLOCK, socket timeouts, and a poll() syscall so userspace can wait on multiple sockets without spinning.**
> **Kernel changes required**: Yes — new `core/src/wait.rs`, new `SYSCALL_POLL`, update socket layer
> **Difficulty**: Medium-High
> **Depends on**: Phase 5

### 6A: Wait Queue Implementation

- [ ] **6A.1** Create `core/src/wait.rs` with `WaitQueue` struct:
  - A list of `(task_id: usize, readiness_mask: ReadinessMask)` pairs
  - `wait(&mut self, task_id: usize, mask: ReadinessMask)` — adds task to queue, yields CPU
  - `wake_all(&mut self, mask: ReadinessMask)` — wakes all tasks waiting on matching bits
  - `wake_one(&mut self, mask: ReadinessMask)` — wakes first matching task (for accept)
  - Design for level-triggered semantics (not edge-triggered) — simpler, matches `poll()` semantics, epoll upgrade path is via separate `EPOLLET` flag later
- [ ] **6A.2** Define `ReadinessMask` bitflags:
  - `READABLE = 0x01` — data available to read, or connection ready to accept
  - `WRITABLE = 0x02` — send buffer has space, or connection established
  - `ERROR = 0x04` — error condition (RST, connection refused)
  - `HUP = 0x08` — connection closed by remote (FIN received)
- [ ] **6A.3** Wire `WaitQueue::wake_all()` into the socket layer:
  - UDP `handle_rx()`: wake `READABLE` after pushing to recvq
  - TCP `Established` transition: wake connecting socket with `WRITABLE`
  - TCP data RX: wake `READABLE`
  - TCP FIN RX: wake `READABLE | HUP`
  - TCP RST RX: wake `ERROR`
  - TCP accept queue push: wake listening socket with `READABLE`

### 6B: O_NONBLOCK and Socket Timeouts

- [ ] **6B.1** Implement `O_NONBLOCK` via setsockopt/fcntl:
  - Add `SYSCALL_FCNTL` with `F_GETFL`/`F_SETFL` support, or use `setsockopt` with a `SO_NONBLOCK` option
  - All blocking operations check the flag: if set, return `Err(WouldBlock)` instead of blocking
- [ ] **6B.2** Implement `SO_RCVTIMEO` and `SO_SNDTIMEO` blocking behavior:
  - Already stored in `SocketOptions` from Phase 4
  - Blocking `recv()`: schedule a wakeup timer via `NetTimerWheel`, block on wait queue
  - On timeout: return `Err(TimedOut)`
  - On data arrival: cancel timeout timer, return data
- [ ] **6B.3** Update `handle_recv()` and `handle_send()` for blocking:
  - If socket is blocking and operation would block: call `WaitQueue::wait()`, yield
  - On wakeup: retry the operation
  - If `O_NONBLOCK`: return `Err(WouldBlock)` immediately
  - If timeout set: use timed wait

### 6C: poll() Syscall

- [ ] **6C.1** Define `PollFd` in `abi/src/net.rs`:
  - `fd: i32`, `events: u16` (requested), `revents: u16` (returned)
  - `POLLIN = 0x0001`, `POLLOUT = 0x0004`, `POLLERR = 0x0008`, `POLLHUP = 0x0010`, `POLLNVAL = 0x0020`
- [ ] **6C.2** Add `SYSCALL_POLL`:
  - `poll(fds: *mut PollFd, nfds: usize, timeout_ms: i32) -> i32`
  - Returns count of fds with non-zero `revents`, 0 on timeout, -1 on error
  - `timeout_ms = -1`: block indefinitely, `timeout_ms = 0`: non-blocking check
- [ ] **6C.3** Implement `handle_poll()`:
  - Copy `PollFd` array from userspace
  - For each fd: check current readiness (POLLIN if recvq non-empty, POLLOUT if send buffer has space, etc.)
  - If any fd ready: fill revents, copy back, return count
  - If none ready and timeout != 0: register task on all relevant wait queues, yield
  - On wakeup: re-check all fds, fill revents, return
  - `POLLNVAL` for invalid fds (don't fail the whole call, just set the flag)
- [ ] **6C.4** Add `poll()` wrapper to `userland/src/syscall/net.rs`

### Phase 6 Test Coverage

- [ ] **6.T1** Unit test `WaitQueue`: register two tasks, wake both, verify both scheduled
- [ ] **6.T2** Unit test `O_NONBLOCK`: `recv()` on empty socket returns `WouldBlock`
- [ ] **6.T3** Unit test `SO_RCVTIMEO`: blocking recv returns `TimedOut` after timeout
- [ ] **6.T4** Integration test: `poll()` on UDP socket blocks until packet arrives
- [ ] **6.T5** Integration test: `poll()` on two sockets, data on one, verify only that fd has `POLLIN`
- [ ] **6.T6** Integration test: `poll(timeout=0)` returns immediately
- [ ] **6.T7** Integration test: TCP `connect()` with `O_NONBLOCK` returns `InProgress`, `poll(POLLOUT)` wakes on connect

### Phase 6 Gate

- [ ] **GATE**: `O_NONBLOCK` sockets return `WouldBlock` on all blocking operations
- [ ] **GATE**: Blocking `recv()` wakes on data arrival (no busy-wait)
- [ ] **GATE**: `poll()` returns correct `revents` for `POLLIN`, `POLLOUT`, `POLLERR`, `POLLHUP`
- [ ] **GATE**: `SO_RCVTIMEO` returns `TimedOut` after the specified interval
- [ ] **GATE**: No busy-wait loops remain in any socket operation

---

## 7. Phase 7: Userspace DNS Resolver

> **Moves DNS resolution out of the kernel into a userspace library using UDP sockets.**
> **Kernel changes required**: Minimal — kernel keeps DNS server list, compatibility syscall preserved
> **Difficulty**: Medium
> **Depends on**: Phase 6

### 7A: Kernel DNS Server List

- [ ] **7A.1** Verify DHCP populates `NetStack::IfaceConfig::dns` correctly from Phase 3
  - Add `SYSCALL_NET_DNS_SERVERS` that copies the DNS server list to userspace
  - Signature: `net_dns_servers(buf: *mut u32, count: usize) -> i32`
- [ ] **7A.2** Add `net_dns_servers()` wrapper to `userland/src/syscall/net.rs`

### 7B: Userspace Resolver Library

- [ ] **7B.1** Create `userland/src/net/dns.rs` with `DnsResolver`:
  - Opens UDP socket, calls `net_dns_servers()` to get server IPs
  - 32-entry LRU cache keyed by hostname, stores `(Ipv4Addr, ttl_expires: u64)`
- [ ] **7B.2** Implement `DnsResolver::resolve(hostname: &str) -> Result<Ipv4Addr, DnsError>`:
  - Check cache first, return if TTL valid
  - Build DNS query (A record), `sendto()` first DNS server, `poll()` with 3s timeout
  - Parse response, update cache
  - On timeout: try second DNS server
  - Handle CNAME chains (up to 5 hops)
- [ ] **7B.3** Implement DNS packet builder/parser
- [ ] **7B.4** Create hosts table: `localhost -> 127.0.0.1` checked before DNS

### 7C: Compatibility

- [ ] **7C.1** Keep `SYSCALL_RESOLVE` (135) as kernel fallback for early boot
- [ ] **7C.2** Update `resolve` shell command to use userspace `DnsResolver`
- [ ] **7C.3** Document: new code should use `DnsResolver`, not the kernel syscall

### Phase 7 Gate

- [ ] **GATE**: `DnsResolver::resolve()` works via UDP DNS query
- [ ] **GATE**: DNS cache prevents duplicate queries within TTL
- [ ] **GATE**: `SYSCALL_RESOLVE` still works for backward compatibility
- [ ] **GATE**: `localhost` resolves via hosts table without network query
- [ ] **GATE**: No DNS parsing in kernel hot path

---

## 8. Phase 8: IPv4 Robustness and TCP Hardening

> **Adds IPv4 fragmentation/reassembly, ICMP error handling, and TCP timer hardening.**
> **Kernel changes required**: Yes — new `drivers/src/net/fragment.rs`, `drivers/src/net/icmp.rs`, updates to TCP
> **Difficulty**: High
> **Depends on**: Phase 5

### 8A: IPv4 Fragmentation TX

- [ ] **8A.1** Implement `ipv4::fragment_and_send(dev, dst_ip, pkt, mtu)`:
  - If payload fits in MTU: send directly
  - Otherwise: split into fragments with correct MF flag and fragment offset (8-byte units)
  - Set DF bit based on socket option (default: set DF for TCP, clear for UDP)
- [ ] **8A.2** Wire into `ipv4::send()`, look up MTU from device

### 8B: IPv4 Reassembly RX

- [ ] **8B.1** Create `ReassemblyBuffer` keyed by `(src_ip, dst_ip, protocol, identification)`:
  - Sorted fragment list, returns reassembled packet when complete
  - Uses `PacketBuf::oversized()` for reassembly buffers (may exceed pool buffer size)
- [ ] **8B.2** Resource limits: max 8 concurrent groups, max 64KB per group, 30s timeout via `ReassemblyTimeout` timer
- [ ] **8B.3** Wire into `ipv4::handle_rx()`: check MF/offset, pass to reassembly

### 8C: ICMP Error Handling

- [ ] **8C.1** Implement `icmp::handle_rx(dev, pkt)`:
  - Type 3 (destination unreachable): extract embedded header, set `SO_ERROR` on matching socket, wake with `ERROR`
  - Type 3/Code 4 (fragmentation needed): extract next-hop MTU for path MTU discovery
  - Type 11 (time exceeded): log, notify socket
  - Drop other types (echo reply is Phase 9 via raw sockets)
- [ ] **8C.2** Implement `icmp::send_unreachable(dev, original_pkt, code)`:
  - Build ICMP type 3 with original IP header + first 8 bytes of transport
  - Called from `udp::handle_rx()` when no socket found

### 8D: TCP Timer Hardening

- [ ] **8D.1** RTT estimation (RFC 6298): track `srtt`, `rttvar` per connection, compute `RTO = max(1s, srtt + 4*rttvar)`
- [ ] **8D.2** Exponential backoff: double RTO per retransmit (max 60s), close after 5 retransmits with `TimedOut`
- [ ] **8D.3** Nagle's algorithm: hold small writes if unACKed data exists, flush on `TCP_NODELAY` (via `setsockopt`)
- [ ] **8D.4** TCP keepalive: `SO_KEEPALIVE` via `setsockopt`, probe every 75s after 2h idle, close after 9 failed probes. Uses `TcpKeepalive` timer kind.
- [ ] **8D.5** TCP MSS clamping: set MSS to `min(announced_mss, mtu - 40)` to prevent fragmentation

### Phase 8 Gate

- [ ] **GATE**: Fragmentation TX produces correct headers for oversized payloads
- [ ] **GATE**: Reassembly reconstructs from out-of-order fragments
- [ ] **GATE**: Reassembly groups dropped after 30s (no memory leak)
- [ ] **GATE**: ICMP unreachable sent for UDP to closed port
- [ ] **GATE**: TCP RTO uses RTT estimation, not fixed value
- [ ] **GATE**: Nagle respects `TCP_NODELAY` set via `setsockopt`

---

## 9. Phase 9: Multi-NIC, Packet Filter, Raw Sockets

> **Extends the stack to multiple interfaces, adds packet filtering, and introduces raw sockets.**
> **Kernel changes required**: Yes — updates to registry, new filter.rs, new raw_socket.rs
> **Difficulty**: Very High
> **Depends on**: Phase 8

### 9A: Multi-NIC Routing and ARP

- [ ] **9A.1** Verify `NetDeviceRegistry` handles multiple devices (per-interface ARP, routes)
- [ ] **9A.2** Update `ipv4::handle_rx()`: check dst against all local addresses via `NetStack::is_our_addr()`
- [ ] **9A.3** Source IP selection: use outgoing interface's address when socket is bound to `INADDR_ANY`
- [ ] **9A.4** Update `ifconfig` to display all interfaces with per-interface stats

### 9B: Packet Filter

- [ ] **9B.1** Create `FilterRule` struct: 5-tuple (`src_ip`, `dst_ip`, `src_port`, `dst_port`, `protocol`) with `Option` wildcards, `FilterAction::Accept | Drop`
- [ ] **9B.2** `FilterChain` with three hooks: `PREROUTING`, `INPUT`, `OUTPUT`
  - `Vec<FilterRule>` per chain, first match wins, default accept
  - Linear evaluation — O(n) per packet, acceptable for <50 rules
- [ ] **9B.3** Wire hooks: `net_rx()` → PREROUTING, `ipv4::handle_rx()` → INPUT, `ipv4::send()` → OUTPUT
- [ ] **9B.4** Add `SYSCALL_FILTER_ADD` and `SYSCALL_FILTER_FLUSH` (privileged)

### 9C: Raw Sockets

- [ ] **9C.1** `RawSocket` as `SocketInner::Raw` — added to the enum
  - `socket(AF_INET, SOCK_RAW, protocol)` creates for specific IP protocol
  - `send()`: takes IP payload, kernel prepends IP header
  - `recv()`: delivers complete IP packets including header
- [ ] **9C.2** `CAP_NET_RAW` capability check on creation, return `Err(PermissionDenied)` without
- [ ] **9C.3** Wire into `ipv4::handle_rx()`: after TCP/UDP dispatch, deliver copy to matching raw sockets
- [ ] **9C.4** `ping.rs` userland app: raw ICMP echo via `SOCK_RAW` + `poll()` + RTT display

### Phase 9 Gate

- [ ] **GATE**: Two interfaces route independently with separate ARP caches
- [ ] **GATE**: Filter DROP rules prevent delivery to sockets
- [ ] **GATE**: Raw socket send/recv works for ICMP
- [ ] **GATE**: Raw socket creation blocked without `CAP_NET_RAW`
- [ ] **GATE**: `ping.rs` sends echo, receives reply via raw socket

---

## Dependency Graph

```
Phase 1: Net Core Contracts (PacketBuf + NetDev + Types)
    |
    v
Phase 2: Timer Infrastructure + ARP Neighbor Cache v1
    |
    v
Phase 3: L3 Config + Loopback + Routing v1
    |
    v
Phase 4: Socket Framework + UDP Sockets v1
    |   (setsockopt, getsockopt, shutdown, SO_REUSEADDR, growable table)
    v
Phase 5: TCP Stream Sockets v1
    |   (two-queue listen, shutdown integration)
    |
    +---> Phase 6: Blocking/Nonblocking + poll/select
    |         |
    |         v
    |     Phase 7: Userspace DNS Resolver
    |
    v
Phase 8: IPv4 Robustness + TCP Hardening
    |   (setsockopt: TCP_NODELAY, SO_KEEPALIVE)
    v
Phase 9: Multi-NIC + Packet Filter + Raw Sockets
```

Notes:

- Phase 6 depends on Phase 5 (needs TCP socket layer for blocking connect/accept)
- Phase 7 depends on Phase 6 (needs poll() for DNS timeout handling)
- Phase 8 depends on Phase 5 (needs TCP state machine for timer hardening + setsockopt framework for TCP_NODELAY)
- Phase 9 depends on Phase 8 (needs robust IPv4 before adding multi-NIC)
- Phases 6+7 and Phase 8 can be developed in parallel after Phase 5 completes
- setsockopt/getsockopt framework is introduced in Phase 4 and used by Phases 5, 6, 8

---

## Future Considerations

The nine phases above bring SlopOS to a functional, BSD-socket-compatible IPv4 stack. What comes after is a longer road, and none of it is in scope for this plan.

**IPv6**: The neighbor cache (Phase 2) and routing table (Phase 3) were designed with per-interface scoping specifically to make IPv6 NDP and routing tables addable without a rewrite. `AF_INET6` would be a new address family. The `SockAddr` struct would gain a `V6` variant. NDP replaces ARP. The `SocketInner` enum gains protocol-specific IPv6 variants or the existing ones become dual-stack. Estimated effort: one full plan of similar scope.

**TLS**: Not a kernel concern. A userspace library (`rustls` adaptation for `no_std` or minimal TLS 1.3) sits on top of TCP stream sockets. The kernel provides the socket; TLS is the application's problem.

**HTTP**: Same as TLS. Userspace HTTP client/server on top of TCP sockets.

**epoll**: Phase 6 adds `poll()`. `epoll` is the scalable evolution. It requires: `epoll_create()` returning an epoll FD, `epoll_ctl()` for add/modify/delete, `epoll_wait()` for event retrieval. The `WaitQueue` infrastructure from Phase 6 is the foundation. Design note: ensure wait queues support both level-triggered (default) and edge-triggered (`EPOLLET`) semantics from the start — this is a `ReadinessMask` configuration, not a structural change.

**WiFi**: A new `NetDevice` implementation. The 802.11 state machine lives in a driver. The rest of the stack (ARP, routing, sockets) is unchanged. The `NetDevice` trait boundary from Phase 1 makes this possible.

**VLANs**: 802.1Q tagging is a thin `NetDevice` wrapper that strips/inserts tags.

**Bonding and bridging**: Multiple `NetDevice` instances aggregated into a logical interface.

**DNSSEC**: Extension to Phase 7's userspace resolver. No kernel changes.

**Packet capture (pcap)**: A raw socket variant using Phase 9's filter hooks.

**Multicast and IGMP**: Currently absent. The plan explicitly handles broadcast (ARP) but drops multicast. Adding IGMP requires a multicast group table per interface and join/leave socket options (`IP_ADD_MEMBERSHIP`). This is a Phase 9+ feature — the `setsockopt` framework from Phase 4 is the right place to add these options.

**Path MTU Discovery**: Phase 8 adds DF-bit handling and ICMP "fragmentation needed" reception. Full PMTUD requires caching path MTU per destination and reducing segment size accordingly. The `NeighborCache` from Phase 2 is the natural place to store PMTU alongside MAC addresses.

The wizards' work is never done. The Wheel of Fate keeps spinning.

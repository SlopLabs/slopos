# SlopOS Networking Evolution Plan

> **Status**: Planned — All phases pending
> **Target**: Evolve SlopOS networking from functional prototype to architecturally sound, BSD-socket-compatible TCP/IP stack
> **Scope**: Packet buffers, netdev abstraction, ARP, routing, BSD sockets (UDP+TCP), I/O multiplexing, userspace DNS, IPv4 hardening, multi-NIC, packet filtering

---

## Table of Contents

1. [Executive Summary](#executive-summary)
2. [Current State Assessment](#current-state-assessment)
3. [Critical Architectural Decisions](#critical-architectural-decisions)
4. [Phase 1: Net Core Contracts (PacketBuf + NetDev)](#1-phase-1-net-core-contracts-packetbuf--netdev)
5. [Phase 2: ARP Neighbor Cache v1](#2-phase-2-arp-neighbor-cache-v1)
6. [Phase 3: L3 Config, Loopback, and Routing v1](#3-phase-3-l3-config-loopback-and-routing-v1)
7. [Phase 4: Userspace UDP Sockets v1](#4-phase-4-userspace-udp-sockets-v1)
8. [Phase 5: Userspace TCP Stream Sockets v1](#5-phase-5-userspace-tcp-stream-sockets-v1)
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

| Current Component | Status | Future State | Phase |
|---|---|---|---|
| `drivers/src/net/mod.rs` — Ethernet/IP parsing | Functional, no abstraction | Replaced by PacketBuf + demux pipeline | 1 |
| `drivers/src/virtio_net.rs` — VirtIO driver | Hardcoded dispatch, no trait | Implements `NetDevice` trait | 1 |
| ARP table in `virtio_net.rs` | Static, hardcoded | Dynamic neighbor cache with state machine | 2 |
| IPv4 config | Implicit, DHCP-only | Per-interface config struct, routing table | 3 |
| Loopback | Absent | `lo` netdev, 127.0.0.1/8, local delivery | 3 |
| `drivers/src/net/socket.rs` — UDP | Functional, fragile demux | Clean socket layer with proper ABI | 4 |
| `drivers/src/net/tcp.rs` — TCP | RFC 793 state machine, no socket integration | Full PCB-to-socket mapping, blocking wakeups | 5 |
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

What's missing: a `PacketBuf` type with headroom, a `NetDevice` trait, a dynamic ARP cache, a routing table, a loopback device, `O_NONBLOCK`, `poll()`/`select()`, ICMP error handling, IPv4 fragmentation/reassembly, multi-NIC routing, and packet filtering. The DNS resolver lives in the kernel when it should live in userspace.

---

## Critical Architectural Decisions

These five decisions have rewrite-danger if made wrong. They must be settled before or during Phase 1, and subsequent phases must not contradict them.

### CAD-1: PacketBuf Model

**Decision**: Linear-only buffers for now. Reserve a `frags` field in the struct for future scatter-gather, but do not implement it. All header access goes through offset fields (`l2_offset`, `l3_offset`, `l4_offset`) stored in the `PacketBuf`. Ownership is move-only — no `Arc<PacketBuf>` in the fast path.

**Rationale**: Scatter-gather adds complexity that the current single-NIC, low-throughput use case does not justify. The reserved field means we don't have to break the ABI when we add it.

**Risk**: If we ever need zero-copy RX from VirtIO, we'll need to revisit. Accept this debt consciously.

### CAD-2: NetDevice Boundary

**Decision**: `NetDevice` is a trait with `tx()`, `poll_rx()`, `set_up()`, `set_down()`, `mtu()`, `mac()`, `stats()`, and `features()`. The driver owns its rings and DMA buffers. The stack owns routing and demux. Nothing crosses this boundary except `PacketBuf`.

**Rationale**: Clean separation is the only way multi-NIC works without a rewrite. VirtIO-net becomes one implementation. Loopback becomes another. Future e1000 or virtio-net-v2 slots in without touching the stack.

**Risk**: The trait object dispatch overhead is negligible at kernel networking speeds. Use `dyn NetDevice` behind a pointer in the device registry.

### CAD-3: Socket Layer and FD Semantics

**Decision**: `Socket` is a struct with a `dyn SocketOps` vtable, a state field, flags (including `O_NONBLOCK`), a receive queue, and a wait queue. File descriptors map to sockets through the existing FD table. Copy-in/copy-out for all userspace data. `errno` is set on every error path.

**Rationale**: The vtable lets UDP and TCP share the same `Socket` wrapper. The wait queue is the foundation for Phase 6 blocking I/O. Copy-in/copy-out is the only safe model for a kernel that doesn't have a unified virtual memory abstraction yet.

**Risk**: The `dyn SocketOps` dispatch adds one indirection per syscall. Acceptable.

### CAD-4: Timer Integration

**Decision**: All network timers (ARP aging, TCP retransmit, reassembly timeout) use a single timer wheel introduced in Phase 2. The wheel is driven by the existing timer interrupt. No per-subsystem ad-hoc timers.

**Rationale**: Three separate timer mechanisms would be impossible to reason about under load. A single wheel with per-entry callbacks is the standard approach.

**Risk**: The timer wheel must be implemented correctly in Phase 2 or every subsequent phase that depends on timeouts will be broken.

### CAD-5: Routing and Neighbor Cache Scoping

**Decision**: ARP caches and routing tables are per-interface from Phase 2 onward. No global singletons. `route_lookup(dst_ip)` returns `(netdev, next_hop_ip)`. The neighbor cache is keyed by `(netdev_id, ipv4_addr)`.

**Rationale**: Global singletons are the reason multi-NIC is hard to add later. Per-interface scoping from day one means Phase 9 is an extension, not a rewrite.

**Risk**: Slightly more complex lookup in Phase 2-3 when there's only one interface. Worth it.

---

## 1. Phase 1: Net Core Contracts (PacketBuf + NetDev)

> **Establishes the two foundational abstractions that every subsequent phase builds on.**
> **Kernel changes required**: Yes — new `drivers/src/net/packetbuf.rs`, new `drivers/src/net/netdev.rs`, refactor `drivers/src/virtio_net.rs`
> **Difficulty**: Medium-High
> **Depends on**: Nothing

### Background

Right now, the VirtIO driver is the network stack. It allocates raw byte buffers from its DMA ring, parses Ethernet headers inline, dispatches ARP and IPv4 by hand, and calls into TCP/UDP directly. There is no separation between "driver that moves bytes" and "stack that understands protocols."

This works for one NIC and one developer who knows where everything is. It breaks the moment you add a second NIC, a loopback device, or a developer who didn't write the original code. Every protocol layer re-parses headers from the beginning of the buffer because there's no shared understanding of where L2 ends and L3 begins.

Phase 1 introduces `PacketBuf` (a buffer type with headroom and layer offsets) and `NetDevice` (a trait that drivers implement). It then refactors VirtIO-net to implement `NetDevice` and wires up a single ingress path: `net_rx(dev, pkt)` that demuxes L2 to L3 to L4. Nothing else changes in Phase 1 — no new protocol support, no new syscalls. Just the contracts.

### 1A: PacketBuf Implementation

Define the core buffer type that all network layers will use to pass packets between each other.

- [ ] **1A.1** Create `drivers/src/net/packetbuf.rs` with the `PacketBuf` struct:
  - Fields: `data: Vec<u8>`, `head: usize` (start of valid data), `tail: usize` (end of valid data), `l2_offset: usize`, `l3_offset: usize`, `l4_offset: usize`
  - Reserve a `_frags_reserved: ()` field as a placeholder for future scatter-gather
  - Derive nothing — implement `Debug` manually to avoid leaking buffer contents in logs
- [ ] **1A.2** Implement `PacketBuf::with_headroom(headroom: usize, capacity: usize) -> Self`:
  - Allocates `capacity` bytes, sets `head = headroom`, `tail = headroom`
  - Headroom allows prepending headers without reallocating (standard skb pattern)
  - Document the headroom contract: callers must not write past `head` without calling `push_header`
- [ ] **1A.3** Implement header push/pull methods:
  - `push_header(&mut self, len: usize) -> &mut [u8]` — extends head backward into headroom
  - `pull_header(&mut self, len: usize) -> &[u8]` — advances head forward, returns consumed bytes
  - `payload(&self) -> &[u8]` — returns `data[head..tail]`
  - `payload_mut(&mut self) -> &mut [u8]` — mutable variant
- [ ] **1A.4** Implement layer offset helpers:
  - `set_l2(&mut self, offset: usize)`, `l2_header(&self) -> &[u8]`
  - `set_l3(&mut self, offset: usize)`, `l3_header(&self) -> &[u8]`
  - `set_l4(&mut self, offset: usize)`, `l4_header(&self) -> &[u8]`
  - Each returns a slice from the given offset to the next offset (or tail for L4)
- [ ] **1A.5** Implement `PacketBuf::from_raw(data: Vec<u8>) -> Self` for RX path:
  - Used by drivers when they receive a raw DMA buffer
  - Sets `head = 0`, `tail = data.len()`, all offsets to 0
  - This is the entry point for all received packets
- [ ] **1A.6** Add `PacketBuf` to `drivers/src/net/mod.rs` re-exports and update the module declaration

### 1B: NetDevice Trait

Define the trait that all network drivers implement, separating driver mechanics from protocol logic.

- [ ] **1B.1** Create `drivers/src/net/netdev.rs` with the `NetDevice` trait:
  - `fn tx(&mut self, pkt: PacketBuf) -> Result<(), NetError>` — transmit one packet
  - `fn poll_rx(&mut self, budget: usize) -> Vec<PacketBuf>` — drain up to `budget` received packets
  - `fn set_up(&mut self)` and `fn set_down(&mut self)` — link state control
  - `fn mtu(&self) -> usize` — maximum transmission unit
  - `fn mac(&self) -> [u8; 6]` — hardware MAC address
- [ ] **1B.2** Add `NetDeviceStats` struct and `fn stats(&self) -> NetDeviceStats` to the trait:
  - Fields: `rx_packets: u64`, `tx_packets: u64`, `rx_bytes: u64`, `tx_bytes: u64`, `rx_errors: u64`, `tx_errors: u64`
  - Stats are read-only from outside the driver; the driver updates them internally
- [ ] **1B.3** Add `NetDeviceFeatures` bitflags and `fn features(&self) -> NetDeviceFeatures`:
  - `CHECKSUM_TX` — driver can compute TX checksums in hardware
  - `CHECKSUM_RX` — driver has verified RX checksums
  - `TSO` — TCP segmentation offload (reserved, not implemented yet)
  - Stack uses these flags to decide whether to compute checksums in software
- [ ] **1B.4** Create `NetDeviceRegistry` in `drivers/src/net/netdev.rs`:
  - A global (spinlock-protected) list of `Box<dyn NetDevice>` with assigned interface indices
  - `register(dev: Box<dyn NetDevice>) -> usize` — returns assigned index
  - `get(index: usize) -> Option<&mut dyn NetDevice>` — lookup by index
  - `iter_mut()` — iterate all registered devices

### 1C: VirtIO-Net Refactor and Ingress Pipeline

Wire VirtIO-net to implement `NetDevice` and build the single ingress demux path.

- [ ] **1C.1** Refactor `drivers/src/virtio_net.rs` to implement `NetDevice`:
  - Move DMA ring management, feature negotiation, and MSI-X setup into the struct
  - Implement `tx()` by enqueuing a `PacketBuf` into the TX virtqueue
  - Implement `poll_rx()` by draining the RX virtqueue into `Vec<PacketBuf>`
  - Remove all protocol parsing from the driver — it becomes a pure byte mover
- [ ] **1C.2** Create `drivers/src/net/ingress.rs` with `net_rx(dev_index: usize, pkt: PacketBuf)`:
  - Parse Ethernet header, set `l2_offset`, extract ethertype
  - Dispatch: `ETHERTYPE_ARP` to `arp::handle_rx()`, `ETHERTYPE_IPV4` to `ipv4::handle_rx()`
  - Drop unknown ethertypes with a stat increment, no panic
  - This function is the single entry point for all received packets
- [ ] **1C.3** Create `drivers/src/net/ipv4.rs` with `ipv4::handle_rx(dev_index: usize, pkt: PacketBuf)`:
  - Validate IP version, header length, total length
  - Set `l3_offset`, extract protocol field
  - Dispatch: `IPPROTO_UDP` to `udp::handle_rx()`, `IPPROTO_TCP` to `tcp::handle_rx()`
  - Drop unknown protocols, increment stats
- [ ] **1C.4** Move existing ARP handling from `virtio_net.rs` into `drivers/src/net/arp.rs`:
  - `arp::handle_rx(dev_index: usize, pkt: PacketBuf)` — stub that logs and drops for now
  - `arp::send_request(dev_index: usize, target_ip: u32)` — stub
  - Phase 2 will fill these in; Phase 1 just establishes the module boundary
- [ ] **1C.5** Update the NAPI poll loop in `drivers/src/net/napi.rs`:
  - Call `dev.poll_rx(budget)` to get a `Vec<PacketBuf>`
  - Feed each packet to `net_rx(dev_index, pkt)`
  - Remove the old inline dispatch that called into TCP/UDP directly
- [ ] **1C.6** Update `drivers/src/net/mod.rs` to declare all new submodules:
  - `pub mod packetbuf`, `pub mod netdev`, `pub mod ingress`, `pub mod ipv4`, `pub mod arp`
  - Re-export key types: `PacketBuf`, `NetDevice`, `NetDeviceRegistry`

### Phase 1 Test Coverage

- [ ] **1.T1** Unit test `PacketBuf::with_headroom`: verify `payload()` is empty, headroom is accessible
- [ ] **1.T2** Unit test `push_header` / `pull_header`: push 14 bytes (Ethernet), verify offset and slice correctness
- [ ] **1.T3** Unit test `PacketBuf::from_raw`: verify `payload()` returns full buffer, offsets are zero
- [ ] **1.T4** Unit test `NetDeviceStats` accumulation: increment fields, verify reads
- [ ] **1.T5** Integration test: boot with VirtIO-net refactored, verify DHCP still completes (existing DHCP test)
- [ ] **1.T6** Integration test: send a UDP packet from userland, verify it reaches the ingress pipeline and is dispatched to UDP handler
- [ ] **1.T7** Verify `NetDeviceRegistry::register` assigns sequential indices starting at 0

### Phase 1 Gate

- [ ] **GATE**: `PacketBuf` compiles with no warnings under `#![no_std]`
- [ ] **GATE**: `NetDevice` trait is implemented by `VirtioNet` and passes all method calls
- [ ] **GATE**: `net_rx()` ingress path handles ARP and IPv4 dispatch without panicking on malformed input
- [ ] **GATE**: Existing DHCP flow still works end-to-end after VirtIO-net refactor
- [ ] **GATE**: No protocol parsing remains in `drivers/src/virtio_net.rs`

---

## 2. Phase 2: ARP Neighbor Cache v1

> **Replaces the static ARP table with a dynamic neighbor cache that handles request/reply and ages out stale entries.**
> **Kernel changes required**: Yes — new `drivers/src/net/neighbor.rs`, timer wheel, ARP state machine
> **Difficulty**: Medium
> **Depends on**: Phase 1

### Background

The current ARP table is a static array inside `virtio_net.rs`. It's populated at boot and never changes. This means the kernel can only talk to hosts whose MAC addresses were hardcoded at compile time. Any host that wasn't anticipated at boot time is unreachable, regardless of whether it's on the same subnet.

A real neighbor cache tracks the state of each entry: `Incomplete` (ARP request sent, waiting for reply), `Reachable` (reply received, entry is fresh), `Stale` (entry is old but not yet confirmed dead), and `Failed` (no reply after retries). Entries age out and are retried. Packets destined for an `Incomplete` entry are queued and flushed when the reply arrives.

Phase 2 builds this cache, wires it into the IPv4 egress path, and introduces the timer wheel that ARP aging (and later TCP retransmit and reassembly timeouts) will use.

### 2A: Timer Wheel

A minimal timer wheel to drive ARP aging, retransmit, and future timeout needs.

- [ ] **2A.1** Create `drivers/src/net/timer.rs` with `NetTimerWheel`:
  - Fixed-size wheel with 256 slots, each slot is a linked list of `TimerEntry`
  - `TimerEntry { callback: fn(), deadline_ticks: u64, next: Option<Box<TimerEntry>> }`
  - `insert(delay_ms: u64, callback: fn())` — inserts into the appropriate slot
  - `tick()` — called from the timer interrupt, fires all entries whose deadline has passed
- [ ] **2A.2** Wire `NetTimerWheel::tick()` into the existing timer interrupt handler:
  - The timer interrupt already fires at a known rate; call `tick()` from there
  - Protect the wheel with a spinlock; `tick()` acquires it, fires callbacks, releases
  - Document the constraint: callbacks must not re-acquire the timer lock (no recursive timers)
- [ ] **2A.3** Add `cancel(token: TimerToken) -> bool` to `NetTimerWheel`:
  - `insert()` returns a `TimerToken` (opaque u64)
  - `cancel()` marks the entry as cancelled; `tick()` skips cancelled entries
  - This is needed for TCP retransmit cancellation when an ACK arrives

### 2B: Neighbor Cache State Machine

The per-entry state machine and the cache data structure.

- [ ] **2B.1** Create `drivers/src/net/neighbor.rs` with `NeighborState` enum:
  - `Incomplete { retries: u8, pending: Vec<PacketBuf> }` — ARP sent, packets queued
  - `Reachable { mac: [u8; 6], expires_at: u64 }` — fresh entry
  - `Stale { mac: [u8; 6], last_used: u64 }` — old but usable, will re-probe on next use
  - `Failed` — no reply after max retries, drop queued packets
- [ ] **2B.2** Implement `NeighborCache` struct:
  - Keyed by `(dev_index: usize, ipv4: u32)` — per-interface from day one (CAD-5)
  - Fixed capacity of 256 entries with LRU eviction
  - `lookup(dev_index, ip) -> Option<[u8; 6]>` — returns MAC if `Reachable` or `Stale`
  - `insert_or_update(dev_index, ip, mac)` — called when an ARP reply arrives
- [ ] **2B.3** Implement `NeighborCache::resolve(dev_index, ip, pkt: PacketBuf)`:
  - If `Reachable` or `Stale`: set destination MAC in `pkt`, call `dev.tx(pkt)`
  - If `Incomplete`: push `pkt` onto the pending queue, do nothing else
  - If absent: create `Incomplete` entry, queue `pkt`, call `arp::send_request(dev_index, ip)`
  - If `Failed`: drop `pkt`, award an L (W/L currency)
- [ ] **2B.4** Implement aging via the timer wheel:
  - On `insert_or_update`, schedule a timer for `REACHABLE_TIME` (30s default)
  - Timer callback transitions `Reachable` to `Stale`
  - On `Stale` entry use, schedule a re-probe timer for `STALE_TIME` (10s default)
  - After `MAX_RETRIES` (3) without reply, transition to `Failed`

### 2C: ARP Protocol Handler

Fill in the ARP stubs from Phase 1 with real request/reply logic.

- [ ] **2C.1** Implement `arp::handle_rx(dev_index, pkt)` in `drivers/src/net/arp.rs`:
  - Parse ARP header: hardware type, protocol type, operation (request/reply)
  - On ARP reply: call `NeighborCache::insert_or_update(dev_index, sender_ip, sender_mac)`
  - On ARP request for our IP: send ARP reply with our MAC
  - Update cache on any observed ARP traffic (gratuitous ARP is a future concern)
- [ ] **2C.2** Implement `arp::send_request(dev_index, target_ip)`:
  - Build ARP request packet using `PacketBuf::with_headroom`
  - Fill Ethernet header: dst = broadcast (`ff:ff:ff:ff:ff:ff`), src = our MAC
  - Fill ARP header: opcode = REQUEST, sender = our IP/MAC, target = target IP, target MAC = zeros
  - Call `dev.tx(pkt)` via `NetDeviceRegistry`
- [ ] **2C.3** Wire `NeighborCache` into the IPv4 egress path in `drivers/src/net/ipv4.rs`:
  - `ipv4::send(dev_index, dst_ip, pkt)` calls `NeighborCache::resolve(dev_index, next_hop, pkt)`
  - Next-hop is `dst_ip` for now (routing table comes in Phase 3)
  - This replaces the hardcoded MAC lookup in the old VirtIO driver

### Phase 2 Test Coverage

- [ ] **2.T1** Unit test `NeighborCache::lookup` on empty cache returns `None`
- [ ] **2.T2** Unit test `insert_or_update` followed by `lookup` returns correct MAC
- [ ] **2.T3** Unit test `Incomplete` state: queued packets are flushed when reply arrives
- [ ] **2.T4** Unit test `Failed` state: packets are dropped, W/L loss is awarded
- [ ] **2.T5** Unit test timer wheel: insert a 100ms timer, advance ticks, verify callback fires
- [ ] **2.T6** Unit test timer cancellation: cancel before deadline, verify callback does not fire
- [ ] **2.T7** Integration test: boot, send UDP to a new host, verify ARP request appears on wire, reply resolves the entry
- [ ] **2.T8** Integration test: ARP entry ages to `Stale` after timeout, re-probe is sent on next use

### Phase 2 Gate

- [ ] **GATE**: ARP request/reply cycle completes for a new host without manual cache population
- [ ] **GATE**: Packets queued during `Incomplete` state are transmitted after reply arrives
- [ ] **GATE**: Timer wheel fires callbacks within one tick of their deadline
- [ ] **GATE**: No static ARP table remains in `drivers/src/virtio_net.rs`
- [ ] **GATE**: Neighbor cache is keyed by `(dev_index, ip)`, not a global map

---

## 3. Phase 3: L3 Config, Loopback, and Routing v1

> **Gives each interface an explicit IPv4 configuration and adds a routing table with longest-prefix match, plus a loopback device.**
> **Kernel changes required**: Yes — new `drivers/src/net/route.rs`, `drivers/src/net/loopback.rs`, `drivers/src/net/netstack.rs`
> **Difficulty**: Medium
> **Depends on**: Phase 2

### Background

Currently, the kernel's IP address is implicit — it comes from DHCP and is stored somewhere in the VirtIO driver without a clean interface. There's no routing table, so the kernel can only reach hosts on the directly connected subnet. There's no loopback device, so `127.0.0.1` doesn't work at all.

Phase 3 introduces an explicit `NetStack` struct that owns per-interface IPv4 configuration (address, netmask, gateway, DNS servers). It adds a routing table with longest-prefix match that can hold connected routes and a default route. It adds a loopback `NetDevice` implementation that delivers packets locally without touching the wire. Together, these three pieces make the kernel's network configuration explicit, inspectable, and extensible.

### 3A: Per-Interface IPv4 Configuration

- [ ] **3A.1** Create `drivers/src/net/netstack.rs` with `IfaceConfig` struct:
  - Fields: `dev_index: usize`, `ipv4_addr: u32`, `netmask: u32`, `gateway: u32`, `dns: [u32; 2]`, `up: bool`
  - `broadcast(&self) -> u32` — computed from addr and netmask
  - `is_local(&self, ip: u32) -> bool` — true if ip is on the directly connected subnet
- [ ] **3A.2** Create `NetStack` struct in `drivers/src/net/netstack.rs`:
  - Owns a `Vec<IfaceConfig>` (one per registered netdev)
  - `configure(dev_index, addr, netmask, gateway, dns)` — called by DHCP on lease
  - `iface_for_dev(dev_index) -> Option<&IfaceConfig>` — lookup by device index
  - `our_ip(dev_index) -> Option<u32>` — convenience accessor
- [ ] **3A.3** Update `drivers/src/net/dhcp.rs` to call `NetStack::configure()` on ACK:
  - Remove any ad-hoc IP storage from the DHCP module
  - Pass parsed subnet, router, and DNS fields into `NetStack`
  - Trigger route table update: add connected route and default route after configuration

### 3B: Routing Table v1

- [ ] **3B.1** Create `drivers/src/net/route.rs` with `RouteEntry` struct:
  - Fields: `prefix: u32`, `prefix_len: u8`, `gateway: u32` (0 = directly connected), `dev_index: usize`, `metric: u32`
  - `matches(&self, dst: u32) -> bool` — checks if dst falls within prefix/prefix_len
- [ ] **3B.2** Implement `RouteTable` with longest-prefix match:
  - `add(entry: RouteEntry)` — inserts, keeps table sorted by prefix_len descending
  - `remove(prefix: u32, prefix_len: u8)` — removes matching entry
  - `lookup(dst_ip: u32) -> Option<(dev_index: usize, next_hop: u32)>` — returns first match
  - `next_hop` is `gateway` if non-zero, else `dst_ip` (directly connected)
- [ ] **3B.3** Wire `RouteTable::lookup()` into `ipv4::send()`:
  - Replace the hardcoded `dev_index = 0` with a route lookup
  - If no route found: drop packet, log warning, award an L
  - Pass `(dev_index, next_hop)` to `NeighborCache::resolve()`

### 3C: Loopback Device

- [ ] **3C.1** Create `drivers/src/net/loopback.rs` with `LoopbackDev` struct implementing `NetDevice`:
  - `tx()` pushes the packet onto an internal `VecDeque<PacketBuf>`
  - `poll_rx()` drains up to `budget` packets from the queue
  - `mtu()` returns 65535, `mac()` returns `[0u8; 6]`
  - `features()` returns `CHECKSUM_TX | CHECKSUM_RX` (loopback never needs checksum computation)
- [ ] **3C.2** Register `LoopbackDev` at kernel init before VirtIO-net:
  - It gets `dev_index = 0` by convention
  - Configure it with `127.0.0.1/8`, no gateway, no DNS
  - Add connected route `127.0.0.0/8 -> dev_index=0` to the route table
- [ ] **3C.3** Verify loopback delivery in `net_rx()`:
  - Packets received on the loopback device go through the same `net_rx()` ingress path
  - No special-casing needed if the ingress path is correct
  - Add a stat counter for loopback RX packets in `NetDeviceStats`

### Phase 3 Test Coverage

- [ ] **3.T1** Unit test `RouteTable::lookup` with connected route: dst on subnet returns correct dev_index
- [ ] **3.T2** Unit test `RouteTable::lookup` with default route: dst off subnet returns gateway
- [ ] **3.T3** Unit test `RouteTable::lookup` with no routes: returns `None`
- [ ] **3.T4** Unit test longest-prefix match: /24 beats /16 for a matching address
- [ ] **3.T5** Integration test: send UDP to `127.0.0.1`, verify packet is delivered locally without hitting VirtIO
- [ ] **3.T6** Integration test: DHCP lease populates `NetStack` and route table correctly
- [ ] **3.T7** Integration test: `ifconfig` shell app reads `IfaceConfig` and displays correct IP/netmask/gateway

### Phase 3 Gate

- [ ] **GATE**: `RouteTable::lookup()` returns correct `(dev_index, next_hop)` for all route types
- [ ] **GATE**: Loopback device delivers packets to `127.0.0.1` without touching VirtIO-net
- [ ] **GATE**: DHCP lease populates `NetStack` and triggers route table update
- [ ] **GATE**: `ipv4::send()` uses route lookup, not hardcoded device index
- [ ] **GATE**: No IP address storage remains outside `NetStack`

---

## 4. Phase 4: Userspace UDP Sockets v1

> **Replaces the ad-hoc UDP socket layer with a clean BSD-ish socket API: socket(), bind(), sendto(), recvfrom().**
> **Kernel changes required**: Yes — rewrite `drivers/src/net/socket.rs`, update `core/src/syscall/net_handlers.rs`
> **Difficulty**: Medium
> **Depends on**: Phase 3

### Background

The existing `socket.rs` has `KernelSocket`, `UdpReceiveQueue`, and `SocketTable`, but the demux logic is fragile and the socket abstraction doesn't cleanly separate UDP from TCP. The `SocketTable` is a flat array with `MAX_SOCKETS=64`. There's no ephemeral port allocator, so port assignment is ad-hoc. The `sockaddr_in` ABI is defined in `abi/src/net.rs` but isn't consistently used across all paths.

Phase 4 rebuilds the socket layer around the `Socket { ops: dyn SocketOps, ... }` model from CAD-3. UDP gets a clean implementation: `socket()` creates a socket, `bind()` assigns a local address/port, `connect()` sets a default remote address, `sendto()` transmits a datagram, `recvfrom()` receives one. Ephemeral ports are allocated from a proper range. The receive queue is per-socket and bounded.

### 4A: Socket Abstraction Layer

- [ ] **4A.1** Rewrite `drivers/src/net/socket.rs` with the new `Socket` struct:
  - Fields: `ops: Box<dyn SocketOps>`, `state: SocketState`, `flags: u32` (includes `O_NONBLOCK` placeholder), `local_addr: SockAddrIn`, `remote_addr: Option<SockAddrIn>`, `recvq: BoundedQueue<PacketBuf>`, `waitq: WaitQueue`
  - `SocketState` enum: `Unbound`, `Bound`, `Connected`, `Listening`, `Closed`
  - `WaitQueue` is a stub for now (Phase 6 fills it in); just a `bool` flag
- [ ] **4A.2** Define `SocketOps` trait in `drivers/src/net/socket.rs`:
  - `fn send(&mut self, sock: &Socket, pkt: PacketBuf) -> Result<usize, NetError>`
  - `fn recv(&mut self, sock: &mut Socket) -> Option<(PacketBuf, SockAddrIn)>`
  - `fn bind(&mut self, sock: &mut Socket, addr: SockAddrIn) -> Result<(), NetError>`
  - `fn connect(&mut self, sock: &mut Socket, addr: SockAddrIn) -> Result<(), NetError>`
  - `fn close(&mut self, sock: &mut Socket)`
- [ ] **4A.3** Implement `SocketTable` as a fixed-size array of `Option<Socket>` with `MAX_SOCKETS=64`:
  - `alloc() -> Option<usize>` — finds first free slot, returns socket index
  - `get(idx) -> Option<&Socket>` and `get_mut(idx) -> Option<&mut Socket>`
  - `free(idx)` — calls `ops.close()`, drops the socket
  - Protect with a spinlock; document that the lock must not be held across blocking operations
- [ ] **4A.4** Implement ephemeral port allocator in `drivers/src/net/socket.rs`:
  - Range: 49152..65535 (IANA dynamic/private ports)
  - `alloc_ephemeral() -> Option<u16>` — round-robin scan, skip ports already in use
  - `release_port(port: u16)` — marks port as available
  - Track allocated ports in a 2048-bit bitmap (256 bytes)

### 4B: UDP Socket Implementation

- [ ] **4B.1** Create `drivers/src/net/udp.rs` with `UdpSocket` struct implementing `SocketOps`:
  - `bind()`: validates address, registers `(local_ip, local_port)` in the UDP demux table
  - `connect()`: sets `remote_addr`, validates it's not multicast (for now)
  - `send()`: builds UDP header, calls `ipv4::send()` with the packet
  - `recv()`: pops from `sock.recvq`, returns `None` if empty (blocking is Phase 6)
- [ ] **4B.2** Implement UDP demux table in `drivers/src/net/udp.rs`:
  - Maps `(local_ip: u32, local_port: u16)` to socket index
  - `register(local_ip, local_port, sock_idx)` — called by `bind()`
  - `lookup(dst_ip, dst_port) -> Option<usize>` — called by `udp::handle_rx()`
  - Wildcard: `local_ip = 0` matches any destination IP (INADDR_ANY)
- [ ] **4B.3** Implement `udp::handle_rx(dev_index, pkt)` in `drivers/src/net/udp.rs`:
  - Parse UDP header: src port, dst port, length, checksum
  - Validate checksum if `!dev.features().contains(CHECKSUM_RX)`
  - Look up socket via demux table
  - Push `pkt` onto `sock.recvq`; drop if queue is full (award an L)
- [ ] **4B.4** Implement `udp::build_packet(src: SockAddrIn, dst: SockAddrIn, payload: &[u8]) -> PacketBuf`:
  - Allocates `PacketBuf` with headroom for Ethernet + IP + UDP headers
  - Fills UDP header, computes checksum if hardware offload not available
  - Returns packet ready for `ipv4::send()`

### 4C: Syscall Integration

- [ ] **4C.1** Update `core/src/syscall/net_handlers.rs` `handle_socket()`:
  - `socket(AF_INET, SOCK_DGRAM, 0)` allocates a `Socket` with `UdpSocket` ops
  - Returns socket index as file descriptor
  - Return `EAFNOSUPPORT` for non-`AF_INET`, `EPROTONOSUPPORT` for unknown type
- [ ] **4C.2** Update `handle_bind()`, `handle_connect()`, `handle_sendto()`, `handle_recvfrom()`:
  - Copy `SockAddrIn` from userspace with bounds checking
  - Delegate to `SocketOps` methods
  - Set errno and return -1 on error; return bytes transferred on success
- [ ] **4C.3** Update `abi/src/net.rs` to stabilize the `SockAddrIn` layout:
  - `sin_family: u16`, `sin_port: u16` (network byte order), `sin_addr: u32` (network byte order)
  - Add `htons()` and `ntohs()` helpers to `abi/src/net.rs`
  - Document that all port numbers in the ABI are in network byte order
- [ ] **4C.4** Update `userland/src/syscall/net.rs` wrappers to use the stabilized ABI:
  - Ensure `sendto` and `recvfrom` wrappers handle the `SockAddrIn` copy correctly
  - Add `udp_echo_test()` helper to userland for integration testing

### Phase 4 Test Coverage

- [ ] **4.T1** Unit test ephemeral port allocator: allocate 100 ports, verify no duplicates
- [ ] **4.T2** Unit test UDP demux: register two sockets on different ports, verify correct dispatch
- [ ] **4.T3** Unit test INADDR_ANY: socket bound to 0.0.0.0 receives packets on any local IP
- [ ] **4.T4** Unit test receive queue overflow: push beyond capacity, verify oldest packet is dropped
- [ ] **4.T5** Integration test: userland `socket(AF_INET, SOCK_DGRAM)` + `bind` + `recvfrom` receives a UDP packet from QEMU's SLIRP
- [ ] **4.T6** Integration test: `sendto` sends a UDP packet, verify it appears on the wire
- [ ] **4.T7** Integration test: two sockets on different ports, verify no cross-delivery

### Phase 4 Gate

- [ ] **GATE**: `socket(AF_INET, SOCK_DGRAM)` + `bind` + `sendto` + `recvfrom` round-trip works end-to-end
- [ ] **GATE**: Ephemeral port allocator never returns a duplicate within a single boot
- [ ] **GATE**: UDP demux correctly routes packets to the right socket
- [ ] **GATE**: Receive queue drops packets gracefully when full (no panic, no corruption)
- [ ] **GATE**: `SockAddrIn` ABI is stable and documented in `abi/src/net.rs`

---

## 5. Phase 5: Userspace TCP Stream Sockets v1

> **Connects the existing TCP state machine to the socket layer, enabling socket(), connect(), listen(), accept(), send(), recv().**
> **Kernel changes required**: Yes — new `drivers/src/net/tcp_socket.rs`, update `drivers/src/net/tcp.rs`, update syscall handlers
> **Difficulty**: High
> **Depends on**: Phase 4

### Background

The TCP state machine in `drivers/src/net/tcp.rs` is a solid RFC 793 implementation with all eleven states, 3-way handshake, retransmit, and delayed ACKs. It has `MAX_CONNECTIONS=64`. What it doesn't have is a clean mapping to the socket layer. There's no `accept()` queue, no way for userspace to block waiting for a connection, and no clean `send()`/`recv()` path that goes through the socket abstraction.

Phase 5 builds the bridge. `TcpSocket` implements `SocketOps` and owns a reference to a `TcpConnection` in the existing state machine. `listen()` creates a listening socket with a backlog queue. `accept()` dequeues a completed connection. `send()` and `recv()` move data through the TCP send/receive buffers. FIN and half-close are handled. Blocking wakeups are stubbed (Phase 6 completes them).

### 5A: TCP PCB to Socket Mapping

- [ ] **5A.1** Create `drivers/src/net/tcp_socket.rs` with `TcpSocket` struct implementing `SocketOps`:
  - Fields: `conn_id: Option<usize>` (index into `TcpConnection` table), `backlog: Option<AcceptQueue>`
  - `AcceptQueue`: a bounded `VecDeque<usize>` of completed connection IDs, capacity = listen backlog
  - `send()`: copies data into `TcpConnection`'s send buffer, triggers `tcp::try_send()`
  - `recv()`: copies data out of `TcpConnection`'s receive buffer
- [ ] **5A.2** Add `socket_idx: Option<usize>` field to `TcpConnection` in `drivers/src/net/tcp.rs`:
  - When a connection transitions to `Established`, set `socket_idx` to the owning socket
  - When a connection is closed, clear `socket_idx` and wake the socket's wait queue
  - This is the bidirectional link between the state machine and the socket layer
- [ ] **5A.3** Implement `TcpDemuxTable` in `drivers/src/net/tcp_socket.rs`:
  - Maps `(local_ip, local_port, remote_ip, remote_port)` to connection ID
  - Also maps `(local_ip, local_port)` to listening socket index (for SYN dispatch)
  - `lookup_established()` and `lookup_listener()` are separate methods
- [ ] **5A.4** Update `tcp::handle_rx()` in `drivers/src/net/tcp.rs` to use `TcpDemuxTable`:
  - On SYN: look up listener, create new `TcpConnection` in `SynReceived` state
  - On established segment: look up connection, feed to state machine
  - On unknown: send RST (or drop, depending on state)

### 5B: Listen, Accept, Connect

- [ ] **5B.1** Implement `TcpSocket::bind()` and `TcpSocket::listen()`:
  - `bind()`: registers `(local_ip, local_port)` in `TcpDemuxTable` as a listener
  - `listen(backlog)`: sets `self.backlog = Some(AcceptQueue::new(backlog))`
  - Transition socket state to `Listening`
- [ ] **5B.2** Implement `TcpSocket::accept()`:
  - Dequeues a connection ID from `self.backlog`
  - Creates a new `Socket` with a `TcpSocket` ops pointing to the dequeued connection
  - Allocates a new socket index in `SocketTable`, returns it
  - Returns `EAGAIN` if backlog is empty (blocking is Phase 6)
- [ ] **5B.3** Implement `TcpSocket::connect()`:
  - Allocates an ephemeral local port
  - Creates a `TcpConnection` in `SynSent` state, sends SYN
  - Registers in `TcpDemuxTable`
  - Returns `EINPROGRESS` (blocking connect is Phase 6)
- [ ] **5B.4** Wire completed connections into the accept queue:
  - When `TcpConnection` transitions to `Established` via SYN-ACK, push its ID onto the listener's `AcceptQueue`
  - If the queue is full, send RST and drop the connection (backlog overflow)
  - Wake the listener socket's wait queue (stub for Phase 6)

### 5C: Send, Recv, Close

- [ ] **5C.1** Implement `TcpSocket::send()` with send buffer:
  - Copy data from userspace into `TcpConnection`'s send buffer
  - Call `tcp::try_send()` to push data into the wire if the window allows
  - Return bytes accepted (may be less than requested if buffer is full)
- [ ] **5C.2** Implement `TcpSocket::recv()` with receive buffer:
  - Copy data from `TcpConnection`'s receive buffer into userspace
  - Advance the receive window, send ACK if window update is significant
  - Return 0 on FIN (EOF), `EAGAIN` if buffer is empty and `O_NONBLOCK` is set
- [ ] **5C.3** Implement FIN and half-close:
  - `TcpSocket::close()` sends FIN, transitions to `FinWait1`
  - Receiving FIN transitions to `CloseWait`, notifies the socket layer
  - `recv()` returns 0 after the FIN has been delivered to userspace
- [ ] **5C.4** Update `core/src/syscall/net_handlers.rs` for TCP syscalls:
  - `handle_socket(AF_INET, SOCK_STREAM)` creates a `Socket` with `TcpSocket` ops
  - `handle_listen()`, `handle_accept()`, `handle_connect()` delegate to `TcpSocket`
  - `handle_send()` and `handle_recv()` use the stream path (no address argument)

### 5D: Timer Integration for TCP

- [ ] **5D.1** Wire TCP retransmit timer into `NetTimerWheel` from Phase 2:
  - On segment TX, schedule a retransmit timer using `NetTimerWheel::insert()`
  - On ACK receipt, cancel the timer using the returned `TimerToken`
  - On timer fire, retransmit the unacknowledged segment, double the RTO (exponential backoff)
- [ ] **5D.2** Wire TCP TIME_WAIT timer into `NetTimerWheel`:
  - On entering `TimeWait`, schedule a 2*MSL (4 second) timer
  - On timer fire, transition to `Closed`, release the connection slot
  - This prevents port reuse before the connection is fully dead

### Phase 5 Test Coverage

- [ ] **5.T1** Unit test `TcpDemuxTable`: register listener, send SYN, verify new connection created
- [ ] **5.T2** Unit test accept queue overflow: fill backlog, verify RST is sent on overflow
- [ ] **5.T3** Unit test FIN handling: send FIN, verify `recv()` returns 0 after data is drained
- [ ] **5.T4** Unit test retransmit timer: send segment, don't ACK, verify retransmit fires
- [ ] **5.T5** Unit test TIME_WAIT: verify connection slot is released after 2*MSL
- [ ] **5.T6** Integration test: userland TCP client connects to QEMU SLIRP, sends "hello", receives echo
- [ ] **5.T7** Integration test: TCP server `listen()` + `accept()` + `recv()` + `send()` round-trip
- [ ] **5.T8** Integration test: connection teardown via FIN, verify clean close on both sides

### Phase 5 Gate

- [ ] **GATE**: TCP 3-way handshake completes via `socket()` + `connect()` or `listen()` + `accept()`
- [ ] **GATE**: `send()` and `recv()` move data correctly through the TCP state machine
- [ ] **GATE**: FIN/half-close delivers EOF to userspace (`recv()` returns 0)
- [ ] **GATE**: Retransmit timer fires and retransmits unacknowledged segments
- [ ] **GATE**: TIME_WAIT releases connection slots after 2*MSL

---

## 6. Phase 6: Blocking/Nonblocking and poll/select

> **Adds O_NONBLOCK, socket timeouts, and a poll() syscall so userspace can wait on multiple sockets without spinning.**
> **Kernel changes required**: Yes — new `core/src/wait.rs`, new `SYSCALL_POLL`, update socket layer
> **Difficulty**: Medium-High
> **Depends on**: Phase 5

### Background

Every socket operation in Phases 4 and 5 either returns immediately or returns `EAGAIN`. There's no way for userspace to block waiting for data to arrive. The only option is a busy-wait loop, which wastes CPU and makes the scheduler unhappy.

Phase 6 adds three things: `O_NONBLOCK` flag support with consistent `EAGAIN` semantics, socket-level timeouts (`SO_RCVTIMEO`/`SO_SNDTIMEO`), and a `poll()` syscall that lets userspace wait on multiple file descriptors simultaneously. The wait queue stub from Phase 4/5 gets a real implementation. The scheduler gains the ability to block a task on a wait queue and wake it when readiness changes.

### 6A: Wait Queue Implementation

- [ ] **6A.1** Create `core/src/wait.rs` with `WaitQueue` struct:
  - A list of `(task_id, readiness_mask)` pairs
  - `wait(&mut self, task_id: usize, mask: ReadinessMask)` — adds task to the queue, yields CPU
  - `wake_all(&mut self, mask: ReadinessMask)` — wakes all tasks waiting on matching bits
  - `wake_one(&mut self, mask: ReadinessMask)` — wakes the first matching task
- [ ] **6A.2** Define `ReadinessMask` bitflags:
  - `READABLE` — data available to read, or connection ready to accept
  - `WRITABLE` — send buffer has space, or connection established
  - `ERROR` — error condition (RST received, connection refused, etc.)
  - `HUP` — connection closed by remote (FIN received)
- [ ] **6A.3** Wire `WaitQueue::wake_all()` into the socket layer:
  - UDP `handle_rx()`: wake with `READABLE` after pushing to recvq
  - TCP `Established` transition: wake connecting socket with `WRITABLE`
  - TCP data RX: wake with `READABLE`
  - TCP FIN RX: wake with `READABLE | HUP`
  - TCP RST RX: wake with `ERROR`

### 6B: O_NONBLOCK and Socket Timeouts

- [ ] **6B.1** Implement `O_NONBLOCK` flag in `Socket`:
  - `set_nonblocking(sock_idx, nonblocking: bool)` — sets/clears the flag
  - All blocking operations check the flag: if set, return `EAGAIN` instead of blocking
  - Add `SYSCALL_FCNTL` or a `setsockopt`-style call to set the flag from userspace
- [ ] **6B.2** Implement `SO_RCVTIMEO` and `SO_SNDTIMEO` via `setsockopt`:
  - Add `recv_timeout: Option<u64>` and `send_timeout: Option<u64>` to `Socket`
  - Blocking `recv()` uses `NetTimerWheel` to schedule a wakeup after the timeout
  - On timeout, return `ETIMEDOUT`
- [ ] **6B.3** Update `handle_recv()` and `handle_send()` syscall handlers:
  - If socket is blocking and queue is empty: call `WaitQueue::wait()`, yield
  - On wakeup: retry the operation
  - If `O_NONBLOCK`: return `EAGAIN` immediately without yielding

### 6C: poll() Syscall

- [ ] **6C.1** Define `PollFd` struct in `abi/src/net.rs`:
  - `fd: i32`, `events: u16` (requested events), `revents: u16` (returned events)
  - Event flags: `POLLIN=0x0001`, `POLLOUT=0x0004`, `POLLERR=0x0008`, `POLLHUP=0x0010`
- [ ] **6C.2** Add `SYSCALL_POLL` (syscall number TBD) to `abi/src/syscall.rs`:
  - Signature: `poll(fds: *mut PollFd, nfds: usize, timeout_ms: i32) -> i32`
  - Returns number of fds with non-zero `revents`, 0 on timeout, -1 on error
- [ ] **6C.3** Implement `handle_poll()` in `core/src/syscall/net_handlers.rs`:
  - Copy `PollFd` array from userspace
  - Check current readiness for each fd (non-blocking check)
  - If any fd is ready: fill `revents`, copy back, return count
  - If none ready and `timeout_ms != 0`: register task on all relevant wait queues, yield
  - On wakeup: re-check readiness, fill `revents`, return
- [ ] **6C.4** Add `poll()` wrapper to `userland/src/syscall/net.rs`:
  - Safe wrapper that takes a `&mut [PollFd]` slice
  - Handles the copy-in/copy-out automatically
  - Add a `poll_readable(fd, timeout_ms)` convenience function

### Phase 6 Test Coverage

- [ ] **6.T1** Unit test `WaitQueue::wake_all`: register two tasks, wake both, verify both are scheduled
- [ ] **6.T2** Unit test `O_NONBLOCK`: `recv()` on empty socket returns `EAGAIN` immediately
- [ ] **6.T3** Unit test `SO_RCVTIMEO`: blocking `recv()` returns `ETIMEDOUT` after timeout
- [ ] **6.T4** Integration test: `poll()` on a UDP socket blocks until a packet arrives, then returns 1
- [ ] **6.T5** Integration test: `poll()` on two sockets, data arrives on one, verify only that fd has `POLLIN` set
- [ ] **6.T6** Integration test: `poll()` with `timeout_ms=0` returns immediately (non-blocking check)
- [ ] **6.T7** Integration test: TCP `connect()` with `O_NONBLOCK` returns `EINPROGRESS`, `poll()` with `POLLOUT` wakes when connected

### Phase 6 Gate

- [ ] **GATE**: `O_NONBLOCK` sockets return `EAGAIN` consistently on all blocking operations
- [ ] **GATE**: Blocking `recv()` wakes correctly when data arrives (no busy-wait)
- [ ] **GATE**: `poll()` syscall returns correct `revents` for at least `POLLIN` and `POLLOUT`
- [ ] **GATE**: `SO_RCVTIMEO` returns `ETIMEDOUT` after the specified interval
- [ ] **GATE**: No busy-wait loops remain in any socket operation

---

## 7. Phase 7: Userspace DNS Resolver

> **Moves DNS resolution out of the kernel and into a userspace library that uses UDP sockets.**
> **Kernel changes required**: Minimal — kernel keeps DNS server list, compatibility syscall preserved
> **Difficulty**: Medium
> **Depends on**: Phase 6

### Background

The current DNS resolver in `drivers/src/net/dns.rs` lives in the kernel. It has a 16-entry LRU cache and can parse A and CNAME records. This works, but it's wrong architecturally. DNS is an application-layer protocol. It uses UDP sockets. It should run in userspace, where it can be updated, debugged, and replaced without touching the kernel.

Phase 7 moves DNS resolution to a userspace library. The library opens a UDP socket, sends queries to the DNS server (obtained from DHCP via a kernel syscall), and parses responses. It maintains its own cache. The existing `SYSCALL_RESOLVE` (135) is preserved as a compatibility shim that calls into the userspace library, but new code should use the library directly.

### 7A: Kernel DNS Server List

- [ ] **7A.1** Add `dns_servers: [u32; 2]` to `NetStack::IfaceConfig` (already present from Phase 3):
  - Verify DHCP populates this correctly after Phase 3
  - Add `SYSCALL_NET_DNS_SERVERS` (new syscall) that copies the list to userspace
  - Signature: `net_dns_servers(buf: *mut u32, count: usize) -> i32`
- [ ] **7A.2** Add `net_dns_servers()` wrapper to `userland/src/syscall/net.rs`:
  - Returns a `[u32; 2]` array of DNS server IPs
  - Returns `[0u32; 2]` if no DNS servers are configured (DHCP not yet complete)

### 7B: Userspace Resolver Library

- [ ] **7B.1** Create `userland/src/net/dns.rs` with `DnsResolver` struct:
  - Fields: `socket: i32` (UDP socket fd), `servers: [u32; 2]`, `cache: DnsCache`
  - `DnsCache`: 32-entry LRU, keyed by hostname, stores `(ip: u32, ttl_expires: u64)`
  - `new() -> Self` — opens UDP socket, calls `net_dns_servers()` to populate servers
- [ ] **7B.2** Implement `DnsResolver::resolve(hostname: &str) -> Result<u32, DnsError>`:
  - Check cache first; return cached IP if TTL has not expired
  - Build DNS query packet: standard A record query, random transaction ID
  - `sendto()` to the first DNS server, `poll()` with 3s timeout
  - Parse response: extract A record IP, update cache with TTL
  - On timeout or error, try second DNS server
  - Return `DnsError::NotFound` if both servers fail
- [ ] **7B.3** Implement DNS packet builder in `userland/src/net/dns.rs`:
  - `build_query(txid: u16, hostname: &str) -> Vec<u8>` — builds a minimal DNS query
  - `parse_response(buf: &[u8]) -> Result<(u32, u32), DnsError>` — returns `(ip, ttl)`
  - Handle CNAME chains (follow up to 5 hops)
- [ ] **7B.4** Create `/etc/hosts`-equivalent in `userland/src/net/hosts.rs`:
  - `HostsTable`: a static list of `(hostname, ip)` pairs compiled into the binary
  - `lookup(hostname) -> Option<u32>` — checked before DNS query
  - Pre-populate with `localhost -> 127.0.0.1`

### 7C: Compatibility and Shell Integration

- [ ] **7C.1** Update `SYSCALL_RESOLVE` handler in `core/src/syscall/net_handlers.rs`:
  - Keep the syscall number (135) for compatibility
  - The kernel-side implementation can remain as a fallback for early boot
  - Document that new code should use the userspace library
- [ ] **7C.2** Update the `resolve` shell builtin to use `DnsResolver`:
  - Replace the `SYSCALL_RESOLVE` call with `DnsResolver::new().resolve(hostname)`
  - Display TTL alongside the resolved IP
- [ ] **7C.3** Update `userland/src/syscall/net.rs` `net_resolve()` wrapper:
  - Add a note that this calls the kernel fallback; prefer `DnsResolver` for new code
  - Keep the wrapper for backward compatibility with existing shell apps

### Phase 7 Test Coverage

- [ ] **7.T1** Unit test `DnsCache`: insert entry, lookup before TTL expiry returns hit, after expiry returns miss
- [ ] **7.T2** Unit test `build_query`: verify wire format matches RFC 1035 for a simple hostname
- [ ] **7.T3** Unit test `parse_response`: parse a captured A record response, verify IP and TTL extraction
- [ ] **7.T4** Unit test CNAME chain: parse a response with one CNAME hop, verify final A record is returned
- [ ] **7.T5** Integration test: `DnsResolver::resolve("example.com")` returns a valid IP via QEMU SLIRP DNS
- [ ] **7.T6** Integration test: `resolve` shell command uses userspace resolver, displays TTL
- [ ] **7.T7** Integration test: `HostsTable` lookup for `localhost` returns `127.0.0.1` without a DNS query

### Phase 7 Gate

- [ ] **GATE**: `DnsResolver::resolve()` successfully resolves a hostname via UDP DNS query
- [ ] **GATE**: DNS cache prevents duplicate queries for the same hostname within TTL
- [ ] **GATE**: `SYSCALL_RESOLVE` (135) still works for backward compatibility
- [ ] **GATE**: `localhost` resolves to `127.0.0.1` via `HostsTable` without a network query
- [ ] **GATE**: No DNS parsing code remains in the kernel hot path

---

## 8. Phase 8: IPv4 Robustness and TCP Hardening

> **Adds IPv4 fragmentation/reassembly, ICMP error handling, and TCP timer hardening for real-world network conditions.**
> **Kernel changes required**: Yes — new `drivers/src/net/fragment.rs`, new `drivers/src/net/icmp.rs`, updates to `drivers/src/net/tcp.rs`
> **Difficulty**: High
> **Depends on**: Phase 5

### Background

The current IPv4 implementation assumes every packet fits in a single MTU. In practice, packets larger than the path MTU get fragmented by intermediate routers. Without reassembly, the kernel silently drops these fragments. Without ICMP "fragmentation needed" handling, the kernel doesn't know to reduce its packet size.

TCP retransmit under QEMU's SLIRP network is also fragile. SLIRP introduces jitter and occasional drops that expose weaknesses in the retransmit timer: fixed RTO, no RTT estimation, no congestion window. Phase 8 hardens both layers.

### 8A: IPv4 Fragmentation TX

- [ ] **8A.1** Implement `ipv4::fragment_and_send(dev_index, dst_ip, pkt, mtu)` in `drivers/src/net/ipv4.rs`:
  - If `pkt.payload().len() <= mtu - IP_HEADER_LEN`: send directly (no fragmentation needed)
  - Otherwise: split payload into fragments of `mtu - IP_HEADER_LEN` bytes each
  - Set `MF` (more fragments) flag on all but the last fragment
  - Set fragment offset field correctly (in 8-byte units)
- [ ] **8A.2** Update `ipv4::send()` to call `fragment_and_send()`:
  - Look up MTU from `NetDeviceRegistry::get(dev_index).mtu()`
  - Pass MTU to `fragment_and_send()`
  - Document that fragmentation is a last resort; callers should use path MTU discovery eventually
- [ ] **8A.3** Add fragmentation stats to `NetDeviceStats`:
  - `tx_fragments: u64` — number of fragments transmitted
  - `tx_fragmented_packets: u64` — number of original packets that required fragmentation

### 8B: IPv4 Reassembly RX

- [ ] **8B.1** Create `drivers/src/net/fragment.rs` with `ReassemblyBuffer` struct:
  - Keyed by `(src_ip, dst_ip, protocol, identification)` — the 4-tuple that identifies a fragment group
  - Stores received fragments as a sorted list of `(offset, data)` pairs
  - `insert(frag: Fragment) -> Option<PacketBuf>` — returns reassembled packet when complete
- [ ] **8B.2** Implement reassembly resource limits in `ReassemblyBuffer`:
  - Maximum 8 concurrent reassembly groups (drop oldest on overflow)
  - Maximum 64KB per reassembly group (drop fragments that would exceed this)
  - Timeout: 30 seconds per group (use `NetTimerWheel`); on timeout, drop group and award an L
- [ ] **8B.3** Wire `ReassemblyBuffer` into `ipv4::handle_rx()`:
  - Check `MF` flag and fragment offset: if either is non-zero, this is a fragment
  - Pass to `ReassemblyBuffer::insert()`; if it returns `Some(pkt)`, continue with reassembled packet
  - If it returns `None`, the group is incomplete; return without further processing

### 8C: ICMP Error Handling

- [ ] **8C.1** Create `drivers/src/net/icmp.rs` with `icmp::handle_rx(dev_index, pkt)`:
  - Parse ICMP header: type, code, checksum
  - Handle type 3 (destination unreachable): extract embedded IP header, notify socket layer
  - Handle type 11 (time exceeded): log, notify socket layer
  - Drop all other ICMP types for now (ping is a future feature)
- [ ] **8C.2** Implement ICMP-to-socket feedback in `icmp.rs`:
  - Extract the embedded IP+transport header from the ICMP error payload
  - Look up the affected socket via `TcpDemuxTable` or `UdpDemuxTable`
  - Set `ERROR` readiness on the socket's wait queue
  - Store the ICMP error code in the socket for `getsockopt(SO_ERROR)` retrieval
- [ ] **8C.3** Implement `icmp::send_unreachable(dev_index, original_pkt, code)`:
  - Build ICMP type 3 packet with the appropriate code
  - Include the original IP header + first 8 bytes of transport header
  - Send via `ipv4::send()`
  - Call this from `udp::handle_rx()` when no socket is found for a UDP packet

### 8D: TCP Timer Hardening

- [ ] **8D.1** Implement RTT estimation in `drivers/src/net/tcp.rs`:
  - Track `srtt` (smoothed RTT) and `rttvar` (RTT variance) per connection
  - Update on each ACK using RFC 6298 formula: `srtt = 7/8 * srtt + 1/8 * rtt`
  - Compute RTO: `max(1s, srtt + 4 * rttvar)`
- [ ] **8D.2** Implement exponential backoff for retransmit:
  - On retransmit, double the RTO (up to a maximum of 60 seconds)
  - Reset RTO to computed value after a successful ACK
  - After 5 retransmits without ACK, close the connection with `ETIMEDOUT`
- [ ] **8D.3** Implement Nagle's algorithm in `tcp::try_send()`:
  - If there is unacknowledged data and the pending data is less than MSS: hold it
  - Flush immediately if `TCP_NODELAY` is set (add the flag to `TcpConnection`)
  - This reduces small-packet overhead under SLIRP's high-latency simulation
- [ ] **8D.4** Add TCP keepalive timer support:
  - `SO_KEEPALIVE` socket option enables keepalive probes
  - After 2 hours of inactivity, send a keepalive probe every 75 seconds
  - After 9 failed probes, close the connection
  - Use `NetTimerWheel` for all keepalive timers

### Phase 8 Test Coverage

- [ ] **8.T1** Unit test fragmentation TX: 3000-byte payload with 1500-byte MTU produces 2 fragments with correct offsets
- [ ] **8.T2** Unit test reassembly: insert 2 fragments out of order, verify correct reassembled payload
- [ ] **8.T3** Unit test reassembly timeout: insert one fragment, advance timer past 30s, verify group is dropped
- [ ] **8.T4** Unit test reassembly overflow: insert 9 groups, verify oldest is evicted
- [ ] **8.T5** Unit test ICMP unreachable: send UDP to closed port, verify ICMP type 3 is generated
- [ ] **8.T6** Unit test RTT estimation: simulate ACKs with known RTT, verify `srtt` converges
- [ ] **8.T7** Unit test Nagle: send two small writes, verify they are coalesced into one segment
- [ ] **8.T8** Integration test: send a 4000-byte UDP payload, verify it arrives reassembled at the receiver

### Phase 8 Gate

- [ ] **GATE**: IPv4 fragmentation TX produces correct fragment headers for payloads exceeding MTU
- [ ] **GATE**: IPv4 reassembly correctly reconstructs packets from out-of-order fragments
- [ ] **GATE**: Reassembly groups are dropped after 30s timeout (no memory leak)
- [ ] **GATE**: ICMP destination unreachable is sent for UDP packets with no matching socket
- [ ] **GATE**: TCP RTO uses RTT estimation, not a fixed value

---

## 9. Phase 9: Multi-NIC, Packet Filter, Raw Sockets

> **Extends the stack to multiple network interfaces, adds a 5-tuple packet filter, and introduces capability-gated raw sockets.**
> **Kernel changes required**: Yes — updates to `NetDeviceRegistry`, new `drivers/src/net/filter.rs`, new `drivers/src/net/raw_socket.rs`
> **Difficulty**: Very High
> **Depends on**: Phase 8

### Background

Everything built in Phases 1-8 was designed with multi-NIC in mind (per-interface ARP caches, route table with dev_index, `NetDevice` trait) but only tested with one interface. Phase 9 exercises those decisions by registering a second NIC and verifying that routing, ARP, and socket demux all work correctly across interfaces.

The packet filter adds hook points at `PREROUTING`, `INPUT`, and `OUTPUT` where simple 5-tuple rules (src IP, dst IP, src port, dst port, protocol) can accept or drop packets. This is not iptables — no NAT, no conntrack, no stateful inspection. Just a linear rule list with accept/drop verdicts.

Raw sockets let privileged userspace programs send and receive arbitrary IP packets. This enables `ping` (ICMP echo) and `traceroute` (UDP with TTL manipulation) without kernel changes.

### 9A: Multi-NIC Routing and ARP

- [ ] **9A.1** Verify `NetDeviceRegistry` handles multiple devices correctly:
  - Register a second VirtIO-net device (or a second loopback for testing)
  - Verify `route_lookup()` returns the correct `dev_index` for each subnet
  - Verify `NeighborCache` maintains separate entries for each interface
- [ ] **9A.2** Update `ipv4::handle_rx()` to handle packets on any interface:
  - Check if `dst_ip` matches any of our interface addresses (not just the first one)
  - If `dst_ip` matches a local address: deliver to socket layer
  - If `dst_ip` doesn't match any local address: drop (no forwarding for now)
- [ ] **9A.3** Update `ipv4::send()` for multi-NIC:
  - `route_lookup()` already returns `dev_index`; verify this is used correctly
  - Source IP selection: use the address of the outgoing interface
  - If source IP is `INADDR_ANY` in the socket: fill in the interface address before TX
- [ ] **9A.4** Update `ifconfig` shell app to display all interfaces:
  - Iterate `NetDeviceRegistry` and `NetStack` to display each interface's config
  - Show stats from `NetDeviceStats` per interface

### 9B: Packet Filter

- [ ] **9B.1** Create `drivers/src/net/filter.rs` with `FilterRule` struct:
  - Fields: `src_ip: Option<u32>`, `dst_ip: Option<u32>`, `src_port: Option<u16>`, `dst_port: Option<u16>`, `protocol: Option<u8>`, `action: FilterAction`
  - `FilterAction` enum: `Accept`, `Drop`
  - `None` in any field means "match any" (wildcard)
- [ ] **9B.2** Implement `FilterChain` with three hook points:
  - `PREROUTING`: applied to all incoming packets before routing decision
  - `INPUT`: applied to packets destined for local sockets
  - `OUTPUT`: applied to packets generated locally before TX
  - Each chain is a `Vec<FilterRule>` evaluated in order; first match wins
- [ ] **9B.3** Implement `FilterChain::evaluate(pkt: &PacketBuf) -> FilterAction`:
  - Extract 5-tuple from packet headers
  - Iterate rules; return action of first matching rule
  - If no rule matches: return `Accept` (default accept policy)
- [ ] **9B.4** Wire filter hooks into the ingress and egress paths:
  - `net_rx()`: apply `PREROUTING` after L2 demux, before L3 dispatch
  - `ipv4::handle_rx()`: apply `INPUT` before socket demux
  - `ipv4::send()`: apply `OUTPUT` before neighbor resolution
- [ ] **9B.5** Add `SYSCALL_FILTER_ADD` and `SYSCALL_FILTER_FLUSH` syscalls:
  - `filter_add(chain: u8, rule: FilterRule) -> i32` — appends a rule to a chain
  - `filter_flush(chain: u8) -> i32` — clears all rules from a chain
  - Both require a capability check (privileged operation)

### 9C: Raw Sockets

- [ ] **9C.1** Create `drivers/src/net/raw_socket.rs` with `RawSocket` implementing `SocketOps`:
  - `socket(AF_INET, SOCK_RAW, protocol)` creates a raw socket for the given IP protocol
  - `send()`: takes a complete IP payload (no IP header prepended by kernel), wraps in IP header
  - `recv()`: delivers complete IP packets (including IP header) to userspace
- [ ] **9C.2** Implement capability check for raw socket creation:
  - Define `CAP_NET_RAW` capability flag in `abi/src/net.rs`
  - `handle_socket()` checks the calling process's capability set before creating a raw socket
  - Return `EPERM` if the process lacks `CAP_NET_RAW`
- [ ] **9C.3** Wire raw socket demux into `ipv4::handle_rx()`:
  - After normal TCP/UDP dispatch, check if any raw socket is registered for the protocol
  - Deliver a copy of the packet to each matching raw socket's receive queue
  - Raw sockets receive all packets for their protocol, regardless of port
- [ ] **9C.4** Add `raw_socket` userland example in `userland/src/bin/ping.rs`:
  - Opens a raw socket with `IPPROTO_ICMP`
  - Sends ICMP echo request, waits for echo reply via `poll()`
  - Displays RTT
  - This is the canonical test for raw socket functionality

### Phase 9 Test Coverage

- [ ] **9.T1** Integration test: two interfaces configured, verify packets route to correct interface based on destination
- [ ] **9.T2** Integration test: ARP cache entries are separate per interface (no cross-contamination)
- [ ] **9.T3** Unit test `FilterChain::evaluate`: DROP rule matches specific dst_port, other ports pass
- [ ] **9.T4** Unit test wildcard filter: rule with all `None` fields matches every packet
- [ ] **9.T5** Integration test: add DROP rule for port 8080, verify UDP to port 8080 is dropped, port 8081 passes
- [ ] **9.T6** Integration test: `PREROUTING` DROP rule blocks packet before it reaches any socket
- [ ] **9.T7** Integration test: raw socket receives ICMP packets, `ping.rs` sends echo and receives reply
- [ ] **9.T8** Integration test: raw socket creation without `CAP_NET_RAW` returns `EPERM`

### Phase 9 Gate

- [ ] **GATE**: Two network interfaces route packets independently with separate ARP caches
- [ ] **GATE**: Packet filter DROP rules prevent delivery to sockets
- [ ] **GATE**: Raw socket `send()` and `recv()` work for ICMP protocol
- [ ] **GATE**: Raw socket creation is blocked without `CAP_NET_RAW` capability
- [ ] **GATE**: `ping.rs` successfully sends ICMP echo and receives reply via raw socket

---

## Dependency Graph

```
Phase 1: Net Core Contracts (PacketBuf + NetDev)
    |
    v
Phase 2: ARP Neighbor Cache v1
    |
    v
Phase 3: L3 Config + Loopback + Routing v1
    |
    v
Phase 4: Userspace UDP Sockets v1
    |
    v
Phase 5: Userspace TCP Stream Sockets v1
    |
    +---> Phase 6: Blocking/Nonblocking + poll/select
    |         |
    |         v
    |     Phase 7: Userspace DNS Resolver
    |
    v
Phase 8: IPv4 Robustness + TCP Hardening
    |
    v
Phase 9: Multi-NIC + Packet Filter + Raw Sockets
```

Notes on the graph:

- Phase 6 depends on Phase 5 (needs TCP socket layer for blocking connect/accept)
- Phase 7 depends on Phase 6 (needs poll() for DNS timeout handling)
- Phase 8 depends on Phase 5 (needs TCP state machine for timer hardening)
- Phase 9 depends on Phase 8 (needs robust IPv4 before adding multi-NIC complexity)
- Phases 6+7 and Phase 8 can be developed in parallel after Phase 5 completes

---

## Future Considerations

The nine phases above bring SlopOS to a functional, BSD-socket-compatible IPv4 stack. What comes after is a longer road, and none of it is in scope for this plan. But the Wheel of Fate spins on.

**IPv6**: The neighbor cache (Phase 2) and routing table (Phase 3) were designed with per-interface scoping specifically to make IPv6 NDP and routing tables addable without a rewrite. `AF_INET6` would be a new address family in `abi/src/net.rs`. NDP replaces ARP. The socket layer is already protocol-agnostic enough to handle it. Estimated effort: one full plan of similar scope.

**TLS**: Not a kernel concern. A userspace TLS library (mbedTLS port, or a Rust `rustls` adaptation) would sit on top of TCP stream sockets from Phase 5. The kernel provides the socket; TLS is the application's problem.

**HTTP**: Same as TLS. A userspace HTTP client/server library on top of TCP sockets. The kernel doesn't care.

**epoll**: Phase 6 adds `poll()`. `epoll` is the scalable evolution: an fd-based event notification mechanism that avoids re-scanning the entire fd set on each call. It requires a more sophisticated readiness tracking structure than the simple wait queue from Phase 6, but the wait queue is the right foundation to build on.

**WiFi**: A new `NetDevice` implementation. The 802.11 association and authentication state machine would live in a new driver. The rest of the stack (ARP, routing, sockets) would be unchanged. The `NetDevice` trait boundary from Phase 1 makes this possible.

**VLANs**: 802.1Q VLAN tagging is a thin shim between the Ethernet driver and the L3 demux. It would be a new `NetDevice` wrapper that strips/inserts VLAN tags and presents virtual interfaces to the routing layer.

**Bonding and bridging**: Multiple `NetDevice` instances aggregated into a single logical interface. The `NetDeviceRegistry` from Phase 1 is the right place to add this abstraction.

**DNSSEC**: An extension to the userspace DNS resolver from Phase 7. The resolver already parses DNS responses; DNSSEC adds signature verification. No kernel changes required.

**Packet capture (pcap)**: A raw socket variant that captures all traffic on an interface, regardless of protocol. The filter hook points from Phase 9 are the natural place to tap.

The wizards' work is never done. The Wheel of Fate keeps spinning.

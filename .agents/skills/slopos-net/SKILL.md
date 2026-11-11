---
name: slopos-net
description: >
  The SlopOS network stack, its cards, kept network settings, TLS and the remote control of a real machine.
  Use when a task touches `net/`, `net-core/`, `rtl8168-core/`, `tls-core/`, `remote-core/`, `ip`, `netctl`, `/etc/network`, `remoted`, `scripts/remote.py`, `just remote*`, or mentions NIC, DHCP, TCP, sockets or pairing.
---

## Read first

- `docs/architecture/networking`: the stack, cards, `ip`, `netctl`, admission rules, TLS
- `docs/self-hosting/remote-control`: `remoted`, the broker, pairing, the safety boundary
- `docs/architecture/usb`: USB Ethernet adapters
- `docs/architecture/settings`: the Settings app's network page

## Rules a change must keep

- Keep all work in this repository and in QEMU/OVMF: never copy kernels to system paths, install or chainload on real hardware, or touch the host's `/boot` or `/efi`. One exception: a SlopOS machine the user booted on a base paired with this host (`just remote-serve` and the bootstrap it prints) may be driven through `scripts/remote.py` alone, which runs programs, moves files, installs a base on its boot partition and a kernel into its spare slot. Never write its ESP, its firmware variables beyond what `bootctl` does, or another system's partitions. Commit a slot (`--commit`, `bootctl commit`) only once the boot it came from answered with the expected tag.
- Every NIC enters through `nic::publish` and leaves through `nic::retire`, with no driver lock held. A driver keeps no thread; its interrupt only calls `napi::wake_napi`.
- `ipv4::admits` alone decides what a non-loopback device accepts: the weak host model, plus UDP port 68 on a device with no address.
- `ipv4::send` stamps the source MAC of the device the route picked; a route delete takes only the named interface's route.
- `rtl8168-core` is the chip over `RegisterBus`; versions outside its table are declined. Take facts from Linux's `r8169`, never its GPL code or prose.
- `ip` lasts until the next start; `netctl` keeps `/etc/network`. Neither `org.slopos.network.*` action is the paired agent's.
- `network::plan` stays pure and returns one phase; a second apply asks for nothing. `netctl` changes under `/run/netctl.lock`.
- The pairing lives only in the base; `remoted` never dials from `/etc` or arguments, and nothing on SlopOS listens.
- remoted's privd connection goes only to a mechanism or the shell.
- `tls-core` stays `no_std`, `forbid(unsafe_code)`; signers stay under `test-server`. Replace the CA bundle only with `scripts/update_ca_bundle.sh`.

## Checks

- `just test 'slopos_net::*'`: the kernel's network stack
- `just test 'slopos_core::utests::utest_ip_netctl'`: `netctl` in a guest
- `just test-host`: `net-core`, `rtl8168-core`, `settings-core`, `tls-core`, `remote-core`
- `just test-usb`: a USB adapter published, leased, retired
- `just test-remote`: remote control under QEMU

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.

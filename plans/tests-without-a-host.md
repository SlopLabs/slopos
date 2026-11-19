# Running the suite without a host (spike)

This file is a spike. Its work is research, and its result replaces it: when
the questions below are answered, rewrite this file as the plan they lead
to, or delete it and say in the commit why no plan follows.

## The goal

On a computer SlopOS was installed on, `just test` runs the kernel's tests
(`stest!`) and the userland tests (`utest!`) and reports as the host's
`tools/run_tests` does, so a change is checked where it was made. The
kernel's tests are a kernel change and take one tried boot; the userland
tests test programs and run on the kernel that is up, under the tests base
tried with `bootctl base try`.

## What is known

| Part | Today | On a machine with no host |
|------|-------|---------------------------|
| Selection | `tests=`, `tests.run=`, `tests.skip=` on the command line | `bootctl install --cmdline` stamps them into the spare slot's kernel |
| Kernel phase | Boot step drivers/90, before the root mounts, on the `vfs_ready` fixture's RAM `/` | The same; disks are reached by claiming them |
| Scratch disks | Claimed by the QEMU identity the suite attaches (`/dev/disk/by-id`), never by node name | No disk carries those identities, so a test that writes one cannot run |
| Output | KTAP through the kernel log to COM1; subtest lines skip the ring `/dev/kmsg` reads (`ktesting/src/capture.rs`) | Lost at the end of the boot |
| End of run | `isa-debug-exit`, then power-off (`ktesting/src/qemu_signal.rs`) | A power-off; nothing boots the default slot again |
| Result kept across a reset | None; the crash store keeps a panic's record only: 64 KiB slots, an NVMe disk with a panic queue, and `bootctl collect` marks the slot a record names as crashed | None |
| Userland tests' list | The kernel's registry (`core/src/utests.rs`) | Not in userland |
| Starting one | The kernel spawns it at the uid and capabilities its `utest!` names (`Endowment::Kernel`, `core/src/exec/utest.rs`) | Not possible from userland today |
| Its report | `SYSCALL_TEST_REPORT` into a per-task ring of 192 entries that only the kernel's runner drains | No call reads a ring back |
| What only a test boot gives | `test_key` and init's test mode (no login, desktop or roulette) need `tests=on` alone, on any kernel; five kernel tests registered `kind = Userland` need the tests kernel | |
| QEMU's machine | `mount_test` (`/media`), `priv_test` and `spawn_privilege_test` (`/dev/nvme0n1`, `/dev/vda`, `/dev/sda`), `curl_e2e_test`, `ip_e2e_test`, `dns_resolve_test`, `settings_test` (SLIRP's 10.0.2.x), `toolchain_test` (an ssh peer) | Absent |

## Questions

Each answer is a measurement or a reading of the source, with where it came
from.

### The kernel's tests on hardware

1. Which kernel tests need a disk at all, and which of them could run on a
   disk the suite makes for itself: a RAM block device carrying the scratch
   identities, or a file on the root? What does each cost in memory?
2. How large is one suite's KTAP output, measured, and where does it go: a
   non-panic record in the crash store, several of them, the root's
   `/var/log` once it mounts, or the boot partition? What must
   `bootctl collect` tell apart, so that a test run never marks its slot
   crashed?
3. How does a run end on hardware: a reset that boots the default slot again
   (`tests.shutdown=reboot`), and who reports the result at that boot?
4. Which tests assume QEMU's machine (`isa-debug-exit`, the virtio and NVMe
   controllers, the USB stick, SLIRP), and which of them should skip on
   another machine rather than fail?
5. Under `tests=on` panic recovery is off and a recoverable oops is fatal.
   What does an expected panic (`EXPECTED_PANIC`) do to a machine with
   `panic=reboot` in its stamped words?

### A userland runner

6. Can the list leave the kernel: a manifest in the tests base naming each
   program, its arguments, uid and capabilities? Can a runner holding what
   init holds start each at that uid with those capabilities, as
   `Endowment::Kernel` does, and what does that runner then hold that a
   person's session must not reach?
7. How does a report reach the runner, unchanged in KTAP, so that
   `tools/run_tests` and the log gates read either harness's output: the
   slibc harness writing to a descriptor the runner gives, or a call that
   reads a child's ring?
8. `test_key` and init's test mode need `tests=on`. Can they become a
   capability the runner holds on any boot, or does the runner run only on
   a `tests=on` boot of the running kernel, which costs a reboot?
9. What becomes of the five kernel tests that run in the userland phase?
10. With the tests base tried live, what of the machine do the tests change
    (accounts, the package store, `~/.local`, sessions, the network),
    and how is it put back: a RAM root for the run, or a root of their own?

## How

- Work on the development machine (`just boot`) or the self-hosting test
  root, never on a person's installed disk. A test that would write a disk
  it did not make is a finding, not a step to run.
- Change nothing in the tree to make a step pass. A step that fails is a
  finding: write down the error and the cause.
- Keep the measurements beside the claims, with the commit they were taken
  at.

## Done when

Every question has an answer or a stated reason it could not be answered,
and this file has become the plan: phases in an order, each with what it
builds, what it needs first and how it is checked, as
`self-hosting-on-hardware.md` is written.

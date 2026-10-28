# SlopOS Vulnerability Audit and CVSS Scoring

The ledger of open findings. A finding is removed once fixed, and an ID is
never issued twice.

The highest ID issued so far is **SLOPOS-2026-0085**. The next finding is
`SLOPOS-2026-0086`.

## Open findings

Every finding below is under the bar (confidence < 80) and carries no CVSS
vector; each is kept because an attacker reaches its trigger. "Raised by"
names the evidence that would lift it.

### SLOPOS-2026-0058
- Title: Without RDRAND the CSPRNG is keyed from four TSC reads and never takes entropy again
- Status: `open`
- Confidence: 60 — evidence 35 (the seed step and the rekey are direct code), exploitability 10 (needs a CPU model without RDRAND and an observer of one output, a ClientHello random or a DNS id), reproducibility 15 (deterministic on such a model; no seed search run). Raised by: a search that recovers the seed from a captured ClientHello under `-cpu qemu64,-rdrand`.
- Impact: every `getrandom` value of the boot — TLS key shares, Argon2id salts, DNS ids and ports — follows from a seed an observer can search.
- Evidence: `boot/src/boot_drivers.rs:487-498` (the fallback seed: four `rdtsc` values times constants); `boot/src/boot_drivers.rs:472-476` (an RDRAND chunk that fails stays zero); `drivers/src/random.rs:121-138` (a rekey derives the next key from the current one alone); `drivers/src/random.rs:168-194` (the early seed, same fallback).
- Repro: boot with `-cpu qemu64,-rdrand`; the log says `CSPRNG seeded from tsc`. The unknown is the TSC at the `csprng seed` step plus the small deltas between four consecutive reads.
- Remediation: a pool fed by interrupt and timer jitter that reseeds the generator, and no TLS or password hashing until it holds enough on a CPU without RDRAND.

### SLOPOS-2026-0059
- Title: The commit ceiling and the OOM killer are one pool for every person, and the killer consults no uid
- Status: `open`
- Confidence: 60 — evidence 35 (the people's rows limit tasks and processes only; `choose_victim` shields by capabilities alone while `kill` also needs the uid rule), exploitability 15 (two people running at once; the attacker spreads its promises so each of its processes holds less than the victim's heaviest, within its 256 processes), reproducibility 10 (not run). Raised by: a utest that fills the ceiling as uid 1000 and sees uid 1001's process killed.
- Impact: one person's processes refuse another person's `mmap` and `brk` (`ENOMEM`), and a forked copy's write has the kernel `SIGKILL` another person's process — their terminal, an app with unsaved work — which `kill` refuses the writer. A process holding a gated capability, as the compositor does, stays shielded.
- Evidence: `mm/src/commit.rs:38-40` (one `CommitPages` limit, the root's); `slopos-ostd/src/process/quota/persons.rs:85-91` (a person's row limits processes and tasks alone); `mm/src/oom.rs:322-345` (the victim is the heaviest killable process); `core/src/oom.rs:39-67` (killable is `signal_dominates` alone); `core/src/syscall/signal.rs:132-154` (`kill` also needs `uid_permits`); `mm/src/oom.rs:21-24` (states that only a process the writer could `kill` is taken).
- Repro: `users add bob`; bob runs one process holding 256 MiB of touched anonymous memory. As uid 1000, start processes that each `mmap` and touch 200 MiB until `sys_info` shows the commit headroom near zero, then `fork` in one and write the child's copy of a private page: the log reads `OOM: killed pid <bob's process>`.
- Remediation: take only a process the writer may `kill` (the uid rule beside dominance), and give each person a share of the commit ceiling.

### SLOPOS-2026-0060
- Title: A client can draw a likeness of privd's question, and no key a client cannot see proves the real one
- Status: `open`
- Confidence: 45 — evidence 25 (the secure attention key locks the screen and nothing else; a client's surface holds any pixels), exploitability 15 (the person must type a password into a dialog a client drew; `ASKING` tells a requester the moment a real question shows, so a lookalike can follow one), reproducibility 5 (no lookalike built). Raised by: a client that captures a password typed into a drawn card under `just ui-shots`.
- Impact: session code collects the person's password, or an administrator's, by drawing the question's card. A real question names the asking program and pid but not the terminal or window, so a request background code raises right after the person's own reads as theirs; an extension's action is described in the extension's own words, shown under its name.
- Evidence: `userland/src/apps/compositor/gate.rs:187-200` (Ctrl+Alt+Delete locks); `userland/src/apps/compositor/mod.rs:390-431` (a question takes keys only while it is up; no key summons or confirms it); `userland/src/apps/privd/mod.rs:791-797` (`ASKING` on `SHOWN`); `userland/src/apps/privd/mod.rs:920-928` (an extension's `describe` in the headline).
- Repro: a client maps a window over the area between the menu bar and the dock, draws the card for `org.slopos.priv` with a password field, and reads the keys typed into it as ordinary input.
- Remediation: a trusted path — the question shows only after the secure attention key, or carries what no client can draw (a mark in the menu bar, an image the person chose) — and it names the requester's terminal or window.

### SLOPOS-2026-0061
- Title: A requester keeps its mechanism, and the exclusive group the mechanism holds, for as long as it keeps the mechanism blocked
- Status: `open`
- Confidence: 45 — evidence 30 (privd waits on a mechanism with no bound and releases its group only at the reap; the mechanism writes to descriptors the requester supplied; `users passwd` reads its secret with no timeout), exploitability 10 (an administrator's session asks `boot.oneshot` and `boot.commit` unasked; a standard user needs one click on `accounts.password`), reproducibility 5. Raised by: a utest that holds `boot-slots` for a minute this way.
- Impact: an administrator's session code, unasked, holds `boot-slots` — every kernel and base install, commit and one-shot, the paired agent's included — or after one click `accounts`, every account change, until that process exits; a privd started again takes no exclusive group while the mechanism runs.
- Evidence: `assets/privd/actions:108-118` and `:132-142` (`allow admin`, `exclusive boot-slots`); `assets/privd/actions:265-274` (`allow user confirm`, `exclusive accounts`); `userland/src/apps/privd/mod.rs:720-735` (the claim ends at the reap); `userland/src/apps/privd/mod.rs:1308-1340` (an unbounded wait; a requester that leaves does not end it); `userland/src/apps/privd/mod.rs:741` (a predecessor's mechanism keeps the group); `userland/src/secret.rs:76-90` (a blocking `read_line`).
- Repro: as an administrator, fill a pipe, send privd `org.slopos.boot.oneshot` with the pipe's write end as stdout and stderr, and keep the read end unread; `remote.py install` and root's `bootctl install` are refused while that process lives.
- Remediation: privd owns a mechanism's output pipes and relays them, so no requester's descriptor blocks a mechanism, and a mechanism in an exclusive group runs under a deadline.

### SLOPOS-2026-0062
- Title: Extension users, the greeter and a seventeenth person debit the root, bounded by no person's share
- Status: `open`
- Confidence: 45 — evidence 35 (a spawn takes a person's row only for a uid in `PERSON_UIDS`, and the rows are the first sixteen claimed), exploitability 5 (an extension's service, or sixteen other people in one boot), reproducibility 5. Raised by: an extension service fork loop that fills the task table in a utest.
- Impact: an extension's service (uids 100-999) or a seventeenth person forks without the quarter of `MAX_PROCESSES` and `MAX_TASKS` a person's row allows, so its fork bomb or unreaped children fill the machine's task table, which the rows exist to prevent.
- Evidence: `core/src/exec/mod.rs:384-394` (`account_for`); `slopos-ostd/src/process/quota/persons.rs:31-46` (the root once sixteen rows are taken); `slopos-ostd/src/process/quota/persons.rs:85-91` (the rows' limits).
- Repro: an extension whose service forks in a loop; once the table is full, every person's spawn fails.
- Remediation: a row per extension, keyed by its record's number, and a shared row bounded as one person's for every uid past the sixteenth.

### SLOPOS-2026-0063
- Title: `QEMU_DEBUG=1` opens a GDB stub on every interface and a monitor socket at a fixed `/tmp` path
- Status: `open`
- Confidence: 40 — evidence 30 (`-s` is QEMU's `-gdb tcp::1234`, which binds every address; the monitor path is fixed and removed with `rm -f` first), exploitability 5 (only while a developer debugs), reproducibility 5 (not run). Raised by: a connection from another host during a debug boot.
- Impact: any host that reaches the developer's machine on TCP 1234 reads and writes the guest's memory and registers, and with them its root and the workspace clone; another local user who creates `/tmp/slopos-monitor.sock` first keeps QEMU from starting.
- Evidence: `scripts/qemu_run.sh:569-576`.
- Repro: `QEMU_DEBUG=1 just boot-log`; from another host, `gdb -ex 'target remote <host>:1234'`.
- Remediation: `-gdb tcp:127.0.0.1:1234` and a monitor socket under `builddir/`.

### SLOPOS-2026-0064
- Title: A mechanism reaches by path what the person allowed: its working directory and the files its command line names
- Status: `open`
- Confidence: 35 — evidence 25 (a working directory is a path; privd's check before the spawn compares `(st_dev, st_ino)` and `st_dev` is 0 for every file; `bootctl` installs whatever a path holds when no digest follows it, and `selfhost.sh` names none), exploitability 5 (session code of the same person must move a directory or replace a file between the answer and the read), reproducibility 5. Raised by: a test that swaps a base under `bootctl base install` after the answer.
- Impact: a `priv` command allowed in one directory runs, holding `DacOverride` and `Mount`, against relative paths session code re-pointed by renaming directories; a base or kernel allowed by its path is replaced before `bootctl` reads it, and a base runs at once.
- Evidence: `slopos-ostd/src/task/fs_context.rs:20` (`CwdPath`); `userland/src/apps/privd/mod.rs:960` and `:1202-1207` (`still_at`); `fs/src/vfs/traits.rs:126` (`st_dev = 0`); `assets/privd/actions:59`, `:86`, `:385-386` (`cwd`); `userland/src/apps/bootctl.rs:463-484` (the digest is optional) and `:1072`; `scripts/selfhost.sh:89` and `:100`.
- Repro: as an administrator run `scripts/selfhost.sh install`; while the question shows, replace the base it names with another that passes `boot_core::base::check`, then allow; `bootctl status` names the other build tag.
- Remediation: the requester opens the directory and files and privd hands the mechanism descriptors, or each image travels with the SHA-256 the question shows; `stat` reports a real `st_dev`.

### SLOPOS-2026-0065
- Title: What `priv` makes is the requester's, so their later session code rewrites what they placed in shared directories
- Status: `open`
- Confidence: 35 — evidence 25 (`org.slopos.priv` runs as the requester, so a file it makes in a uid-0 directory is theirs), exploitability 5 (an administrator must first make the file with `priv`), reproducibility 5. Raised by: a utest on a two-person root that rewrites a file `priv` made in `/usr/local/bin`.
- Impact: once an administrator makes a file in `/usr/local/bin` or `/etc` with `priv`, any code of their session rewrites it unasked, and other people run or read it.
- Evidence: `assets/privd/actions:381-392` (`uid requester`); `userland/src/apps/privd/mod.rs:1216-1240` (the requester's ids); `userland/src/apps/priv.rs:264-274` (`priv` itself trusts only what uid 0 alone changes, which such a file is not).
- Repro: with a second person added, `priv cp tool /usr/local/bin/tool`; `ls -l` names the administrator; their shell rewrites the file without a question, and the second person's `tool` runs the new bytes.
- Remediation: a file `priv` makes in a directory only uid 0 may change is uid 0's.

### SLOPOS-2026-0066
- Title: A uid's session code can take every privd connection its person's requests need
- Status: `open`
- Confidence: 30 — evidence 25 (32 connections per uid and eight per uid and program, an idle connection kept for good, every program outside the base one key), exploitability 5 (a base requester whose stdout or stderr is a full pipe blocks with its connection open, so three base programs and one outside the base fill a uid's 32), reproducibility 0. Raised by: a utest that fills uid 1000's connections and sees its next request refused.
- Impact: the person's own requests — questions, password changes, installs — are refused as crowded until those processes exit; root, the paired agent and other people are unaffected.
- Evidence: `userland/src/apps/privd/mod.rs:74-80` (the limits); `userland/src/apps/privd/mod.rs:285-307` (`Admitted::take` keyed on uid and program path); `userland/src/apps/privd/mod.rs:428-433` (`Other` has no path, one key); `userland/src/apps/privd/mod.rs:688-718` (a connection lives until its requester closes it).
- Repro: as uid 1000, hold eight idle connections from a program outside the base, then start `bootctl status`, `keymap` and `users passwd` eight times each with stdout and stderr a full pipe; the person's next `priv` is refused as crowded.
- Remediation: count only connections with a request in flight and close idle ones after a bound, or keep a share for the programs the person's desktop starts.

### SLOPOS-2026-0067
- Title: Until the next boot, a descriptor the first person opened in `/usr/local` before the hand-over still writes what later people run
- Status: `open`
- Confidence: 35 — evidence 25 (the hand-over changes owners in place; only `priv` waits for a settled boot; `users add` only advises a restart), exploitability 5, reproducibility 5. Raised by: a utest that keeps a writable descriptor across `users add` and changes what the new person runs.
- Impact: code of the first person's that opened `/usr/local/bin/tool` for writing before `users add` keeps changing it while the second person runs it, until a restart.
- Evidence: `userland/src/root_layout.rs:15-22` (`settled`); `userland/src/apps/users.rs:340-346` (the advice); `userland/src/apps/priv.rs:268-269`.
- Repro: as the first person, open `/usr/local/bin/tool` for writing and keep it; `users add bob`; write through the descriptor; bob's `tool` runs the new bytes.
- Remediation: revoke writable descriptors under `/usr/local` at the hand-over, or keep the new person off `/usr/local` until a settled boot.

### SLOPOS-2026-0068
- Title: A rename's permission checks and the rename look the names up separately, with no lock across both
- Status: `open`
- Confidence: 30 — evidence 25 (`check_rename` and `rename_resolved` each look up both names; the seal check is a third walk), exploitability 5 (the one check a racing rename evades is write on a directory moved between parents, which guards only its `..`; the sticky checks hold, since only a name the caller could already remove can be swapped in), reproducibility 0. Raised by: a race loop that moves another uid's directory between parents.
- Impact: a process with write on two parents moves a directory it may not write to another parent, changing its `..`.
- Evidence: `fs/src/vfs/ops.rs:737-743`; `fs/src/vfs/ops.rs:775-802`; `fs/src/vfs/ops.rs:807-850`.
- Repro: one thread loops `rename(p/a, q/a)`, `a` a directory the caller may write; another swaps `p/a` with a directory of another uid's in `p`.
- Remediation: hold the parents' rename lock across the checks and the rename, checking the inodes the rename acts on.

### SLOPOS-2026-0069
- Title: A kept answer reruns the same command with whatever input its requester gives, and an extension sets its own `keep`
- Status: `open`
- Confidence: 30 — evidence 25 (a kept answer keys the command line, directory, session and live parent, never the input; an extension's declaration takes `keep` up to 900 that no grant names), exploitability 5, reproducibility 0. Raised by: a utest that reruns a kept `priv tee` with other input, unasked.
- Impact: within 300 s of a person allowing `priv tee /etc/x` from a shell, code that shell starts reruns it unasked with its own standard input; an extension's action asked once stays answered for up to fifteen minutes.
- Evidence: `userland/src/apps/privd/answers.rs:19-35` and `:49-62`; `assets/privd/actions:387`; `extension-core/src/declare.rs:736`; `priv-core/src/policy.rs:719-723`.
- Repro: allow `priv tee /etc/motd` from a shell; within five minutes a script started from it runs `echo X | priv tee /etc/motd` and is not asked.
- Remediation: no kept answer for a command fed by the requester's input, and an extension's `keep` held to its key's ceiling.

### SLOPOS-2026-0070
- Title: A mechanism's own terminal is open to session code in the unlock window, and then not held to its session
- Status: `open`
- Confidence: 30 — evidence 25 (`openpty` unlocks the slave before privd's requester takes it by `TIOCGPTPEER` and locks it again; terminal ioctls check no session; a terminal whose foreground group has died serves any session), exploitability 5, reproducibility 0. Raised by: a utest that opens the slave in the window and turns echo on during `users passwd`.
- Impact: a process of the requester's uid that opens `/dev/pts/N` in the window keeps a descriptor on the terminal the installer, an accounts mechanism or the rescue shell reads: it turns echo back on while a password is typed, flushes the input or stops the output, and between a rescue shell's programs reads what is typed.
- Evidence: `userland/src/syscall/process.rs:323-338`; `userland/src/privd.rs:491-512`; `core/src/syscall/fs/poll_ioctl_handlers.rs:927-934`; `drivers/src/tty/session.rs:69`, `:82-91`, `:110-116`.
- Repro: as uid 1000 loop opening the next `/dev/pts/N` while another process runs `users passwd`; once open, `TCSETS` with `ECHO` set as the prompt shows.
- Remediation: never unlock a mechanism's slave (take it by `TIOCGPTPEER` alone); hold state-changing terminal ioctls to the terminal's session; treat a dead foreground group as no access from another session.

### SLOPOS-2026-0071
- Title: An extension's program keeps its extension's label with a library from `LD_LIBRARY_PATH` that maps nothing executable
- Status: `open`
- Confidence: 30 — evidence 25 (privd checks only base `program` principals for static linking; the loader honours `LD_LIBRARY_PATH` for a process that is not `AT_SECURE`; only an executable mapping forfeits the program), exploitability 5 (the interposed data must steer the program), reproducibility 0. Raised by: a utest in which an extension program under such a library asks privd and is admitted.
- Impact: session code runs an extension's program with a data-only library interposing its data symbols, and the program still asks privd as that extension's `program` principal, reaching the actions its declaration lets the program ask unasked, with arguments the program would not send.
- Evidence: `userland/src/apps/privd/mod.rs:346-347` and `:375-385`; `slibc/src/ld_so/mod.rs:644-655`; `core/src/syscall/memory_handlers.rs:64-71`; `mm/src/process_vm.rs:2924-2946`.
- Repro: build a shared object whose only `PT_LOAD` is writable data defining a symbol the program imports; run the program with `LD_LIBRARY_PATH` naming it; `SO_PEEREXTENSION` on its privd connection still names the activation.
- Remediation: forfeit the program when the loader maps any object from outside the base and the activation, or ignore `LD_LIBRARY_PATH` for an extension's program.

### SLOPOS-2026-0072
- Title: Init ends a login tag's processes one `kill` at a time from a listing, so a process that forks fast outlives the end and many stall init
- Status: `open`
- Confidence: 30 — evidence 25, exploitability 5, reproducibility 0. Raised by: a utest whose fork loop outlives `end-session`.
- Impact: a person's process forking in a loop survives their logout (the request is answered with how many did), and the old broker's survive a pairing change; ending a person's 256 processes costs init a task-table walk per `kill`, while it answers no other session.
- Evidence: `userland/src/apps/init/session.rs:26-27`, `:839`, `:869-897`; `userland/src/apps/init/mod.rs:741-772` (one `SIGKILL` pass, then the new remoted).
- Repro: a session process whose children each fork again in a loop; `end-session` answers with a count above zero and `process_list` still shows the tag.
- Remediation: one kernel call that freezes and kills every process of a login tag, as `cgroup.kill` does.

### SLOPOS-2026-0073
- Title: privd and init check an extension program's activation, then spawn it by path
- Status: `open`
- Confidence: 25 — evidence 20 (the check opens the file and drops it; the spawn walks the path again), exploitability 5 (the replacement takes `ExtensionUpdate`: an owner's update, a session's `install-signed` of a key's newer image, or a deactivation over a file a `DacOverride` holder left under the mount point), reproducibility 0. Raised by: a test hook in the window that shows the replacement image running with the old grant.
- Impact: an activation replaced or taken away between the check and the spawn runs the new image's program, or a file on the root under `/opt/<name>`, with the old declaration's grant until the reload ends it.
- Evidence: `userland/src/apps/privd/mod.rs:944-947`, `:963-971`, `:1281-1282`; `userland/src/activation.rs:71-81`; `userland/src/apps/init/extensions.rs:1036-1039`, `:1118-1131`, `:1197-1200`.
- Repro: nearest deterministic: a test hook between `activation::holds` and the spawn that deactivates the extension; the mechanism starts from the root's file.
- Remediation: spawn from the descriptor the check opened, or have the kernel refuse a spawn whose program is not of the named activation.

### SLOPOS-2026-0074
- Title: Keys typed between one compositor's exit and the next one taking the input sink reach the console and its relay
- Status: `open`
- Confidence: 25 — evidence 20, exploitability 5 (the typist and a console session must coincide with a compositor's exit), reproducibility 0. Raised by: a utest that types after a compositor exits and reads the keys from a console session.
- Impact: what a person types into their desktop as its compositor exits, a password among it, lands at the console, which `/bin/login` relays to the console session running, another person's shell included.
- Evidence: `drivers/src/input_event.rs:556-566`; `drivers/src/keyboard.rs:258`; `userland/src/apps/login.rs:89-91`.
- Repro: with bob logged in at the console, kill uid 1000's compositor while typing; bob's shell reads the keys.
- Remediation: while a graphical session is in front, keys reach no console until init hands the sink on.

### SLOPOS-2026-0075
- Title: An extension's session service runs as the first person, uid 1000, holding the extension's grant, whoever installed it
- Status: `open`
- Confidence: 25 — evidence 20, exploitability 5, reproducibility 0. Raised by: a utest showing the session service as uid 1000 after uid 1001 installed it.
- Impact: an extension an administrator other than the first person installs runs code as the first person, in their home, holding capabilities the first person lacks.
- Evidence: `userland/src/apps/init/extensions.rs:1449-1456`.
- Repro: as uid 1001, an administrator, install an extension declaring a session service; `process_list` shows it as uid 1000.
- Remediation: run a session service as the person whose desktop is in front, for one who allowed the grant, and name that person in the install question.

### SLOPOS-2026-0076
- Title: Any process pins a retired base generation or extension activation by holding one of its files, and their slots are few
- Status: `open`
- Confidence: 25 — evidence 20, exploitability 5 (the owner must switch or update repeatedly), reproducibility 0. Raised by: a utest that pins sixteen generations and sees `base_activate` answer `EBUSY`.
- Impact: session code that holds a file of each generation and activation keeps them resident: past sixteen generations, or a quarter of memory, a live base switch answers `EBUSY` until a reboot, and past 32 activations no extension activates, updates or enables; neither is charged to the holder.
- Evidence: `fs/src/basefs/mod.rs:48`; `fs/src/basefs/reap.rs:1-10`; `abi/src/extension.rs:20`; `fs/src/extension/mod.rs:50` and `:294`.
- Repro: as uid 1000, after each `bootctl base install` open a file of `/bin` and keep it; the seventeenth live switch is refused.
- Remediation: after a grace, revoke a retired generation's descriptors and mappings, or charge it to its holders.

### SLOPOS-2026-0077
- Title: The install medium is known by a GPT disk GUID any disk can carry
- Status: `open`
- Confidence: 20 — evidence 20, exploitability 0 (the disk is written by a `RawBlock` holder or by hand), reproducibility 0. Raised by: a boot from a CD that mounts a planted stick's payload.
- Impact: a disk copying the medium's GUID with a payload partition is mounted as the payload on a live boot from a CD, whose own disk the kernel cannot read, and the installer installs its toolchain and `/src`; beside the stick it makes the payload and the crash store ambiguous, and any disk carrying it is never offered.
- Evidence: `boot/src/boot_services.rs:637`; `drivers/src/block/mod.rs:530-553`; `drivers/src/crash.rs:163`; `userland/src/apps/installer/disk.rs:56-61`.
- Repro: give a stick the ISO's `boot/disk-guid` and a payload-type ext4 partition; boot the ISO from a CD with it plugged in.
- Remediation: authenticate the payload against a digest the `install` module carries.

### SLOPOS-2026-0078
- Title: uid 0 running `tree-install` itself reads the tree by path, without the copy privd's runs make
- Status: `open`
- Confidence: 20 — evidence 20, exploitability 0 (uid 0 must install a tree another uid can change), reproducibility 0. Raised by: a utest that swaps a link into a tree root installs.
- Impact: when a rescue shell or another uid-0 caller installs such a tree, its owner swaps a file for a link mid-copy and a file only uid 0 may read lands in `/usr/local`.
- Evidence: `userland/src/apps/tree_install.rs:54-65`.
- Repro: as root, `tree-install apps /home/user/tree /usr/local` while uid 1000 loops replacing a file in the tree with a link to `/etc/shadow`.
- Remediation: stage the copy whenever the caller is uid 0.

### SLOPOS-2026-0079
- Title: extctl's mechanism opens the paths a request names as uid 0
- Status: `open`
- Confidence: 15 — evidence 15, exploitability 0, reproducibility 0. Raised by: a utest that tells two refusals apart for paths only uid 0 searches.
- Impact: an administrator's session code, unasked through `install-signed`, learns from the refusal whether a path under a directory only uid 0 searches exists and is a regular file.
- Evidence: `userland/src/apps/extctl/mod.rs:71-81` and `:633`; `assets/privd/actions:182-191`.
- Repro: ask `org.slopos.extension.install-signed` naming `/root/a`, then a file that exists there; the refusals differ.
- Remediation: the requester opens what it names and hands the descriptors over, as `config install` does with standard input.

### SLOPOS-2026-0080
- Title: A script's identity comes from one file while its interpreter opens the path again, which a base switch or an activation replacement re-points
- Status: `open`
- Confidence: 15 — evidence 15, exploitability 0 (the switch takes `BaseUpdate` or `ExtensionUpdate`), reproducibility 0. Raised by: a test that deactivates an extension between a script's exec and its interpreter's open.
- Impact: across a switch a script runs the next generation's or activation's bytes, and after a deactivation whatever lies on the root under `/opt/<name>`, while `SO_PEERPROGRAM` and `SO_PEEREXTENSION` name the first; an old-generation process is judged by the current policy for its path.
- Evidence: `core/src/exec/script.rs:21-34`.
- Repro: nearest deterministic: exec an extension's script and deactivate the extension before the interpreter opens it.
- Remediation: hand the interpreter the script by descriptor (`/dev/fd/N`).

### SLOPOS-2026-0081
- Title: Every service of an extension logs and keeps state as the extension's first user
- Status: `open`
- Confidence: 15 — evidence 15, exploitability 0, reproducibility 0. Raised by: a utest in which one service user of an extension writes another's state.
- Impact: a compromised service running as one of an extension's users reads and rewrites another service's logs, state and runtime directory, undoing the separation the extension declared users for.
- Evidence: `extension-core/src/declare.rs:370-373`; `userland/src/apps/init/extensions.rs:1162`, `:1263`, `:1450`.
- Repro: an extension with two users and a service each; one writes the other's `/var/lib/<name>`.
- Remediation: a directory and logger per service user.

### SLOPOS-2026-0082
- Title: Any task lists `/dev/crash` and reads every disk's size, block size and identities
- Status: `open`
- Confidence: 15 — evidence 15, exploitability 0, reproducibility 0. Raised by: a shown use of what the listing and the size ioctls reveal.
- Impact: unprivileged code learns how many crash records the machine holds and their sizes, and each disk's geometry and labels, as Linux's `/dev/disk/by-*` tells every user.
- Evidence: `fs/src/devfs/crash.rs:101-108`; `fs/src/devfs/block.rs:776-786`.
- Repro: as uid 1000, `ls -l /dev/crash`; `BLKGETSIZE64` on `/dev/nvme0n1`.
- Remediation: list `/dev/crash` and answer the size ioctls to a `RawBlock` holder only.

### SLOPOS-2026-0083
- Title: The host runs `e2fsck -fy` and `resize2fs` over metadata the guest wrote
- Status: `open`
- Confidence: 15 — evidence 15, exploitability 0 (needs a defect in the host's e2fsprogs), reproducibility 0. Raised by: an e2fsprogs defect reachable from a crafted image.
- Impact: a guest that writes crafted metadata into its persistent root reaches any parsing defect of the host's e2fsprogs, as the developer's user, the next time a build grows the image.
- Evidence: `scripts/build_fs_image.sh:262-300`.
- Repro: none safely; nearest: fuzz a preserved image through the build's grow path.
- Remediation: grow the image in a sandbox without the developer's files, or from inside the guest.

### SLOPOS-2026-0084
- Title: A `DacOverride` holder can write a store file between its creation and its seal
- Status: `open`
- Confidence: 10 — evidence 10, exploitability 0 (a power loss must fall between the seal and the read-back), reproducibility 0. Raised by: a test hook that writes between the write and the seal and survives a cut before the read-back.
- Impact: a write in that window fails the edit, a writable mapping refuses the seal (`EBUSY`) so such a holder fails every edit it races, and a power loss between the seal and the read-back leaves its bytes sealed and believed as the newest generation.
- Evidence: `userland/src/sealed.rs:122-143`; `userland/src/apps/extctl/store.rs:243-260`.
- Repro: nearest deterministic: a test hook between `create_sealed`'s write and its seal that writes the file through a second descriptor.
- Remediation: write the generation where no other process can name it and link it in once sealed.

### SLOPOS-2026-0085
- Title: A command line run through `remote.py sh` holds the paired agent's privd connection
- Status: `open`
- Confidence: 10 — evidence 10, exploitability 0 (the operator must source a file the session can edit), reproducibility 0. Raised by: an operator workflow that sources a session-writable file through `sh`.
- Impact: session code that edits a script the operator sources through `remote.py sh` asks privd as the paired agent: kernels, bases, one-shot boots, power.
- Evidence: `scripts/remote.py:996-997`; `userland/src/apps/remoted/serve.rs:226`.
- Repro: the session writes `bootctl oneshot slopos-b` into `~/env.sh`; the operator runs `just remote sh '. ~/env.sh'`.
- Remediation: `sh` runs without the connection unless the operator asks for it.

## Cadence

1. Sweep after each major milestone and before any release or PR handoff.
2. Re-scan what recent commits touched — at minimum syscall paths, memory
   management, filesystems and drivers.

## Triage workflow (strict order)

1. **List every finding first**, unscored.
2. Score confidence 0-100: evidence quality (0-40, direct code with exact
   `path:line`), exploitability clarity (0-30, a realistic attacker path),
   reproducibility (0-30).
3. Only **confidence >= 80** is a guaranteed issue.
4. Only a guaranteed issue gets a CVSS vector, computed with
   `python3 scripts/cvss_calc.py "CVSS:3.1/..."` so agents agree.
5. A finding below 80 with an attacker-reachable trigger is still recorded, with
   no vector and a note saying what evidence would raise it. One with no
   attacker-reachable trigger belongs in `plans/`, not here.

## Non-negotiable rules

- Never present a speculative issue as a CVSS-scored vulnerability.
- **Verify the claim before fixing it.** An entry can be wrong; read the code.
- A fix lands with a test that **fails without it** — confirmed by reverting the
  fix, not by assertion.

## Entry format

```markdown
### SLOPOS-YYYY-NNNN
- Title: one line, the defect rather than the symptom
- Status: `open` or `needs-retest`
- Confidence: NN — evidence NN, exploitability NN, reproducibility NN, with reasoning
- CVSS vector/score: `CVSS:3.1/...` — **N.N SEVERITY**   (omit when confidence < 80)
- Impact: what an attacker gets, in the tree's own vocabulary
- Evidence: exact `path:line` references, one per claim
- Repro: minimal syscall sequence, malformed artifact, or PoC steps; if none is
  safely possible, say why and give the nearest deterministic validation
- Remediation: the shape of the fix, not "add a check"
```

## Scoring notes

CVSS 3.1 Base Score; `0.1-3.9 Low`, `4.0-6.9 Medium`, `7.0-8.9 High`,
`9.0-10.0 Critical`.

The session runs as uid 1000. A program it starts holds the universal
capabilities alone, whatever its path, so an unprivileged local attacker is
any code the session runs: `AV:L/PR:L`.
Init, privd and the mechanisms privd runs as uid 0 are privileged; steering
one from the session scores `PR:L` with that program's impact, and `UI:R`
where the action waits on the person's consent. A paired host is
`AV:N/PR:H` for the actions the policy allows it.

A panic in a `#![forbid(unsafe_code)]` crate is an availability impact, never a
memory-safety one. Overflow checks are on in the dev and tests kernels and off
in release, so the same arithmetic defect is a panic in one build and a silent
wrong value in another — say which when scoring.

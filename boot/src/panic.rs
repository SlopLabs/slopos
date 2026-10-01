use core::ffi::c_int;
use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use slopos_arch::cpu;
use slopos_drivers::crash::PanicRecord;
use slopos_drivers::keyboard::poll_wait_enter;
use slopos_mm::memory_init::is_memory_system_initialized;
use slopos_ostd::panic_recovery;
use slopos_ostd::stacktrace::{self, StacktraceEntry};
use slopos_ostd::sync::StateFlag;
use slopos_video::panic_screen::{self, PanicView};

use crate::shutdown::execute_kernel;

static PANIC_RIP: AtomicU64 = AtomicU64::new(0);
static PANIC_RSP: AtomicU64 = AtomicU64::new(0);
/// RBP of the *interrupted* context, set when the panic comes from an exception
/// handler. Preferred over `PANIC_ORIG_RBP` so the report shows the faulting
/// call chain rather than the panic machinery's own frames.
static PANIC_FRAME_RBP: AtomicU64 = AtomicU64::new(0);
static PANIC_HAS_CPU_STATE: StateFlag = StateFlag::new();
const PANIC_BACKTRACE_MAX: usize = 16;

/// The panicking `&PanicInfo` as a raw pointer. It stays live across the
/// emergency-stack switch, which moves only `RSP` and unwinds nothing.
static PANIC_INFO_PTR: AtomicUsize = AtomicUsize::new(0);
/// Pre-switch `RSP`: the reporter's own is the emergency stack.
static PANIC_ORIG_RSP: AtomicU64 = AtomicU64::new(0);
/// Pre-switch `RBP`, so the backtrace walks the panic origin rather than the
/// reporter's frames on the emergency stack.
static PANIC_ORIG_RBP: AtomicU64 = AtomicU64::new(0);

/// A stuck peer must never block the report, so the owner proceeds on timeout.
const PEER_STOP_SPIN_BUDGET: u64 = 50_000_000;

/// `rbp` is the interrupted context's frame pointer.
#[inline]
pub fn set_panic_cpu_state(rip: u64, rsp: u64, rbp: u64) {
    PANIC_RIP.store(rip, Ordering::SeqCst);
    PANIC_RSP.store(rsp, Ordering::SeqCst);
    PANIC_FRAME_RBP.store(rbp, Ordering::SeqCst);
    PANIC_HAS_CPU_STATE.set_active();
}

fn take_panic_cpu_state() -> (Option<u64>, Option<u64>) {
    if PANIC_HAS_CPU_STATE.take() {
        (
            Some(PANIC_RIP.load(Ordering::SeqCst)),
            Some(PANIC_RSP.load(Ordering::SeqCst)),
        )
    } else {
        (None, None)
    }
}

fn panic_serial_write(s: &str) {
    // The polling `early_console`, never the `SERIAL` spinlock: a CPU that
    // faulted holding that lock would self-deadlock the moment it panics.
    slopos_ostd::early_console::write_bytes(s.as_bytes());
    slopos_ostd::early_console::write_bytes(b"\n");
    // A full report over a polled UART outlasts a timer tick, and one emitted
    // line is real progress. The touch cannot mask a wedge: a dead UART stops
    // inside `write_bytes` above.
    slopos_ostd::watchdog::touch();
}

/// Into the kernel log too, so a later fatal panic's record carries the oops.
fn oops_line(s: &str) {
    panic_serial_write(s);
    slopos_ostd::klog::klog_ring_line(s);
}

/// Last-resort abort: prints only pre-existing `&'static str`s, never a
/// `format_args!` value.
///
/// `format_args!` materialises a `[core::fmt::Argument; N]` as an address-taken
/// local, i.e. on the SafeStack *data* stack — exactly the stack that has
/// overflowed in the one case this exists for, where the normal reporter would
/// re-fault on it. Interrupts are masked first so no IRQ perturbs the halt.
pub fn panic_abort_raw(msg: &'static str) -> ! {
    abort(msg, false)
}

/// [`panic_abort_raw`] on a CPU whose data stack is whole, as a lockup's is,
/// which also leaves a crash record.
pub fn panic_abort_recorded(msg: &'static str) -> ! {
    abort(msg, true)
}

fn abort(msg: &'static str, record: bool) -> ! {
    slopos_ostd::fblog::snapshot_tail_for_panic();
    cpu::disable_interrupts();
    // Published before the bypass below, which force-releases locks a peer
    // takes at once: Release/Acquire only, so this narrows the window in which
    // it still believes this CPU can answer rather than closing it.
    let dying_cpu = slopos_arch::get_current_cpu();
    slopos_mm::tlb::force_ack_local_shootdowns(dying_cpu);
    slopos_mm::tlb::notify_cpu_offline();
    slopos_arch::pcr::mark_cpu_offline(dying_cpu);
    slopos_sched::per_cpu::abandon_dispatch_for_dying_cpu(dying_cpu);
    slopos_ostd::panic::mark_cpu_stopped();
    slopos_ostd::panic::mark_fatal_abort();
    // Ordering validation off before anything below acquires a lock.
    slopos_ostd::sync::enter_fatal_bypass();
    // A fault in writing the record below must come back here, not be
    // recovered or lose the owner election to this CPU's own claim.
    slopos_ostd::panic::panic_in_flight_enter();
    slopos_ostd::panic::panic_depth_enter();
    // Best-effort ownership so a concurrent panic on a peer cannot interleave.
    let _ = slopos_ostd::panic::claim_panic_owner(dying_cpu as u32);
    panic_serial_write("\n\n=== KERNEL ABORT ===");
    panic_serial_write(msg);
    if slopos_ostd::panic::panic_owner_is(dying_cpu as u32) {
        if record {
            record_abort(msg);
        }
        if REBOOT_ON_PANIC.load(Ordering::Relaxed) {
            panic_serial_write("panic=reboot: resetting");
            // A triple fault takes no lock and reserves no port, either of
            // which the fault that brought this CPU here may have left held.
            slopos_ostd::cpu::x86_64::core::trigger_triple_fault();
        }
    }
    panic_serial_write("System halted.");
    cpu::halt_loop()
}

fn record_abort(msg: &'static str) {
    let Some(mut record) = slopos_drivers::crash::begin_panic_record(msg) else {
        panic_serial_write(if slopos_drivers::crash::armed().is_some() {
            "crash record: not begun, its buffer was held"
        } else {
            "crash record: none, the boot disk keeps no crash store"
        });
        return;
    };
    let _ = writeln!(record, "=== KERNEL ABORT ===\n{msg}");
    panic_serial_write(match record.commit() {
        Ok(_) => "crash record: written",
        Err(_) => "crash record: not written",
    });
}

/// Fills `out` with return addresses walked from the stashed rbp; returns the
/// frame count.
fn panic_capture_backtrace(out: &mut [u64]) -> usize {
    let frame_rbp = PANIC_FRAME_RBP.load(Ordering::SeqCst);
    let stashed = PANIC_ORIG_RBP.load(Ordering::SeqCst);
    let rbp = if frame_rbp != 0 {
        frame_rbp
    } else if stashed != 0 {
        stashed
    } else {
        cpu::read_rbp()
    };
    let mut entries: [StacktraceEntry; PANIC_BACKTRACE_MAX] = [StacktraceEntry {
        frame_pointer: 0,
        return_address: 0,
    }; PANIC_BACKTRACE_MAX];
    let captured = stacktrace::stacktrace_capture_from(
        rbp,
        entries.as_mut_ptr(),
        PANIC_BACKTRACE_MAX as c_int,
    );
    if captured <= 0 {
        return 0;
    }
    let n = (captured as usize).min(out.len());
    for (slot, entry) in out[..n].iter_mut().zip(entries.iter()) {
        *slot = entry.return_address;
    }
    n
}

#[inline(never)]
fn panic_dump_backtrace(report: &mut Report) {
    let frame_rbp = PANIC_FRAME_RBP.load(Ordering::SeqCst);
    let stashed = PANIC_ORIG_RBP.load(Ordering::SeqCst);
    let rbp = if frame_rbp != 0 {
        frame_rbp
    } else if stashed != 0 {
        stashed
    } else {
        cpu::read_rbp()
    };
    panic_dump_backtrace_from(rbp, &mut |line| report.line(line))
}

/// Every frame is printed, the panic machinery's own included: a fixed skip
/// count would rot as the call shape changes.
fn panic_dump_backtrace_from(rbp: u64, emit: &mut dyn FnMut(&str)) {
    let mut entries: [StacktraceEntry; PANIC_BACKTRACE_MAX] = [StacktraceEntry {
        frame_pointer: 0,
        return_address: 0,
    }; PANIC_BACKTRACE_MAX];

    let captured = stacktrace::stacktrace_capture_from(
        rbp,
        entries.as_mut_ptr(),
        PANIC_BACKTRACE_MAX as c_int,
    );
    if captured <= 0 {
        emit("Backtrace: <empty>");
        return;
    }

    emit("Backtrace (most recent call first):");
    for i in 0..captured as usize {
        let entry = &entries[i];
        let mut line = MessageBuffer::new();
        if let Some(sym) = slopos_ostd::ksym::lookup(entry.return_address) {
            let _ = write!(
                line,
                "  #{} rbp=0x{:016x} rip=0x{:016x} {}+0x{:x}",
                i, entry.frame_pointer, entry.return_address, sym.symbol, sym.offset
            );
        } else {
            let _ = write!(
                line,
                "  #{} rbp=0x{:016x} rip=0x{:016x}",
                i, entry.frame_pointer, entry.return_address
            );
        }
        emit(line.as_str());
    }
}

/// Called by the kernel's `#[panic_handler]`.
pub fn panic_handler_impl(info: &PanicInfo) -> ! {
    // Before anything below writes to serial: the report shares the capture
    // ring and would scroll out the lines leading up to here.
    slopos_ostd::fblog::snapshot_tail_for_panic();

    let prior_in_flight = slopos_ostd::panic::panic_in_flight_enter();

    // Recovery is task-scoped: only a first-level panic at a recovery boundary
    // outside interrupt context unwinds. Checked before interrupts are disabled
    // so the flag restores correctly.
    if prior_in_flight == 0
        && panic_recovery::recovery_is_active()
        && !slopos_ostd::panic::in_interrupt_context()
    {
        // Test-harness catches are expected control flow, so only production
        // oopses spend the recovered-panic budget.
        let production = panic_recovery::production_recovery_enabled();
        let (oops_count, limit_reached) = if production {
            panic_recovery::oops_record()
        } else {
            (0, false)
        };

        let interrupts_were_enabled = cpu::are_interrupts_enabled();
        cpu::disable_interrupts();

        if limit_reached {
            // Interrupts stay disabled: this CPU is committed to the fatal path.
            let mut buf = MessageBuffer::new();
            let _ = write!(
                buf,
                "\n[PANIC] oops limit reached ({}/{}); escalating to fatal",
                oops_count,
                panic_recovery::oops_limit()
            );
            oops_line(buf.as_str());
        } else {
            oops_line("\n[PANIC — task-scoped recovery]");

            if let Some(location) = info.location() {
                let mut buf = MessageBuffer::new();
                let _ = write!(
                    buf,
                    "  at {}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                );
                oops_line(buf.as_str());
            }

            {
                let mut msg_buf = MessageBuffer::new();
                if let Some(msg) = info.message().as_str() {
                    let _ = write!(msg_buf, "  message: {}", msg);
                } else {
                    let _ = write!(msg_buf, "  message: {}", info.message());
                }
                oops_line(msg_buf.as_str());
            }

            if production {
                let mut buf = MessageBuffer::new();
                let _ = write!(buf, "  oops count: {}", oops_count);
                oops_line(buf.as_str());
            }

            // The live rbp, because the stashed statics belong to the
            // fatal/exception path and may be stale here.
            panic_dump_backtrace_from(cpu::read_rbp(), &mut oops_line);

            // Unwinding restores Rust frames, not the interrupt flag.
            if interrupts_were_enabled {
                cpu::enable_interrupts();
            }

            match slopos_ostd::unwind::begin_panic(info) {
                Ok(never) => match never {},
                Err(code) => {
                    let mut buf = MessageBuffer::new();
                    let _ = write!(buf, "  unwind initiation failed: {}", code.0);
                    oops_line(buf.as_str());
                }
            }
        }
    }

    cpu::disable_interrupts();
    // One-way, and before the reporter runs: everything below acquires locks
    // while this CPU still holds whatever it held at the fault.
    slopos_ostd::sync::enter_fatal_bypass();

    // A non-zero prior depth means the fatal path itself faulted, so the
    // reporter is suspect; degrade to the format-free abort, which the #PF
    // guard-fault path lands on a fresh IST data stack.
    if slopos_ostd::panic::panic_depth_enter() >= 1 {
        panic_abort_raw("recursive fatal fault — emergency reporter re-entered");
    }

    // Single-owner election, first CAS wins. A losing peer self-stops so it
    // neither contends on the console nor holds a lock the owner needs.
    let cpu_id = slopos_arch::get_current_cpu() as u32;
    if !slopos_ostd::panic::claim_panic_owner(cpu_id) {
        loop {
            cpu::disable_interrupts();
            cpu::halt_loop();
        }
    }

    // The switch to the emergency stacks discards RSP-relative locals, so what
    // the reporter needs travels via statics. The frame-pointer walk works
    // because the kernel is built `-C force-frame-pointers=yes`.
    PANIC_INFO_PTR.store(info as *const PanicInfo as usize, Ordering::SeqCst);
    PANIC_ORIG_RSP.store(cpu::read_rsp(), Ordering::SeqCst);
    PANIC_ORIG_RBP.store(cpu::read_rbp(), Ordering::SeqCst);

    // NMI is the only delivery that pierces a wedged IF=0 spin, and stopping the
    // peers is also what dissolves a TLB-shootdown ack wedge.
    slopos_arch::pcr::send_nmi_broadcast();
    wait_for_peer_stop();

    // On the emergency stacks, so panic `core::fmt` has guaranteed headroom and
    // cannot recurse through a guard #PF.
    slopos_ostd::panic::run_on_emergency_stacks(emergency_report)
}

fn wait_for_peer_stop() {
    let expected = (slopos_arch::pcr::get_pcr_count() as u32).saturating_sub(1);
    if expected == 0 {
        return;
    }
    let mut spins: u64 = 0;
    while slopos_ostd::panic::stopped_cpu_count() < expected && spins < PEER_STOP_SPIN_BUDGET {
        spins = spins.wrapping_add(1);
        cpu::pause();
    }
}

struct Report {
    record: Option<PanicRecord<'static>>,
}

const BANNER: &str = "=== KERNEL PANIC ===";

impl Report {
    /// The banner and the panic's message reach serial before a record is
    /// begun, so a fault in beginning it still leaves them there.
    fn begin(message: &str) -> Report {
        panic_serial_write("\n");
        panic_serial_write(BANNER);
        panic_serial_write(message);
        let mut record = slopos_drivers::crash::begin_panic_record(message);
        if let Some(record) = &mut record {
            let _ = writeln!(record, "{BANNER}\n{message}");
        }
        Report { record }
    }

    fn line(&mut self, text: &str) {
        panic_serial_write(text);
        if let Some(record) = &mut self.record {
            let _ = writeln!(record, "{text}");
        }
    }

    /// Before anything resets or waits, so the record outlives both.
    #[inline(never)]
    fn commit(&mut self) -> MessageBuffer {
        let mut outcome = MessageBuffer::new();
        let _ = match self.record.take() {
            Some(record) => {
                let partition = record.partition();
                match record.commit() {
                    Ok(written) => write!(
                        outcome,
                        "crash record: {} written to {} slot {}",
                        written.sequence, partition, written.slot
                    ),
                    Err(e) => write!(
                        outcome,
                        "crash record: not written to {}: {:?}",
                        partition, e
                    ),
                }
            }
            None if slopos_drivers::crash::armed().is_some() => {
                outcome.write_str("crash record: not begun, its buffer was held")
            }
            None => outcome.write_str("crash record: none, the boot disk keeps no crash store"),
        };
        panic_serial_write(outcome.as_str());
        outcome
    }
}

/// How long `panic=reboot` leaves the panic on screen before it resets. Bare
/// metal may have no other record of it; a hypervisor's console is on the
/// host, as `watchdog.panic`'s default also assumes.
pub(crate) const fn reset_hold_ms(hypervisor_present: bool) -> u32 {
    if hypervisor_present { 0 } else { 10_000 }
}

/// A ten-second spin would otherwise read to the watchdog as a lockup.
fn hold_before_reset(ms: u32) {
    slopos_drivers::hpet::spin_for(ms, &mut slopos_ostd::watchdog::touch);
}

/// Recovered panics may have left non-RAII kernel state skewed, so a non-zero
/// count marks this report as post-degradation.
#[inline(never)]
fn report_taint(report: &mut Report) {
    let oopses = panic_recovery::oops_count();
    if oopses > 0 {
        let mut taint_buf = MessageBuffer::new();
        let _ = write!(taint_buf, "tainted: oops={}", oopses);
        report.line(taint_buf.as_str());
    }
}

#[inline(never)]
fn report_registers(report: &mut Report, registers: &[(&str, Option<u64>)]) {
    report.line("Register snapshot:");
    for &(label, value) in registers {
        if let Some(value) = value {
            let mut hex_buf = HexBuffer::new();
            report.line(hex_buf.format_labeled(label, value));
        }
    }
}

/// The fatal-fault report: sole console writer, peers already stopped.
/// `extern "sysv64"` + `-> !` matches the trampoline's bare-fn entry, so all
/// state arrives through statics.
extern "sysv64" fn emergency_report() -> ! {
    let (extra_rip, extra_rsp) = take_panic_cpu_state();
    let display_rsp = extra_rsp.unwrap_or_else(|| PANIC_ORIG_RSP.load(Ordering::SeqCst));
    let cr0 = cpu::read_cr0();
    let cr2 = cpu::read_cr2();
    let cr3 = cpu::read_cr3();
    let cr4 = cpu::read_cr4();

    let mut message_buf = MessageBuffer::new();
    let info_ptr = PANIC_INFO_PTR.load(Ordering::SeqCst) as *const PanicInfo;
    slopos_ostd::panic::format_panic_location_message(info_ptr, &mut message_buf);
    let message_str = message_buf.as_str();

    let mut report = Report::begin(message_str);

    report_taint(&mut report);
    report_registers(
        &mut report,
        &[
            ("RIP", extra_rip),
            ("RSP", Some(display_rsp)),
            ("CR0", Some(cr0)),
            ("CR2", Some(cr2)),
            ("CR3", Some(cr3)),
            ("CR4", Some(cr4)),
        ],
    );
    panic_dump_backtrace(&mut report);

    report.line("===================");
    report.line("Kernel panic: unrecoverable error");
    let outcome = report.commit();

    let mut bt = [0u64; 8];
    let bt_n = panic_capture_backtrace(&mut bt);
    let show_screen = |prompt| {
        panic_screen::display_panic_screen(&PanicView {
            message: message_str,
            rip: extra_rip,
            rsp: display_rsp,
            cr0,
            cr2,
            cr3,
            cr4,
            backtrace: &bt[..bt_n],
            status: outcome.as_str(),
            prompt,
        })
    };

    if REBOOT_ON_PANIC.load(Ordering::Relaxed) {
        let hold = reset_hold_ms(slopos_ostd::arch::x86_64::cpuid::hypervisor_present());
        if hold > 0 && show_screen("Resetting to the default boot entry") {
            hold_before_reset(hold);
        }
        panic_serial_write("panic=reboot: resetting");
        crate::shutdown::reset_after_panic();
    }

    #[cfg(feature = "tests")]
    {
        panic_serial_write("TEST MODE: Exiting QEMU with failure code");
        slopos_testing::tests_request_shutdown(1);
    }

    if show_screen("Press ENTER to shutdown") {
        panic_serial_write("Press ENTER to shutdown...");
        poll_wait_enter();
    } else {
        panic_serial_write("System halted.");
    }

    if is_memory_system_initialized() != 0 {
        execute_kernel();
    } else {
        panic_serial_write("Memory system unavailable; skipping paint ritual");
    }

    slopos_ostd::sync::panic_recovery::poison_all_held_locks();
}

struct MessageBuffer {
    buf: [u8; 256],
    len: usize,
}

impl MessageBuffer {
    const fn new() -> Self {
        Self {
            buf: [0u8; 256],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl Write for MessageBuffer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let available = self.buf.len() - self.len;
        let to_copy = bytes.len().min(available);
        self.buf[self.len..self.len + to_copy].copy_from_slice(&bytes[..to_copy]);
        self.len += to_copy;
        Ok(())
    }
}

struct HexBuffer {
    buf: [u8; 32],
}

impl HexBuffer {
    const fn new() -> Self {
        Self { buf: [0u8; 32] }
    }

    fn format_labeled(&mut self, label: &str, value: u64) -> &str {
        const HEX_CHARS: &[u8] = b"0123456789ABCDEF";

        let mut pos = 0;

        for &b in label.as_bytes() {
            if pos < self.buf.len() {
                self.buf[pos] = b;
                pos += 1;
            }
        }

        if pos + 4 <= self.buf.len() {
            self.buf[pos] = b':';
            self.buf[pos + 1] = b' ';
            self.buf[pos + 2] = b'0';
            self.buf[pos + 3] = b'x';
            pos += 4;
        }

        for i in 0..16 {
            if pos < self.buf.len() {
                let nibble = ((value >> (60 - i * 4)) & 0xF) as usize;
                self.buf[pos] = HEX_CHARS[nibble];
                pos += 1;
            }
        }

        core::str::from_utf8(&self.buf[..pos]).unwrap_or("")
    }
}

static REBOOT_ON_PANIC: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// `panic=reboot`.
pub fn set_reboot_on_panic() {
    REBOOT_ON_PANIC.store(true, core::sync::atomic::Ordering::Relaxed);
}

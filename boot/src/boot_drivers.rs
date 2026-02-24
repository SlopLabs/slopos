use core::ffi::CStr;
#[cfg(feature = "xe-gpu")]
use core::ffi::c_char;

use slopos_lib::klog::{self, KlogLevel};
use slopos_lib::{klog_debug, klog_info};
use slopos_tests::{
    TestRunSummary, tests_request_shutdown, tests_reset_panic_state, tests_run_all,
};
use slopos_video as video;

use crate::early_init::{boot_get_cmdline, boot_init_priority};
use crate::idt::{idt_init, idt_load};
use crate::ist_stacks::ist_stacks_init;
use crate::limine_protocol;
use crate::smp::smp_init;
#[cfg(feature = "xe-gpu")]
use slopos_drivers::xe;
use slopos_drivers::{
    apic, hpet, ioapic,
    pci::{pci_get_primary_gpu, pci_init, pci_probe_drivers},
    pic::pic_quiesce_disable,
    pit::{pit_init, pit_poll_delay_ms},
    virtio_blk::virtio_blk_register_driver,
    virtio_net::virtio_net_register_driver,
};
use slopos_mm::tlb;

const PIT_DEFAULT_FREQUENCY_HZ: u32 = 100;

fn sync_mouse_bounds(display: Option<slopos_abi::FramebufferData>) {
    let Some(display) = display else {
        return;
    };

    let width = display.info.width as i32;
    let height = display.info.height as i32;
    if width > 0 && height > 0 {
        slopos_drivers::mouse::set_bounds(width, height);
    }
}

fn serial_note(msg: &str) {
    slopos_drivers::serial::write_line(msg);
}

#[cfg(feature = "xe-gpu")]
fn cmdline_contains(cmdline: *const c_char, needle: &str) -> bool {
    if cmdline.is_null() {
        return false;
    }

    let haystack = unsafe { CStr::from_ptr(cmdline) }.to_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }

    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn boot_video_backend() -> video::VideoBackend {
    #[cfg(feature = "xe-gpu")]
    {
        let cmdline = boot_get_cmdline();
        if cmdline_contains(cmdline, "video=xe") {
            return video::VideoBackend::Xe;
        }
    }
    video::VideoBackend::Framebuffer
}

fn boot_step_idt_setup_fn() {
    klog_debug!("Initializing IDT...");
    serial_note("boot: idt setup start");
    idt_init();
    ist_stacks_init();
    idt_load();
    serial_note("boot: idt setup done");
    klog_debug!("IDT initialized and loaded.");
}

fn boot_step_irq_setup_fn() {
    klog_debug!("Configuring IRQ dispatcher...");
    slopos_drivers::irq::init();
    // Register input cleanup so exec() and task termination tear down
    // keyboard/pointer focus and event queues for the old process image.
    slopos_core::task::register_task_resource_cleanup_hook(
        slopos_drivers::input_event::input_cleanup_task,
    );
    klog_debug!("IRQ dispatcher ready.");
}

fn boot_step_timer_setup_fn() {
    klog_debug!("Initializing programmable interval timer...");
    pit_init(PIT_DEFAULT_FREQUENCY_HZ);
    klog_debug!("Programmable interval timer configured.");

    let ticks_before = slopos_core::irq::get_timer_ticks();
    pit_poll_delay_ms(100);
    let ticks_after = slopos_core::irq::get_timer_ticks();
    klog_info!(
        "BOOT: PIT ticks after 100ms poll: {} -> {}",
        ticks_before,
        ticks_after
    );
    if ticks_after == ticks_before {
        klog_info!("BOOT: WARNING - no PIT IRQs observed in 100ms window");
    }

    let boot_fb = limine_protocol::boot_info().framebuffer;
    if boot_fb.is_none() {
        klog_info!(
            "WARNING: Limine framebuffer not available (will rely on alternative graphics initialization)"
        );
    }
    let backend = boot_video_backend();
    #[cfg(feature = "xe-gpu")]
    if backend == video::VideoBackend::Xe {
        klog_info!("BOOT: deferring video init until PCI for GPU backend");
        return;
    }
    let fb = boot_fb.map(|bf| slopos_abi::FramebufferData {
        address: bf.address,
        info: bf.info,
    });
    video::init(fb, backend);
    sync_mouse_bounds(fb);
}

fn boot_step_apic_setup_fn() {
    klog_debug!("Detecting Local APIC...");
    if !apic::detect() {
        panic!("SlopOS requires a Local APIC - legacy PIC is gone");
    }

    klog_debug!("Initializing Local APIC...");
    if apic::init() != 0 {
        panic!("Local APIC initialization failed");
    }

    pic_quiesce_disable();

    tlb::register_ipi_sender(apic::send_ipi_all_excluding_self);
    tlb::init();

    klog_debug!("Local APIC initialized (legacy PIC path removed).");
}

fn boot_step_smp_setup_fn() {
    klog_debug!("Discovering CPUs and starting APs...");
    smp_init();
}

fn boot_step_ioapic_setup_fn() {
    klog_debug!("Discovering IOAPIC controllers via ACPI MADT...");
    if ioapic::init() != 0 {
        panic!("IOAPIC discovery failed - SlopOS cannot operate without it");
    }
    klog_debug!("IOAPIC: discovery complete, ready for redirection programming.");
}

fn boot_step_hpet_setup_fn() {
    klog_debug!("Discovering HPET via ACPI...");
    if hpet::init() != 0 {
        klog_info!("HPET: Not available, PIT remains the primary timer");
        return;
    }
    klog_debug!("HPET: Initialization complete, main counter running.");
}

fn boot_step_lapic_calibration_fn() {
    klog_debug!("Calibrating LAPIC timer...");
    let freq = apic::timer::calibrate();
    if freq == 0 {
        klog_info!("BOOT: LAPIC timer calibration failed — scheduler will use PIT");
    }
}

fn boot_step_pci_init_fn() {
    klog_debug!("Enumerating PCI devices...");
    virtio_blk_register_driver();
    virtio_net_register_driver();
    pci_init();
    pci_probe_drivers();
    #[cfg(feature = "xe-gpu")]
    if boot_video_backend() == video::VideoBackend::Xe {
        xe::xe_probe();
    }

    klog_debug!("PCI subsystem initialized.");
    let gpu = pci_get_primary_gpu();
    if gpu.present != 0 {
        klog_debug!(
            "PCI: Primary GPU detected (bus {}, device {}, function {})",
            gpu.device.bus,
            gpu.device.device,
            gpu.device.function
        );
        if gpu.mmio_region.is_mapped() {
            klog_debug!(
                "PCI: GPU MMIO virtual base {:#x}, size {:#x}",
                gpu.mmio_region.virt_base(),
                gpu.mmio_size
            );
        } else {
            klog_info!("PCI: WARNING GPU MMIO mapping unavailable");
        }
    } else {
        klog_debug!("PCI: No GPU-class device discovered during enumeration");
    }

    #[cfg(feature = "xe-gpu")]
    {
        let backend = boot_video_backend();
        if backend == video::VideoBackend::Xe {
            let boot_fb = limine_protocol::boot_info().framebuffer;
            let fb = boot_fb.map(|bf| slopos_abi::FramebufferData {
                address: bf.address,
                info: bf.info,
            });
            let xe_fb = xe::xe_framebuffer_init(fb);
            video::init(xe_fb, backend);
            sync_mouse_bounds(xe_fb);
        }
    }
}

use slopos_lib::testing::config_from_cmdline;

fn boot_step_interrupt_tests_fn() -> i32 {
    // Parse command line to get test config
    let cmdline = boot_get_cmdline();
    let cmdline_str = if cmdline.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(cmdline) }.to_str().ok()
    };
    let test_config = config_from_cmdline(cmdline_str);

    if !test_config.enabled {
        klog_debug!("INTERRUPT_TEST: Harness disabled");
        return 0;
    }

    klog_info!("INTERRUPT_TEST: Running orchestrated harness");

    if klog::is_enabled_level(KlogLevel::Debug) {
        klog_info!("INTERRUPT_TEST: Verbosity -> {}", test_config.verbosity);
        klog_info!("INTERRUPT_TEST: Timeout (ms) -> {}", test_config.timeout_ms);
    }

    tests_reset_panic_state();

    use crate::ffi_boundary::{__start_test_registry, __stop_test_registry};
    let registry_start: *const slopos_lib::testing::TestSuiteDesc =
        unsafe { &__start_test_registry };
    let registry_end: *const slopos_lib::testing::TestSuiteDesc = unsafe { &__stop_test_registry };

    let mut summary = TestRunSummary::default();

    let rc = tests_run_all(&test_config, &mut summary, registry_start, registry_end);

    if test_config.shutdown {
        klog_debug!("TESTS: Auto shutdown enabled after harness");
        tests_request_shutdown(summary.failed as i32);
    }

    if summary.failed > 0 {
        klog_info!("TESTS: Failures detected");
    } else {
        klog_info!("TESTS: Completed successfully");
    }

    rc
}

crate::boot_init!(
    BOOT_STEP_IDT_SETUP,
    drivers,
    b"idt\0",
    boot_step_idt_setup_fn,
    flags = boot_init_priority(30)
);
crate::boot_init!(
    BOOT_STEP_APIC_SETUP,
    drivers,
    b"apic\0",
    boot_step_apic_setup_fn,
    flags = boot_init_priority(40)
);
crate::boot_init!(
    BOOT_STEP_SMP_SETUP,
    drivers,
    b"smp\0",
    boot_step_smp_setup_fn,
    flags = boot_init_priority(45)
);
crate::boot_init!(
    BOOT_STEP_IOAPIC_SETUP,
    drivers,
    b"ioapic\0",
    boot_step_ioapic_setup_fn,
    flags = boot_init_priority(50)
);
crate::boot_init!(
    BOOT_STEP_HPET_SETUP,
    drivers,
    b"hpet\0",
    boot_step_hpet_setup_fn,
    flags = boot_init_priority(55)
);
crate::boot_init!(
    BOOT_STEP_LAPIC_CALIBRATION,
    drivers,
    b"lapic timer calibration\0",
    boot_step_lapic_calibration_fn,
    flags = boot_init_priority(57)
);
crate::boot_init!(
    BOOT_STEP_IRQ_SETUP,
    drivers,
    b"irq dispatcher\0",
    boot_step_irq_setup_fn,
    flags = boot_init_priority(60)
);
crate::boot_init!(
    BOOT_STEP_TIMER_SETUP,
    drivers,
    b"timer\0",
    boot_step_timer_setup_fn,
    flags = boot_init_priority(70)
);
crate::boot_init!(
    BOOT_STEP_PCI_INIT,
    drivers,
    b"pci\0",
    boot_step_pci_init_fn,
    flags = boot_init_priority(80)
);
crate::boot_init!(
    BOOT_STEP_INTERRUPT_TESTS,
    drivers,
    b"interrupt tests\0",
    boot_step_interrupt_tests_fn,
    fallible,
    flags = boot_init_priority(90)
);

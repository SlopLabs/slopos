use super::*;
use crate::xhci::bus::{Error, Wait};
use crate::xhci::context::ContextLayout;
use crate::xhci::controller::{self, Handoff, Health, Setup};
use crate::xhci::ext_cap::{self, Found, legacy};
use crate::xhci::memory::{list_scratchpads, write_segment_table};
use crate::xhci::regs::{Capabilities, Decline, Layout, Malformed};
use crate::xhci::ring::{
    CommandCompletion, CommandRing, Completed, EventRing, ProducerRing, RING_TRBS, SubmitError,
    Unanswered,
};
use crate::xhci::trb::{CompletionCode, Event};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

/// What a bring-up leaves the driver holding.
struct Brought {
    caps: Capabilities,
    layout: Layout,
    found: Found,
    handoff: Handoff,
    commands: CommandRing<SimPage>,
    events: EventRing<SimPage>,
    dcbaa: SimPage,
    array: Option<SimPage>,
    scratchpads: Vec<SimPage>,
    _table: SimPage,
    /// What the driver did to the event ring's page, in order.
    event_page: Rc<RefCell<Vec<PageOp>>>,
    /// What the drain after the scan found: events for changes the run
    /// raised, which only say to look at a port again.
    first_events: Vec<Event>,
}

#[derive(Debug, PartialEq)]
enum Refused {
    Malformed(Malformed),
    Declined(Decline),
    Failed(Error),
}

/// The kernel driver's probe, in its order, less what only PCI has: the
/// power state, memory decode and the interrupt.
fn bring_up(sim: &mut SimController) -> Result<Brought, Refused> {
    let caps = Capabilities::read(sim, BAR_LEN).map_err(Refused::Malformed)?;
    if let Some(decline) = caps.decline() {
        return Err(Refused::Declined(decline));
    }
    let layout = caps.layout();
    let found = ext_cap::find(sim, &layout, BAR_LEN);
    let handoff = controller::take_ownership(sim, found.legacy);
    controller::halt_and_reset(sim, &layout, controller::PROBE_RESET_MS)
        .map_err(Refused::Failed)?;
    let mem = sim.mem.clone();
    let mut dcbaa = mem.page();
    let scratchpads: Vec<SimPage> = (0..caps.scratchpads).map(|_| mem.page()).collect();
    let array = (caps.scratchpads > 0).then(|| {
        let mut array = mem.page();
        list_scratchpads(&mut dcbaa, &mut array, scratchpads.iter().map(|p| p.phys()));
        array
    });
    let commands = CommandRing::new(mem.page());
    let events_page = mem.page();
    let event_page = events_page.fences.clone();
    let mut table = mem.page();
    write_segment_table(&mut table, events_page.phys(), RING_TRBS);
    let setup = Setup {
        slots: caps.max_slots,
        dcbaa: dcbaa.phys(),
        crcr: commands.crcr(),
        segment_table: table.phys(),
        event_ring: events_page.phys(),
    };
    controller::configure(sim, &layout, &setup);
    controller::start(sim, &layout).map_err(Refused::Failed)?;
    for port in 1..=caps.max_ports {
        controller::power_port(sim, &layout, port);
        controller::acknowledge_port(sim, &layout, port);
    }
    let mut events = EventRing::new(events_page);
    let mut first_events = Vec::new();
    controller::drain(sim, &layout, &mut events, controller::DRAIN_BUDGET, |e| {
        first_events.push(e)
    });
    assert!(
        first_events
            .iter()
            .all(|e| matches!(e, Event::PortStatusChange { .. })),
        "{first_events:?}"
    );
    Ok(Brought {
        caps,
        layout,
        found,
        handoff,
        commands,
        events,
        dcbaa,
        array,
        scratchpads,
        _table: table,
        event_page,
        first_events,
    })
}

fn drain(sim: &mut SimController, b: &mut Brought) -> (Vec<Event>, Health) {
    drain_at_most(sim, b, controller::DRAIN_BUDGET)
}

fn drain_at_most(sim: &mut SimController, b: &mut Brought, budget: usize) -> (Vec<Event>, Health) {
    let mut events = Vec::new();
    let drained = controller::drain(sim, &b.layout, &mut b.events, budget, |e| events.push(e));
    (events, drained.health)
}

/// Complete every command completion `events` holds.
fn complete_commands(b: &mut Brought, events: &[Event]) {
    for &event in events {
        if let Event::CommandCompletion {
            trb,
            code,
            parameter,
            slot,
        } = event
        {
            let completion = CommandCompletion {
                code,
                parameter,
                slot,
            };
            assert_eq!(b.commands.complete(trb, completion), Completed::Command);
        }
    }
}

fn position(log: &[Op], pred: impl Fn(&Op) -> bool) -> Option<usize> {
    log.iter().position(pred)
}

#[test]
fn brings_up_a_controller_with_64_byte_contexts_and_scratchpads() {
    let mut sim = SimController::new(Config::intel());
    sim.not_ready_for(5000);
    let mut b = bring_up(&mut sim).expect("bring-up");
    assert!(sim.violations.is_empty(), "{:?}", sim.violations);
    assert!(sim.running() && sim.bus_master);
    assert_eq!(b.handoff, Handoff::Released);
    assert_eq!(b.caps.context_bytes(), 64);
    assert_eq!(ContextLayout::new(b.caps.context_64).device_bytes(), 2048);
    assert_eq!(b.caps.scratchpads, 300);
    assert_eq!(sim.dcbaap(), b.dcbaa.phys());
    assert_eq!(sim.config_reg(), 32);

    let array = b.array.as_ref().unwrap();
    assert_eq!(b.dcbaa.read32(0), array.phys() as u32);
    assert_eq!(b.dcbaa.read32(4), (array.phys() >> 32) as u32);
    let listed: BTreeSet<u64> = (0..300)
        .map(|i| u64::from(array.read32(i * 8)) | u64::from(array.read32(i * 8 + 4)) << 32)
        .collect();
    assert_eq!(listed.len(), 300);
    assert!(listed.iter().all(|&p| p % 4096 == 0 && sim.mem.is_page(p)));
    assert_eq!(listed, b.scratchpads.iter().map(|p| p.phys()).collect());
    assert_eq!(array.read32(300 * 8), 0);

    let erstba = position(
        &sim.log,
        |op| matches!(op, Op::Write64(o, _) if *o == 0x1000 + 0x20 + 0x10),
    )
    .unwrap();
    let erdp = position(
        &sim.log,
        |op| matches!(op, Op::Write64(o, _) if *o == 0x1000 + 0x20 + 0x18),
    )
    .unwrap();
    assert!(erdp < erstba, "ERSTBA, which arms the ring, goes last");

    for port in 1..=6 {
        assert!(sim.portsc(port) & PORT_POWER != 0, "port {port} powered");
        assert_eq!(
            sim.portsc(port) & PORT_CHANGES,
            0,
            "port {port} acknowledged"
        );
    }

    let ticket = b
        .commands
        .submit(
            &mut sim,
            controller::command_doorbell(&b.layout),
            Trb::no_op_command(),
        )
        .unwrap();
    assert_eq!(sim.interrupts, 1);
    let (events, health) = drain(&mut sim, &mut b);
    assert_eq!(health, Health::Running);
    complete_commands(&mut b, &events);
    let done = b.commands.take(ticket).unwrap().unwrap();
    assert_eq!(done.code, CompletionCode::SUCCESS);
    assert_eq!(b.commands.outstanding(), 0);
}

#[test]
fn declines_a_controller_without_64_bit_addressing_touching_nothing() {
    let mut sim = SimController::new(Config {
        ac64: false,
        ..Config::qemu()
    });
    assert_eq!(
        bring_up(&mut sim).err(),
        Some(Refused::Declined(Decline::No64BitAddressing))
    );
    assert!(sim.log.iter().all(|op| matches!(op, Op::Read(..))));
    assert!(sim.running() && sim.bus_master);
}

#[test]
fn declines_what_one_page_cannot_hold() {
    let mut sim = SimController::new(Config {
        scratchpads: 513,
        ..Config::intel()
    });
    assert_eq!(
        bring_up(&mut sim).err(),
        Some(Refused::Declined(Decline::TooManyScratchpads(513)))
    );
    let mut sim = SimController::new(Config {
        page_sizes: 0b10,
        ..Config::qemu()
    });
    assert_eq!(
        bring_up(&mut sim).err(),
        Some(Refused::Declined(Decline::No4KPages))
    );
    let mut sim = SimController::new(Config {
        scratchpads: 512,
        ..Config::intel()
    });
    assert!(bring_up(&mut sim).is_ok());
    assert!(sim.violations.is_empty());
}

#[test]
fn a_function_off_the_bus_is_absent() {
    let mut sim = SimController::new(Config::qemu());
    sim.gone = true;
    assert_eq!(
        bring_up(&mut sim).err(),
        Some(Refused::Malformed(Malformed::Absent))
    );
}

#[test]
fn takes_a_controller_whose_bios_never_lets_go() {
    let mut sim = SimController::new(Config {
        legacy: Some(Legacy::NeverLetsGo),
        ..Config::intel()
    });
    let b = bring_up(&mut sim).expect("bring-up");
    assert_eq!(b.handoff, Handoff::Forced);
    assert!(!sim.bios_owned() && sim.os_owned());
    assert_eq!(sim.legacy_control() & legacy::SMI_ENABLES, 0);
    assert_eq!(sim.legacy_control() & legacy::SMI_EVENTS, 0);
    let waited: u64 = sim
        .log
        .iter()
        .take_while(|op| !matches!(op, Op::Write8(o, 0) if *o == XECP + 2))
        .map(|op| {
            if let Op::Delay(us) = op {
                u64::from(*us)
            } else {
                0
            }
        })
        .sum();
    assert!(
        (1_000_000..1_100_000).contains(&waited),
        "waited {waited} us"
    );
    assert!(sim.running());
    assert!(sim.violations.is_empty(), "{:?}", sim.violations);
}

#[test]
fn bus_mastering_stays_the_firmwares_until_the_controller_halts() {
    let mut sim = SimController::new(Config::intel());
    bring_up(&mut sim).expect("bring-up");
    let log = &sim.log;
    let handed = position(log, |op| matches!(op, Op::Write32(o, _) if *o == XECP + 4)).unwrap();
    let halted = position(
        log,
        |op| matches!(op, Op::Read(o, v) if *o == OP + USBSTS && v & STS_HALTED != 0),
    )
    .unwrap();
    let off = position(log, |op| *op == Op::BusMaster(false)).unwrap();
    let reset = position(
        log,
        |op| matches!(op, Op::Write32(o, v) if *o == OP + USBCMD && v & CMD_RESET != 0),
    )
    .unwrap();
    let on = position(log, |op| *op == Op::BusMaster(true)).unwrap();
    let armed = position(
        log,
        |op| matches!(op, Op::Write64(o, _) if *o == RT + 0x20 + ERSTBA),
    )
    .unwrap();
    let run = position(
        log,
        |op| matches!(op, Op::Write32(o, v) if *o == OP + USBCMD && v & CMD_RUN != 0),
    )
    .unwrap();
    assert!(handed < halted && halted < off && off < reset && reset < on);
    assert!(
        on < armed && armed < run,
        "the segment table is read by DMA once ERSTBA is written"
    );
    assert_eq!(log[reset + 1], Op::Delay(controller::AFTER_RESET_US));
}

#[test]
fn a_controller_that_never_halts_is_never_reset() {
    let mut sim = SimController::new(Config::qemu());
    sim.stuck = Some(Stuck::Halt);
    assert_eq!(
        bring_up(&mut sim).err(),
        Some(Refused::Failed(Error::Timeout(Wait::Halt)))
    );
    assert!(
        !sim.log
            .iter()
            .any(|op| matches!(op, Op::Write32(o, v) if *o == OP + USBCMD && v & CMD_RESET != 0))
    );
    assert!(sim.violations.is_empty());
}

#[test]
fn waits_that_never_end_are_named() {
    for (stuck, wait) in [
        (Stuck::Reset, Wait::Reset),
        (Stuck::Ready, Wait::Ready),
        (Stuck::Run, Wait::Run),
    ] {
        let mut sim = SimController::new(Config::qemu());
        sim.stuck = Some(stuck);
        assert_eq!(
            bring_up(&mut sim).err(),
            Some(Refused::Failed(Error::Timeout(wait))),
            "{stuck:?}"
        );
    }
}

#[test]
fn a_host_system_error_with_a_command_outstanding_kills_the_controller() {
    let mut sim = SimController::new(Config::intel());
    let mut b = bring_up(&mut sim).expect("bring-up");
    sim.stuck = Some(Stuck::Commands);
    let doorbell = controller::command_doorbell(&b.layout);
    let ticket = b
        .commands
        .submit(&mut sim, doorbell, Trb::no_op_command())
        .unwrap();
    assert_eq!(b.commands.outstanding(), 1);
    sim.host_system_error();

    let (events, health) = drain(&mut sim, &mut b);
    assert!(events.is_empty());
    assert_eq!(health, Health::HostSystemError);
    b.commands.fail_all();
    assert_eq!(b.commands.take(ticket), Some(Err(Unanswered::Dead)));
    assert_eq!(b.commands.outstanding(), 0);
    assert_eq!(
        b.commands.submit(&mut sim, doorbell, Trb::no_op_command()),
        Err(SubmitError::Dead)
    );
    assert_eq!(controller::quiesce(&mut sim, &b.layout), Ok(()));
    assert!(sim.halted() && !sim.bus_master);
    assert!(sim.violations.is_empty(), "{:?}", sim.violations);
}

#[test]
fn a_controller_that_left_the_bus_reads_absent() {
    let mut sim = SimController::new(Config::qemu());
    let mut b = bring_up(&mut sim).expect("bring-up");
    sim.gone = true;
    assert_eq!(drain(&mut sim, &mut b).1, Health::Absent);
    assert_eq!(controller::quiesce(&mut sim, &b.layout), Err(Error::Absent));
    assert!(!sim.bus_master);
    assert_eq!(controller::acknowledge_port(&mut sim, &b.layout, 1), None);
}

#[test]
fn shutdown_halts_resets_and_leaves_the_bus() {
    let mut sim = SimController::new(Config::intel());
    let b = bring_up(&mut sim).expect("bring-up");
    controller::halt_and_reset(&mut sim, &b.layout, controller::SHUTDOWN_RESET_MS).unwrap();
    assert!(sim.halted() && !sim.bus_master);
    assert_eq!(sim.dcbaap(), 0);
    assert!(sim.violations.is_empty(), "{:?}", sim.violations);
}

#[test]
fn ports_present_before_the_run_are_found_by_the_scan_and_raise_no_event() {
    let mut sim = SimController::new(Config::qemu());
    sim.attach(1, 4);
    sim.attach(5, 3);
    let b = bring_up(&mut sim).expect("bring-up");
    assert_eq!(b.first_events, []);
    assert!(sim.portsc(1) & PORT_CONNECTED != 0 && sim.portsc(1) & PORT_ENABLED != 0);
    assert!(sim.portsc(5) & PORT_CONNECTED != 0);
    assert!((1..=6).all(|p| sim.portsc(p) & PORT_CHANGES == 0));
    let protocols = &b.found.protocols;
    assert_eq!(protocols.of_port(1).unwrap().revision(), (3, 0));
    assert_eq!(protocols.speed(1, 4).unwrap().name, Some("SuperSpeed"));
    assert_eq!(protocols.speed(5, 3).unwrap().name, Some("high speed"));
}

#[test]
fn each_change_raises_an_event_and_the_ack_leaves_the_port_enabled() {
    let mut sim = SimController::new(Config::qemu());
    let mut b = bring_up(&mut sim).expect("bring-up");
    sim.attach(2, 4);
    sim.detach(2);
    sim.attach(4, 3);
    let (events, _) = drain(&mut sim, &mut b);
    assert_eq!(
        events,
        [
            Event::PortStatusChange { port: 2 },
            Event::PortStatusChange { port: 4 }
        ]
    );
    controller::acknowledge_port(&mut sim, &b.layout, 2).unwrap();
    controller::acknowledge_port(&mut sim, &b.layout, 4).unwrap();
    sim.attach(2, 4);
    sim.detach(2);
    assert_eq!(
        drain(&mut sim, &mut b).0,
        [Event::PortStatusChange { port: 2 }],
        "a change while one is pending raises nothing"
    );
    controller::acknowledge_port(&mut sim, &b.layout, 2).unwrap();
    sim.attach(2, 4);
    let (events, _) = drain(&mut sim, &mut b);
    assert_eq!(events, [Event::PortStatusChange { port: 2 }]);
    let status = controller::acknowledge_port(&mut sim, &b.layout, 2)
        .unwrap()
        .status;
    assert!(status.connected() && status.connect_changed() && status.enabled());
    assert!(sim.portsc(2) & PORT_ENABLED != 0);
    assert_eq!(sim.portsc(2) & PORT_CHANGES, 0);
    assert!(sim.violations.is_empty(), "{:?}", sim.violations);
}

#[test]
fn a_port_with_a_change_pending_raises_nothing_more_until_it_is_acknowledged() {
    let mut sim = SimController::new(Config::intel());
    let mut b = bring_up(&mut sim).expect("bring-up");
    sim.attach(1, 3);
    sim.detach(1);
    sim.attach(1, 3);
    assert_eq!(
        drain(&mut sim, &mut b).0,
        [Event::PortStatusChange { port: 1 }]
    );
    controller::acknowledge_port(&mut sim, &b.layout, 1).unwrap();
    sim.detach(1);
    assert_eq!(
        drain(&mut sim, &mut b).0,
        [Event::PortStatusChange { port: 1 }]
    );
}

#[test]
fn a_run_raises_events_for_changes_the_halt_held() {
    let mut sim = SimController::new(Config {
        port_power_control: false,
        ..Config::intel()
    });
    sim.attach(2, 3);
    let caps = Capabilities::read(&mut sim, BAR_LEN).unwrap();
    let layout = caps.layout();
    let b = bring_up(&mut sim).expect("bring-up");
    assert_eq!(b.first_events, [Event::PortStatusChange { port: 2 }]);
    assert_eq!(b.layout, layout);
}

#[test]
fn switched_port_power_is_turned_on_and_a_device_then_connects() {
    let mut sim = SimController::new(Config::intel());
    sim.attach(3, 1);
    let mut b = bring_up(&mut sim).expect("bring-up");
    assert!(sim.portsc(3) & PORT_POWER != 0);
    assert!(
        sim.portsc(3) & PORT_CONNECTED != 0,
        "the device connects once powered"
    );
    assert!(sim.portsc(4) & PORT_CONNECTED == 0);
    sim.detach(3);
    assert_eq!(
        drain(&mut sim, &mut b).0,
        [Event::PortStatusChange { port: 3 }]
    );
    assert!(sim.violations.is_empty(), "{:?}", sim.violations);
}

#[test]
fn a_change_between_the_read_and_the_clear_is_not_lost() {
    let mut sim = SimController::new(Config::qemu());
    let mut b = bring_up(&mut sim).expect("bring-up");
    sim.attach(5, 3);
    sim.race = Some((5, None));
    let status = controller::acknowledge_port(&mut sim, &b.layout, 5)
        .unwrap()
        .status;
    assert!(
        !status.connected(),
        "the read after the clear sees the device gone"
    );
    assert!(status.connect_changed());
    assert_eq!(sim.portsc(5) & PORT_CHANGES, 0);
    drain(&mut sim, &mut b);
    sim.race = Some((5, Some(3)));
    let status = controller::acknowledge_port(&mut sim, &b.layout, 5)
        .unwrap()
        .status;
    assert!(!status.connected() && !status.connect_changed());
    assert_eq!(
        drain(&mut sim, &mut b).0,
        [Event::PortStatusChange { port: 5 }],
        "a change after a read that found none raises its own event"
    );
}

#[test]
fn a_drain_takes_a_full_ring_at_once() {
    let mut sim = SimController::new(Config::qemu());
    let mut b = bring_up(&mut sim).expect("bring-up");
    sim.post_idle_events(200);
    assert_eq!(drain(&mut sim, &mut b).0.len(), 200);
    sim.post_idle_events(255);
    assert_eq!(drain(&mut sim, &mut b).0.len(), 255);
    assert_eq!(sim.dropped_events, 0);
    assert!(sim.violations.is_empty(), "{:?}", sim.violations);
}

#[test]
fn a_drain_cut_short_by_its_budget_is_interrupted_for_the_rest() {
    let mut sim = SimController::new(Config::qemu());
    let mut b = bring_up(&mut sim).expect("bring-up");
    sim.post_idle_events(200);
    let before = sim.interrupts;
    assert_eq!(drain_at_most(&mut sim, &mut b, 64).0.len(), 64);
    assert_eq!(sim.interrupts, before + 1, "ERDP short of the enqueue");
    assert_eq!(drain(&mut sim, &mut b).0.len(), 136);
    assert_eq!(sim.interrupts, before + 1);
    assert!(sim.violations.is_empty(), "{:?}", sim.violations);
}

#[test]
fn a_port_that_keeps_changing_is_left_unsettled() {
    let mut sim = SimController::new(Config::intel());
    let mut b = bring_up(&mut sim).expect("bring-up");
    sim.attach(1, 3);
    drain(&mut sim, &mut b);
    sim.flapping = Some(1);
    let read = controller::acknowledge_port(&mut sim, &b.layout, 1).unwrap();
    assert!(read.unsettled && read.status.connect_changed());
    assert_ne!(sim.portsc(1) & PORT_CHANGES, 0);
    sim.flapping = None;
    let read = controller::acknowledge_port(&mut sim, &b.layout, 1).unwrap();
    assert!(!read.unsettled);
    assert_eq!(sim.portsc(1) & PORT_CHANGES, 0);
}

#[test]
fn a_completion_that_overtakes_another_marks_it_lost() {
    let mut sim = SimController::new(Config::qemu());
    let mut b = bring_up(&mut sim).expect("bring-up");
    sim.stuck = Some(Stuck::Commands);
    let doorbell = controller::command_doorbell(&b.layout);
    let first = b
        .commands
        .submit(&mut sim, doorbell, Trb::no_op_command())
        .unwrap();
    let abandoned = b
        .commands
        .submit(&mut sim, doorbell, Trb::no_op_command())
        .unwrap();
    let third = b
        .commands
        .submit(&mut sim, doorbell, Trb::no_op_command())
        .unwrap();
    b.commands.abandon(abandoned);
    let base = b.commands.crcr() & !0x3f;
    let ok = CommandCompletion {
        code: CompletionCode::SUCCESS,
        parameter: 0,
        slot: 0,
    };
    assert_eq!(b.commands.complete(base + 32, ok), Completed::Command);
    assert_eq!(b.commands.take(first), Some(Err(Unanswered::Lost)));
    assert!(b.commands.take(third).unwrap().is_ok());
    assert_eq!(b.commands.outstanding(), 0);
}

#[test]
fn commands_and_events_lap_their_rings() {
    let mut sim = SimController::new(Config::qemu());
    let mut b = bring_up(&mut sim).expect("bring-up");
    let doorbell = controller::command_doorbell(&b.layout);
    for n in 0..700 {
        let ticket = b
            .commands
            .submit(&mut sim, doorbell, Trb::no_op_command())
            .unwrap();
        let (events, health) = drain(&mut sim, &mut b);
        assert_eq!((events.len(), health), (1, Health::Running), "command {n}");
        complete_commands(&mut b, &events);
        assert!(b.commands.take(ticket).unwrap().is_ok());
    }
    assert_eq!(sim.dropped_events, 0);
    assert!(sim.violations.is_empty());
}

#[test]
fn a_full_command_table_is_busy_until_a_completion_frees_it() {
    let mut sim = SimController::new(Config::qemu());
    let mut b = bring_up(&mut sim).expect("bring-up");
    sim.stuck = Some(Stuck::Commands);
    let doorbell = controller::command_doorbell(&b.layout);
    let tickets: Vec<_> = (0..crate::xhci::ring::MAX_COMMANDS)
        .map(|_| {
            b.commands
                .submit(&mut sim, doorbell, Trb::no_op_command())
                .unwrap()
        })
        .collect();
    assert_eq!(
        b.commands.submit(&mut sim, doorbell, Trb::no_op_command()),
        Err(SubmitError::Busy)
    );
    b.commands.abandon(tickets[0]);
    sim.stuck = None;
    sim.write32(DB, 0);
    let (events, _) = drain(&mut sim, &mut b);
    assert_eq!(events.len(), tickets.len());
    complete_commands(&mut b, &events);
    assert_eq!(b.commands.take(tickets[0]), None);
    assert!(tickets[1..].iter().all(|&t| b.commands.take(t).is_some()));
    assert_eq!(b.commands.outstanding(), 0);
}

#[test]
fn a_completion_naming_no_command_is_stray() {
    let mut sim = SimController::new(Config::qemu());
    let mut b = bring_up(&mut sim).expect("bring-up");
    let ok = CommandCompletion {
        code: CompletionCode::SUCCESS,
        parameter: 0,
        slot: 0,
    };
    let base = b.commands.crcr() & !0x3f;
    assert_eq!(b.commands.complete(base, ok), Completed::Stray);
    assert_eq!(b.commands.complete(base + 8, ok), Completed::Stray);
    assert_eq!(b.commands.complete(base + 255 * 16, ok), Completed::Stray);
    assert_eq!(b.commands.complete(0, ok), Completed::Stray);
    let stopped = CommandCompletion {
        code: CompletionCode::COMMAND_RING_STOPPED,
        ..ok
    };
    let doorbell = controller::command_doorbell(&b.layout);
    sim.stuck = Some(Stuck::Commands);
    let ticket = b
        .commands
        .submit(&mut sim, doorbell, Trb::no_op_command())
        .unwrap();
    assert_eq!(b.commands.complete(base, stopped), Completed::Stray);
    assert_eq!(b.commands.outstanding(), 1);
    assert_eq!(b.commands.complete(base, ok), Completed::Command);
    assert!(b.commands.take(ticket).is_some());
}

#[test]
fn a_producer_ring_holds_one_short_of_a_lap() {
    let mem = Memory::default();
    let mut ring = ProducerRing::new(mem.page());
    let mut last = 0;
    for _ in 0..RING_TRBS - 2 {
        last = ring.push(Trb::no_op_command()).unwrap();
    }
    assert!(ring.is_full());
    assert_eq!(ring.push(Trb::no_op_command()), None);
    let index = ring.index_of(last).unwrap();
    assert!(!ring.retire_through(index + 1));
    assert!(ring.retire_through(index));
    assert!(ring.is_empty());
    for _ in 0..3 {
        ring.push(Trb::no_op_command()).unwrap();
    }
    assert_eq!(ring.in_flight(), 3);
    assert!(
        !ring.cycle(),
        "the link TRB was handed over and the cycle flipped"
    );
}

#[test]
fn a_trb_reaches_its_consumer_with_the_cycle_bit_written_last() {
    let mem = Memory::default();
    let page = mem.page();
    let fences = page.fences.clone();
    let mut ring = ProducerRing::new(page);
    fences.borrow_mut().clear();
    ring.push(Trb::no_op_command()).unwrap();
    assert_eq!(
        *fences.borrow(),
        [
            PageOp::Write(0),
            PageOp::Write(8),
            PageOp::Release,
            PageOp::Write(12)
        ]
    );
}

#[test]
fn an_event_is_read_only_after_its_cycle_bit() {
    let mut sim = SimController::new(Config::qemu());
    let mut b = bring_up(&mut sim).expect("bring-up");
    b.event_page.borrow_mut().clear();
    assert_eq!(b.events.pop(), None);
    assert_eq!(*b.event_page.borrow(), [PageOp::Read(12)]);
    sim.attach(1, 4);
    b.event_page.borrow_mut().clear();
    assert!(b.events.pop().is_some());
    assert_eq!(
        *b.event_page.borrow(),
        [
            PageOp::Read(12),
            PageOp::Acquire,
            PageOp::Read(0),
            PageOp::Read(4),
            PageOp::Read(8)
        ]
    );
    assert_eq!(b.events.pop(), None);
}

/// Registers a test names, and zero elsewhere.
struct Fixed(&'static [(usize, u32)]);

impl RegisterBus for Fixed {
    fn read32(&mut self, offset: usize) -> u32 {
        self.0
            .iter()
            .find(|&&(at, _)| at == offset)
            .map_or(0, |&(_, value)| value)
    }
    fn write8(&mut self, _: usize, _: u8) {}
    fn write32(&mut self, _: usize, _: u32) {}
    fn write64(&mut self, _: usize, _: u64) {}
    fn bus_master(&mut self, _: bool) {}
    fn delay_us(&mut self, _: u32) {}
}

#[test]
fn a_register_file_that_misplaces_the_operational_set_is_malformed() {
    let regs = &[
        (0, 0x0100_0024),
        (4, 0x0800_1040),
        (0x10, 1),
        (0x14, 0x2000),
        (0x18, 0x1000),
    ];
    assert_eq!(
        Capabilities::read(&mut Fixed(regs), 0x4000),
        Err(Malformed::Misaligned)
    );
    assert_eq!(
        Capabilities::read(&mut Fixed(regs), 0x1c),
        Err(Malformed::OutOfBar)
    );
}

/// A register file of noise: every read is a deterministic hash of its
/// offset, and a read past BAR0 fails the test.
struct Noise {
    seed: u64,
    bar_len: usize,
}

impl Noise {
    fn value(&self, offset: usize) -> u32 {
        let mut x = self.seed ^ (offset as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 29;
        x as u32
    }
}

impl RegisterBus for Noise {
    fn read32(&mut self, offset: usize) -> u32 {
        assert!(offset + 4 <= self.bar_len, "read at {offset:#x} past BAR0");
        assert!(offset.is_multiple_of(4), "misaligned read at {offset:#x}");
        self.value(offset)
    }
    fn write8(&mut self, _: usize, _: u8) {}
    fn write32(&mut self, _: usize, _: u32) {}
    fn write64(&mut self, _: usize, _: u64) {}
    fn bus_master(&mut self, _: bool) {}
    fn delay_us(&mut self, _: u32) {}
}

/// QEMU's register file with bits of some registers flipped by a hash of the
/// seed and the offset.
struct Mutated {
    sim: SimController,
    noise: Noise,
}

impl RegisterBus for Mutated {
    fn read32(&mut self, offset: usize) -> u32 {
        assert!(
            offset + 4 <= self.noise.bar_len,
            "read at {offset:#x} past BAR0"
        );
        let value = self.sim.read32(offset);
        let hash = self.noise.value(offset);
        if !hash.is_multiple_of(3) {
            return value;
        }
        let first = 1 << ((hash >> 8) % 32);
        let second = ((hash >> 16) & 1) << ((hash >> 24) % 32);
        value ^ first ^ second
    }
    fn write8(&mut self, _: usize, _: u8) {}
    fn write32(&mut self, _: usize, _: u32) {}
    fn write64(&mut self, _: usize, _: u64) {}
    fn bus_master(&mut self, _: bool) {}
    fn delay_us(&mut self, _: u32) {}
}

#[test]
fn noise_in_the_registers_reads_nothing_past_bar0() {
    for seed in 0..20_000u64 {
        let bar_len = 0x1000 << (seed % 4);
        let mut bus = Noise { seed, bar_len };
        if let Ok(caps) = Capabilities::read(&mut bus, bar_len) {
            ext_cap::find(&mut bus, &caps.layout(), bar_len);
        }
    }
}

#[test]
fn mutated_registers_read_nothing_past_bar0() {
    let mut described = 0;
    for seed in 0..20_000u64 {
        let bar_len = BAR_LEN >> (seed % 2);
        let mut bus = Mutated {
            sim: SimController::new(Config {
                legacy: Some(Legacy::NeverLetsGo),
                ..Config::qemu()
            }),
            noise: Noise { seed, bar_len },
        };
        let Ok(caps) = Capabilities::read(&mut bus, bar_len) else {
            continue;
        };
        described += 1;
        let found = ext_cap::find(&mut bus, &caps.layout(), bar_len);
        for protocol in found.protocols.iter() {
            assert!(protocol.first_port >= 1 && protocol.last_port() <= caps.max_ports);
            for psiv in 0..16 {
                let _ = found.protocols.speed(protocol.first_port, psiv);
            }
        }
        for port in 1..=caps.max_ports {
            assert!(caps.layout().port(port) + 16 <= bar_len);
        }
    }
    assert!(described > 0);
}

#[test]
fn an_extended_capability_list_of_noise_ends() {
    for seed in 0..5_000u64 {
        let mut bus = Noise {
            seed,
            bar_len: 0x10000,
        };
        let caps = Capabilities {
            cap_length: 0x40,
            version: 0x100,
            max_slots: 8,
            max_interrupters: 1,
            max_ports: (seed % 255) as u8 + 1,
            scratchpads: 0,
            ac64: true,
            context_64: false,
            xecp: (seed % 0x3fff) as u16,
            dboff: 0x2000,
            rtsoff: 0x1000,
            page_sizes: 1,
        };
        let mut seen = 0;
        ext_cap::walk(&mut bus, &caps.layout(), 0x10000, |_, _| seen += 1);
        assert!(seen <= 64);
        let found = ext_cap::find(&mut bus, &caps.layout(), 0x10000);
        let mut claimed = BTreeSet::new();
        for p in found.protocols.iter() {
            for port in p.first_port..=p.last_port() {
                assert!(claimed.insert(port), "port {port} in two protocols");
            }
        }
    }
}

#[test]
fn events_of_noise_decode_without_panicking() {
    let noise = Noise {
        seed: 7,
        bar_len: usize::MAX,
    };
    for i in 0..50_000usize {
        let trb = Trb {
            parameter: u64::from(noise.value(4 * i)) | u64::from(noise.value(4 * i + 1)) << 32,
            status: noise.value(4 * i + 2),
            control: noise.value(4 * i + 3),
        };
        let _ = Event::decode(trb);
    }
}

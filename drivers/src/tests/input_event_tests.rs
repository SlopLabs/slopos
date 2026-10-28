//! Queue-slot allocation for the input sink's queues.
//!
//! Task ids are monotonic and never recycled, so the slot table must be keyed
//! by slot rather than by task id: a table indexed by task id carries a ceiling
//! that a long-lived boot walks past, silently dropping input for every process
//! created afterwards. These tests drive ids far above any a test boot reaches.

use slopos_abi::input::keycode::KEY_A;
use slopos_abi::input::{InputEventType, MAX_INPUT_TASKS};
use slopos_testing::{TestResult, fail, pass};

use crate::input_event::{
    input_cleanup_task, input_compositor_task_id, input_poll, input_register_compositor,
    input_route_key,
};

/// Comfortably past any id a test boot allocates.
const HIGH_TASK_ID: u32 = 1_000_003;

fn route_a(timestamp_ms: u64) {
    input_route_key(KEY_A, 'a' as u32, 0, 0, true, timestamp_ms);
}

pub fn test_input_queue_serves_high_task_id() -> TestResult {
    let restore = input_compositor_task_id();
    input_cleanup_task(HIGH_TASK_ID);

    input_register_compositor(HIGH_TASK_ID);
    route_a(42);
    let event = input_poll(HIGH_TASK_ID);
    let drained = input_poll(HIGH_TASK_ID);
    input_cleanup_task(HIGH_TASK_ID);
    input_register_compositor(restore);

    let event = match event {
        Some(e) => e,
        None => return fail!("no queue served task id {}", HIGH_TASK_ID),
    };
    if event.event_type != InputEventType::KeyPress || event.key_keycode() != KEY_A {
        return fail!("read back {:?}, want the key routed", event.event_type);
    }
    if event.timestamp_ms != 42 {
        return fail!("read back timestamp {}, want 42", event.timestamp_ms);
    }
    if drained.is_some() {
        return fail!("queue still holds events after being drained");
    }
    pass!()
}

/// Cleanup returns the slot to the pool, so a run of distinct task ids never
/// exhausts the [`MAX_INPUT_TASKS`] queues.
pub fn test_input_slot_is_reusable_across_high_task_ids() -> TestResult {
    let restore = input_compositor_task_id();
    let rounds = (MAX_INPUT_TASKS * 2) as u32;
    for round in 0..rounds {
        let task_id = HIGH_TASK_ID + 1 + round;
        input_cleanup_task(task_id);

        input_register_compositor(task_id);
        route_a(u64::from(round));
        let event = input_poll(task_id);
        input_cleanup_task(task_id);

        match event {
            Some(e) if e.timestamp_ms == u64::from(round) => {}
            Some(e) => {
                input_register_compositor(restore);
                return fail!(
                    "round {}: read back timestamp {}, want {}",
                    round,
                    e.timestamp_ms,
                    round
                );
            }
            None => {
                input_register_compositor(restore);
                return fail!(
                    "round {}: no queue could be claimed for task id {}",
                    round,
                    task_id
                );
            }
        }
    }
    input_register_compositor(restore);
    pass!()
}

pub fn test_input_slot_release_drops_queued_events() -> TestResult {
    let restore = input_compositor_task_id();
    let first = HIGH_TASK_ID + 200;
    let second = HIGH_TASK_ID + 201;
    input_cleanup_task(first);
    input_cleanup_task(second);

    input_register_compositor(first);
    route_a(7);
    // Release the slot with the event still queued.
    input_cleanup_task(first);

    input_register_compositor(second);
    route_a(8);
    let event = input_poll(second);
    let rest = input_poll(second);
    input_cleanup_task(second);
    input_register_compositor(restore);

    if rest.is_some() {
        return fail!("re-claimed queue holds more than the one event routed to it");
    }
    match event {
        Some(e) if e.timestamp_ms == 8 => pass!(),
        Some(e) => fail!(
            "re-claimed queue yielded the previous owner's event (timestamp {})",
            e.timestamp_ms
        ),
        None => fail!("re-claimed queue yielded no event"),
    }
}

/// A sink left naming a dead task swallows every key and pointer event for the
/// rest of the boot.
pub fn test_input_sink_is_released_on_exit() -> TestResult {
    let first = HIGH_TASK_ID + 300;
    let second = HIGH_TASK_ID + 301;
    let restore = input_compositor_task_id();

    input_register_compositor(first);
    if input_compositor_task_id() != first {
        input_cleanup_task(first);
        input_register_compositor(restore);
        return fail!("sink did not accept task {}", first);
    }

    input_cleanup_task(first);
    let after_exit = input_compositor_task_id();

    input_register_compositor(second);
    let after_reclaim = input_compositor_task_id();
    input_cleanup_task(second);
    input_register_compositor(restore);

    if after_exit != 0 {
        return fail!("sink still names {} after its holder exited", after_exit);
    }
    if after_reclaim != second {
        return fail!(
            "second task could not claim the sink (got {})",
            after_reclaim
        );
    }
    pass!()
}

pub fn test_input_sink_survives_an_unrelated_exit() -> TestResult {
    let holder = HIGH_TASK_ID + 310;
    let other = HIGH_TASK_ID + 311;
    let restore = input_compositor_task_id();

    input_register_compositor(holder);
    input_cleanup_task(other);
    let still_held = input_compositor_task_id();

    input_cleanup_task(holder);
    input_register_compositor(restore);

    if still_held != holder {
        return fail!(
            "sink lost its holder to an unrelated exit (now {})",
            still_held
        );
    }
    pass!()
}

slopos_testing::stest!(
    name = test_input_queue_serves_high_task_id,
    suite = input_event
);
slopos_testing::stest!(
    name = test_input_sink_is_released_on_exit,
    suite = input_event
);
slopos_testing::stest!(
    name = test_input_sink_survives_an_unrelated_exit,
    suite = input_event
);
slopos_testing::stest!(
    name = test_input_slot_is_reusable_across_high_task_ids,
    suite = input_event
);
slopos_testing::stest!(
    name = test_input_slot_release_drops_queued_events,
    suite = input_event
);

/// The IRQ-driven routing path never claims a queue slot.
///
/// Routing runs in the PS/2 IRQ handler and the xHCI drain, where there is no
/// principal to charge for a slot and no errno path to refuse on: routing
/// looks up, and only a syscall or a registration claims.
pub fn test_input_routing_never_claims_a_slot() -> TestResult {
    use crate::input_event::input_hold_sink_unqueued;

    const ORPHAN: u32 = 1_000_017;

    let restore = input_compositor_task_id();
    input_cleanup_task(ORPHAN);
    input_hold_sink_unqueued(ORPHAN);

    route_a(0);

    // A claim would have created a queue and delivered the key into it.
    let claimed = input_poll(ORPHAN).is_some();
    input_cleanup_task(ORPHAN);
    input_register_compositor(restore);

    if claimed {
        return fail!("the routing path claimed a queue slot for a task that never asked");
    }
    pass!()
}

/// The other half of the rule: if nothing claimed at registration, the routing
/// path's refusal to claim would lose every event.
pub fn test_input_registration_claims_the_slot_up_front() -> TestResult {
    const HOLDER: u32 = 1_000_019;

    let restore = input_compositor_task_id();
    input_cleanup_task(HOLDER);
    input_register_compositor(HOLDER);

    route_a(0);

    let delivered = input_poll(HOLDER).is_some();
    input_cleanup_task(HOLDER);
    input_register_compositor(restore);

    if !delivered {
        return fail!("registration must reserve the queue, or routing loses every event");
    }
    pass!()
}

slopos_testing::stest!(
    name = test_input_routing_never_claims_a_slot,
    suite = input_event
);
slopos_testing::stest!(
    name = test_input_registration_claims_the_slot_up_front,
    suite = input_event
);

/// A button reaches the sink's holder alone, and goes nowhere while nothing
/// holds the sink: not to a compositor that held it before.
pub fn test_pointer_events_reach_only_the_sink_holder() -> TestResult {
    use crate::input_event::{PointerSource, pointer_buttons};

    const HOLDER: u32 = 1_000_023;
    const BUTTON: u8 = 1 << 0;

    let restore = input_compositor_task_id();
    input_cleanup_task(HOLDER);

    input_register_compositor(HOLDER);
    pointer_buttons(PointerSource::PS2, BUTTON, 5);
    pointer_buttons(PointerSource::PS2, 0, 6);
    let press = input_poll(HOLDER);
    let release = input_poll(HOLDER);

    input_register_compositor(0);
    pointer_buttons(PointerSource::PS2, BUTTON, 7);
    pointer_buttons(PointerSource::PS2, 0, 8);
    let unheld = input_poll(HOLDER);

    input_cleanup_task(HOLDER);
    input_register_compositor(restore);

    match press {
        Some(e)
            if e.event_type == InputEventType::PointerButtonPress
                && e.pointer_button_code() == BUTTON => {}
        _ => return fail!("the holder did not get the button's press"),
    }
    match release {
        Some(e)
            if e.event_type == InputEventType::PointerButtonRelease
                && e.pointer_button_code() == BUTTON => {}
        _ => return fail!("the holder did not get the button's release"),
    }
    if unheld.is_some() {
        return fail!("a button pressed while nothing held the sink reached a queue");
    }
    pass!()
}

slopos_testing::stest!(
    name = test_pointer_events_reach_only_the_sink_holder,
    suite = input_event
);

/// Each session's clipboard is its own, a process of no session has none,
/// and past the clipboards kept the one copied into longest ago goes.
pub fn test_each_session_keeps_its_own_clipboard() -> TestResult {
    use crate::input_event::{clipboard_copy, clipboard_paste};
    use slopos_abi::task::SESSION_LOGINS;

    let first = SESSION_LOGINS.end - 64;
    let second = first + 1;
    let pasted = |login: u32| {
        let mut buf = [0u8; 16];
        let n = clipboard_paste(login, &mut buf);
        (buf, n)
    };
    clipboard_copy(first, b"first");
    clipboard_copy(second, b"second");
    if pasted(first).0[..5] != *b"first" || pasted(second).0[..6] != *b"second" {
        return fail!("a session pasted what another copied");
    }
    if clipboard_copy(0, b"nobody") != 0 || pasted(0).1 != 0 || pasted(2).1 != 0 {
        return fail!("a process of no session reached a clipboard");
    }
    for login in second + 1..second + 9 {
        clipboard_copy(login, b"later");
    }
    if pasted(first).1 != 0 {
        return fail!("the clipboard copied into longest ago stayed past the ones kept");
    }
    pass!()
}

slopos_testing::stest!(
    name = test_each_session_keeps_its_own_clipboard,
    suite = input_event
);

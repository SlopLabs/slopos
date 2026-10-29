//! Sysmon selection identity — a selected row must name a *task*, not a
//! position in a list that re-sorts under it.

use slopos_userland as _;

use slopos_abi::syscall::types::UserTaskEntry;
use slopos_userland::apps::sysmon::selection::{TaskKey, index_of, key_at_row, row_of};

fn task(pid: u32, started_ms: u64) -> UserTaskEntry {
    UserTaskEntry {
        task_id: pid,
        creation_time_ms: started_ms,
        ..UserTaskEntry::default()
    }
}

fn test_selection_follows_task_across_resort() -> bool {
    let tasks = [task(10, 1), task(20, 2), task(30, 3)];
    let key = match key_at_row(&tasks, &[0, 1, 2], 0) {
        Some(k) => k,
        None => return false,
    };
    key.pid == 10 && row_of(key, &tasks, &[2, 1, 0]) == Some(2)
}

fn test_selection_stable_when_order_unchanged() -> bool {
    let tasks = [task(10, 1), task(20, 2), task(30, 3)];
    let order = [0, 1, 2];
    match key_at_row(&tasks, &order, 1) {
        Some(key) => row_of(key, &tasks, &order) == Some(1),
        None => false,
    }
}

/// Ids recycle; a key names one instantiation, so a kill cannot land on a stranger.
fn test_recycled_pid_is_not_the_same_task() -> bool {
    let before = [task(13, 100)];
    let after = [task(13, 900)];
    let key = TaskKey::of(&before[0]);
    key.matches(&before[0])
        && !key.matches(&after[0])
        && row_of(key, &after, &[0]).is_none()
        && index_of(key, &after).is_none()
}

fn test_exited_task_has_no_row() -> bool {
    let tasks = [task(10, 1), task(20, 2)];
    let key = TaskKey::of(&tasks[1]);
    row_of(key, &tasks[..1], &[0]).is_none()
}

fn test_row_beyond_table_selects_nothing() -> bool {
    let tasks = [task(10, 1)];
    key_at_row(&tasks, &[0], 5).is_none()
}

/// `index_of` addresses the task table, `row_of` the display order.
fn test_table_index_and_display_row_are_distinct() -> bool {
    let tasks = [task(10, 1), task(20, 2), task(30, 3)];
    let order = [2, 0, 1];
    let key = TaskKey::of(&tasks[2]);
    index_of(key, &tasks) == Some(2) && row_of(key, &tasks, &order) == Some(0)
}

/// Whatever comes of ending a task is said as a sentence, and each refusal
/// says its own cause.
fn test_every_ending_is_said_as_a_sentence() -> bool {
    use slopos_appkit::theme::words::sentence;
    use slopos_userland::apps::sysmon::{KillOutcome, status_line};
    let name = || String::from("terminal");
    let refusals = [
        KillOutcome::Powerful { name: name() },
        KillOutcome::System { name: name() },
        KillOutcome::Unowned { name: name() },
        KillOutcome::Refused { name: name() },
    ];
    let rest = [
        KillOutcome::Sent { name: name() },
        KillOutcome::Failed { name: name() },
        KillOutcome::Vanished { name: name() },
        KillOutcome::Asking { name: name() },
        KillOutcome::Denied {
            name: name(),
            why: Some(String::from("You didn't allow this.")),
            by_person: true,
        },
        KillOutcome::Denied {
            name: name(),
            why: None,
            by_person: false,
        },
    ];
    let said = |outcome: Option<&KillOutcome>| status_line(outcome).0;
    let mut refused: Vec<String> = refusals.iter().map(|o| said(Some(o))).collect();
    let every = refused
        .iter()
        .cloned()
        .chain(rest.iter().map(|o| said(Some(o))))
        .chain([said(None)]);
    let mut kept = true;
    for line in every {
        if sentence(&line, &[]).is_err() {
            eprintln!("sysmon_selection_test: {line:?}");
            kept = false;
        }
    }
    refused.sort_unstable();
    refused.dedup();
    kept && refused.len() == refusals.len()
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "every_ending_is_said_as_a_sentence",
        test_every_ending_is_said_as_a_sentence,
    ),
    (
        "selection_follows_task_across_resort",
        test_selection_follows_task_across_resort,
    ),
    (
        "selection_stable_when_order_unchanged",
        test_selection_stable_when_order_unchanged,
    ),
    (
        "recycled_pid_is_not_the_same_task",
        test_recycled_pid_is_not_the_same_task,
    ),
    ("exited_task_has_no_row", test_exited_task_has_no_row),
    (
        "row_beyond_table_selects_nothing",
        test_row_beyond_table_selects_nothing,
    ),
    (
        "table_index_and_display_row_are_distinct",
        test_table_index_and_display_row_are_distinct,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}

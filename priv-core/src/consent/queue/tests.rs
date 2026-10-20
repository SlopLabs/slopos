use super::*;
use crate::consent::{Secret, VERSION};
use alloc::vec;

fn ask(n: u32) -> Ask {
    Ask {
        id: 0,
        action: "org.slopos.boot.install".into(),
        describe: "Install".into(),
        program: None,
        uid: 1000,
        pid: n,
        cwd: "/".into(),
        command: vec![b"/bin/bootctl".to_vec()],
        kind: Kind::Confirm,
        person: None,
        admins: vec![],
    }
}

fn ask_as(n: u32, uid: u32) -> Ask {
    Ask { uid, ..ask(n) }
}

fn admin_ask(n: u32) -> Ask {
    Ask {
        kind: Kind::Admin,
        person: Some("bob".into()),
        admins: vec!["alice".into()],
        ..ask(n)
    }
}

fn ready() -> Questions<u32> {
    let mut q = Questions::default();
    q.ready(VERSION);
    q
}

fn deny(id: u64) -> Answer {
    Answer::deny(id)
}

fn allow(id: u64) -> Answer {
    Answer {
        id,
        allow: true,
        secret: None,
        admin: None,
    }
}

fn typed(text: &str) -> Secret {
    let mut secret = Secret::default();
    text.chars().for_each(|ch| assert!(secret.push(ch)));
    secret
}

#[test]
fn nothing_is_asked_before_the_agent_can_draw() {
    let mut q = Questions::default();
    assert_eq!(q.offer(ask(1), 1, 0), Err((1, Refusal::NoAgent)));
    assert!(q.next(0).is_none());
    q.ready(VERSION);
    assert_eq!(q.offer(ask(2), 2, 0), Ok(()));
}

#[test]
fn one_question_is_shown_at_a_time_and_numbered() {
    let mut q = ready();
    q.offer(ask(1), 1, 0).unwrap();
    q.offer(ask(2), 2, 0).unwrap();
    let (shown, asker) = q.next(0).map(|(a, t)| (a, *t)).unwrap();
    assert_eq!((shown.pid, asker, shown.id), (1, 1, 1));
    assert!(q.next(0).is_none());
    assert_eq!(q.answer(allow(1), 10), Ok(vec![(1, Ok(allow(1)))]));
    assert!(q.next(10).is_none());
    q.ready(VERSION);
    let (shown, asker) = q.next(10).map(|(a, t)| (a, *t)).unwrap();
    assert_eq!((shown.pid, asker, shown.id), (2, 2, 2));
}

#[test]
fn a_question_waits_for_the_agent_to_say_it_can_take_one() {
    let mut q = ready();
    q.offer(ask(1), 1, 0).unwrap();
    q.offer(ask(2), 2, 0).unwrap();
    let id = q.next(0).unwrap().0.id;
    assert_eq!(q.expired(EXPIRY_MS), Some((id, 1)));
    assert!(q.next(EXPIRY_MS).is_none());
    assert_eq!(q.offer(ask(3), 3, EXPIRY_MS), Ok(()));
    q.ready(VERSION);
    assert_eq!(q.next(EXPIRY_MS).map(|(a, _)| a.pid), Some(2));
}

#[test]
fn an_answer_to_another_question_settles_nothing() {
    let mut q = ready();
    q.offer(ask(1), 1, 0).unwrap();
    let id = q.next(0).unwrap().0.id;
    assert_eq!(q.answer(allow(id + 1), 0), Ok(vec![]));
    assert_eq!(q.answer(allow(0), 0), Ok(vec![]));
    assert_eq!(q.answer(deny(id), 0), Ok(vec![(1, Err(Refusal::Refused))]));
    assert_eq!(q.answer(allow(id), 0), Ok(vec![]));
}

#[test]
fn at_most_so_many_wait() {
    let mut q = ready();
    q.offer(ask(0), 0, 0).unwrap();
    q.next(0).unwrap();
    for n in 1..=WAITING_MAX as u32 {
        assert_eq!(q.offer(ask_as(n, 2000 + n), n, 0), Ok(()));
    }
    assert_eq!(q.offer(ask_as(99, 99), 99, 0), Err((99, Refusal::Busy)));
}

#[test]
fn one_uid_keeps_no_other_waiting() {
    let mut q = ready();
    for n in 1..=WAITING_EACH as u32 {
        assert_eq!(q.offer(ask(n), n, 0), Ok(()));
    }
    assert_eq!(q.offer(ask(10), 10, 0), Err((10, Refusal::Busy)));
    assert_eq!(q.offer(ask_as(11, 1001), 11, 0), Ok(()));
}

#[test]
fn a_refusal_holds_back_only_its_own_uid() {
    let mut q = ready();
    q.offer(ask(1), 1, 0).unwrap();
    q.offer(ask_as(2, 1001), 2, 0).unwrap();
    q.offer(ask(3), 3, 0).unwrap();
    let id = q.next(0).unwrap().0.id;
    assert_eq!(
        q.answer(deny(id), 100),
        Ok(vec![
            (1, Err(Refusal::Refused)),
            (3, Err(Refusal::Declined))
        ])
    );
    assert_eq!(q.offer(ask(4), 4, 100), Err((4, Refusal::Declined)));
    assert_eq!(q.offer(ask_as(5, 1001), 5, 100), Ok(()));
    q.ready(VERSION);
    assert_eq!(q.next(100).map(|(a, _)| a.pid), Some(2));
}

#[test]
fn a_refusal_refuses_what_waits_and_what_asks_soon_after() {
    let mut q = ready();
    for n in 1..=3 {
        q.offer(ask(n), n, 0).unwrap();
    }
    let id = q.next(0).unwrap().0.id;
    assert_eq!(
        q.answer(deny(id), 100),
        Ok(vec![
            (1, Err(Refusal::Refused)),
            (2, Err(Refusal::Declined)),
            (3, Err(Refusal::Declined)),
        ])
    );
    assert!(q.next(100).is_none());
    assert_eq!(
        q.offer(ask(4), 4, 100 + DECLINED_MS - 1),
        Err((4, Refusal::Declined))
    );
    assert_eq!(q.offer(ask(5), 5, 100 + DECLINED_MS), Ok(()));
}

#[test]
fn an_allowing_answer_leaves_the_rest_waiting() {
    let mut q = ready();
    q.offer(ask(1), 1, 0).unwrap();
    q.offer(ask(2), 2, 0).unwrap();
    let id = q.next(0).unwrap().0.id;
    assert_eq!(q.answer(allow(id), 0), Ok(vec![(1, Ok(allow(id)))]));
    assert_eq!(q.offer(ask(3), 3, 1), Ok(()));
    q.ready(VERSION);
    assert_eq!(q.next(1).map(|(a, _)| a.pid), Some(2));
}

#[test]
fn a_question_expires_from_when_it_is_shown() {
    let mut q = ready();
    q.offer(ask(1), 1, 0).unwrap();
    q.offer(ask(2), 2, 0).unwrap();
    q.next(0).unwrap();
    assert!(q.expired(EXPIRY_MS - 1).is_none());
    assert_eq!(q.expired(EXPIRY_MS), Some((1, 1)));
    let shown_at = EXPIRY_MS + 50;
    q.ready(VERSION);
    let id = q.next(shown_at).unwrap().0.id;
    assert!(q.expired(shown_at + EXPIRY_MS - 1).is_none());
    assert_eq!(q.expired(shown_at + EXPIRY_MS), Some((id, 2)));
}

#[test]
fn a_withdrawn_question_leaves_the_queue_or_the_screen() {
    let mut q = ready();
    for n in 1..=3 {
        q.offer(ask(n), n, 0).unwrap();
    }
    let id = q.next(0).unwrap().0.id;
    assert_eq!(q.withdraw(|&n| n == 2), None);
    assert_eq!(q.withdraw(|&n| n == 1), Some((id, 1)));
    q.ready(VERSION);
    assert_eq!(q.next(0).map(|(a, _)| a.pid), Some(3));
    assert!(q.next(0).is_none());
}

#[test]
fn a_lost_agent_refuses_everyone_from_then_on() {
    let mut q = ready();
    for n in 1..=3 {
        q.offer(ask(n), n, 0).unwrap();
    }
    q.next(0).unwrap();
    assert_eq!(q.lost(), vec![1, 2, 3]);
    assert_eq!(q.offer(ask(4), 4, 0), Err((4, Refusal::NoAgent)));
    assert!(q.next(0).is_none());
    q.ready(VERSION);
    assert_eq!(q.offer(ask(5), 5, 0), Ok(()));
    assert_eq!(q.next(0).map(|(a, _)| a.pid), Some(5));
}

/// Any sequence of steps shows every id once at most and settles every
/// asker once at most; one withdrawn while waiting is dropped unsettled.
#[test]
fn random_steps_settle_each_asker_once() {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..200 {
        let mut q = Questions::default();
        let mut settled = vec![0u32; 64];
        let mut offered = 0u32;
        let mut shown_ids = vec![];
        let mut now = 0u64;
        let mut current = None;
        for _ in 0..64 {
            now += next() % 40_000;
            match next() % 7 {
                0 => q.ready(VERSION),
                1 if offered < 64 => {
                    if let Err((n, _)) = q.offer(ask(offered), offered, now) {
                        settled[n as usize] += 1;
                    }
                    offered += 1;
                }
                2 => {
                    if let Some((a, &n)) = q.next(now) {
                        assert!(!shown_ids.contains(&a.id));
                        shown_ids.push(a.id);
                        current = Some((a.id, n));
                    }
                }
                3 => {
                    let id = current.map_or(next(), |(id, _)| id);
                    let answer = match next() % 2 {
                        0 => allow(id),
                        _ => deny(id),
                    };
                    for (n, _) in q.answer(answer, now).unwrap() {
                        settled[n as usize] += 1;
                    }
                }
                4 => {
                    if let Some((_, n)) = q.expired(now) {
                        settled[n as usize] += 1;
                    }
                }
                5 => {
                    let gone = (next() % 64) as u32;
                    if let Some((_, n)) = q.withdraw(|&m| m == gone) {
                        settled[n as usize] += 1;
                    }
                }
                _ => {
                    for n in q.lost() {
                        settled[n as usize] += 1;
                    }
                }
            }
        }
        assert!(settled.iter().all(|&count| count <= 1), "{settled:?}");
    }
}

#[test]
fn an_agent_of_the_first_version_is_asked_only_to_confirm() {
    let mut q = Questions::default();
    q.ready(1);
    assert_eq!(q.version(), 1);
    assert_eq!(q.offer(admin_ask(1), 1, 0), Err((1, Refusal::Outdated)));
    let password = Ask {
        kind: Kind::Password,
        person: Some("bob".into()),
        ..ask(2)
    };
    assert_eq!(q.offer(password, 2, 0), Err((2, Refusal::Outdated)));
    assert_eq!(q.offer(ask(3), 3, 0), Ok(()));
    q.lost();
    assert_eq!(q.version(), 0);
}

#[test]
fn an_answer_takes_the_form_its_question_does() {
    let mut q = ready();
    q.offer(admin_ask(1), 1, 0).unwrap();
    let id = q.next(0).unwrap().0.id;
    let as_who = |admin: Option<&str>, secret: Option<&str>| Answer {
        id,
        allow: true,
        secret: secret.map(typed),
        admin: admin.map(Into::into),
    };
    for wrong in [
        allow(id),
        as_who(None, Some("pw")),
        as_who(Some("mallory"), Some("pw")),
    ] {
        assert_eq!(q.answer(wrong, 0), Err(()));
    }
    let right = as_who(Some("alice"), Some("pw"));
    assert_eq!(q.answer(right.clone(), 0), Ok(vec![(1, Ok(right))]));
    q.ready(VERSION);
    q.offer(ask(2), 2, 0).unwrap();
    let id = q.next(0).unwrap().0.id;
    let with_secret = Answer {
        secret: Some(typed("pw")),
        ..allow(id)
    };
    assert_eq!(q.answer(with_secret, 0), Err(()));
    assert_eq!(q.answer(allow(id), 0), Ok(vec![(2, Ok(allow(id)))]));
}

#[test]
fn the_agent_says_once_that_the_question_is_on_screen() {
    let mut q = ready();
    q.offer(ask(1), 1, 0).unwrap();
    assert!(q.shown(1).is_none(), "nothing is on screen yet");
    let id = q.next(0).unwrap().0.id;
    assert!(q.shown(id + 1).is_none());
    assert_eq!(q.shown(id), Some(&1));
    assert!(q.shown(id).is_none());
}

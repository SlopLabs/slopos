use std::sync::Mutex;

const MAX_HISTORY: usize = 64;

/// Variable-length: a fixed per-entry width would reintroduce a line ceiling
/// the editor no longer has.
struct HistoryInner {
    entries: Vec<Vec<u8>>,
    count: usize,
    write_pos: usize,
    cursor: usize,
    browsing: bool,
    saved_input: Vec<u8>,
}

impl HistoryInner {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            count: 0,
            write_pos: 0,
            cursor: 0,
            browsing: false,
            saved_input: Vec::new(),
        }
    }
}

static HISTORY: Mutex<HistoryInner> = Mutex::new(HistoryInner::new());

fn with_history<R, F: FnOnce(&mut HistoryInner) -> R>(f: F) -> R {
    let mut history = HISTORY.lock().unwrap();
    if history.entries.len() != MAX_HISTORY {
        history.entries.resize(MAX_HISTORY, Vec::new());
    }
    f(&mut history)
}

pub fn push(line: &[u8], len: usize) {
    let line = &line[..len.min(line.len())];
    if line.is_empty() {
        return;
    }
    with_history(|h| {
        if h.count > 0 {
            let last_pos = if h.write_pos == 0 {
                MAX_HISTORY - 1
            } else {
                h.write_pos - 1
            };
            if h.entries[last_pos] == line {
                return;
            }
        }

        let slot = &mut h.entries[h.write_pos];
        slot.clear();
        slot.extend_from_slice(line);
        h.write_pos = (h.write_pos + 1) % MAX_HISTORY;
        if h.count < MAX_HISTORY {
            h.count += 1;
        }
    });
}

fn copy_out(entry: &[u8], out: &mut [u8]) -> usize {
    let copy_len = entry.len().min(out.len());
    out[..copy_len].copy_from_slice(&entry[..copy_len]);
    copy_len
}

pub fn navigate_up(current_input: &[u8], current_len: usize, out: &mut [u8]) -> Option<usize> {
    with_history(|h| {
        if h.count == 0 {
            return None;
        }

        if !h.browsing {
            h.saved_input.clear();
            h.saved_input
                .extend_from_slice(&current_input[..current_len.min(current_input.len())]);
            h.browsing = true;
            h.cursor = 0;
        } else if h.cursor + 1 >= h.count {
            return None;
        } else {
            h.cursor += 1;
        }

        let idx = if h.write_pos >= h.cursor + 1 {
            h.write_pos - h.cursor - 1
        } else {
            MAX_HISTORY + h.write_pos - h.cursor - 1
        } % MAX_HISTORY;

        Some(copy_out(&h.entries[idx], out))
    })
}

pub fn navigate_down(out: &mut [u8]) -> Option<usize> {
    with_history(|h| {
        if !h.browsing {
            return None;
        }

        if h.cursor == 0 {
            h.browsing = false;
            let copied = copy_out(&h.saved_input, out);
            return Some(copied);
        }

        h.cursor -= 1;

        let idx = if h.write_pos >= h.cursor + 1 {
            h.write_pos - h.cursor - 1
        } else {
            MAX_HISTORY + h.write_pos - h.cursor - 1
        } % MAX_HISTORY;

        Some(copy_out(&h.entries[idx], out))
    })
}

pub fn reset_cursor() {
    with_history(|h| {
        h.browsing = false;
        h.cursor = 0;
    });
}

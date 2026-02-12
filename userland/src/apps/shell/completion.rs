use core::ffi::c_char;

use crate::syscall::{UserFsEntry, UserFsList, fs};

use super::builtins::BUILTINS;
use super::parser::is_space;

pub struct CompletionResult {
    pub insertion: [u8; 128],
    pub insertion_len: usize,
    pub show_matches: bool,
    pub matches_buf: [u8; 512],
    pub matches_len: usize,
}

impl CompletionResult {
    fn empty() -> Self {
        Self {
            insertion: [0; 128],
            insertion_len: 0,
            show_matches: false,
            matches_buf: [0; 512],
            matches_len: 0,
        }
    }
}

pub fn try_complete(input: &[u8], len: usize, cursor_pos: usize, cwd: &[u8]) -> CompletionResult {
    let mut result = CompletionResult::empty();
    let effective_pos = cursor_pos.min(len);

    let mut word_start = effective_pos;
    while word_start > 0 && !is_space(input[word_start - 1]) {
        word_start -= 1;
    }

    let prefix = &input[word_start..effective_pos];
    let prefix_len = effective_pos - word_start;

    if prefix_len == 0 {
        return result;
    }

    let is_first_token = {
        let mut all_space = true;
        for i in 0..word_start {
            if !is_space(input[i]) {
                all_space = false;
                break;
            }
        }
        all_space
    };

    if is_first_token {
        complete_command(prefix, prefix_len, &mut result);
    } else {
        let dirs_only = command_wants_dirs_only(input, word_start);
        complete_path(prefix, prefix_len, cwd, dirs_only, &mut result);
    }

    result
}

fn complete_command(prefix: &[u8], prefix_len: usize, result: &mut CompletionResult) {
    let mut matches: [usize; 32] = [0; 32];
    let mut match_count = 0;

    for (i, entry) in BUILTINS.iter().enumerate() {
        if entry.name.len() >= prefix_len
            && &entry.name[..prefix_len] == prefix
            && match_count < matches.len()
        {
            matches[match_count] = i;
            match_count += 1;
        }
    }

    if match_count == 0 {
        return;
    }

    if match_count == 1 {
        let name = BUILTINS[matches[0]].name;
        let remaining = name.len() - prefix_len;
        let insert_len = remaining + 1;
        if insert_len <= result.insertion.len() {
            result.insertion[..remaining].copy_from_slice(&name[prefix_len..]);
            result.insertion[remaining] = b' ';
            result.insertion_len = insert_len;
        }
    } else {
        let first_name = BUILTINS[matches[0]].name;
        let mut common_len = first_name.len();
        for i in 1..match_count {
            let name = BUILTINS[matches[i]].name;
            let mut j = prefix_len;
            while j < common_len && j < name.len() && first_name[j] == name[j] {
                j += 1;
            }
            common_len = j;
        }

        if common_len > prefix_len {
            let remaining = common_len - prefix_len;
            result.insertion[..remaining].copy_from_slice(&first_name[prefix_len..common_len]);
            result.insertion_len = remaining;
        }

        result.show_matches = true;
        let mut pos = 0;
        for i in 0..match_count {
            let name = BUILTINS[matches[i]].name;
            if pos + name.len() + 2 < result.matches_buf.len() {
                result.matches_buf[pos..pos + name.len()].copy_from_slice(name);
                pos += name.len();
                result.matches_buf[pos] = b' ';
                pos += 1;
                result.matches_buf[pos] = b' ';
                pos += 1;
            }
        }
        if pos > 0 {
            pos -= 2;
        }
        result.matches_len = pos;
    }
}

fn command_wants_dirs_only(input: &[u8], word_start: usize) -> bool {
    let mut cmd_start = 0;
    while cmd_start < word_start && is_space(input[cmd_start]) {
        cmd_start += 1;
    }
    let mut cmd_end = cmd_start;
    while cmd_end < word_start && !is_space(input[cmd_end]) {
        cmd_end += 1;
    }
    let cmd = &input[cmd_start..cmd_end];
    cmd == b"cd" || cmd == b"mkdir"
}

fn complete_path(
    prefix: &[u8],
    prefix_len: usize,
    cwd: &[u8],
    dirs_only: bool,
    result: &mut CompletionResult,
) {
    let mut last_slash = None;
    for i in 0..prefix_len {
        if prefix[i] == b'/' {
            last_slash = Some(i);
        }
    }

    let (file_prefix, file_prefix_len) = if let Some(slash_pos) = last_slash {
        (&prefix[slash_pos + 1..], prefix_len - slash_pos - 1)
    } else {
        (prefix, prefix_len)
    };

    let mut dir_buf = [0u8; 256];
    let dir_len = build_dir_path(prefix, prefix_len, last_slash, cwd, &mut dir_buf);
    if dir_len == 0 {
        return;
    }
    dir_buf[dir_len] = 0;

    let mut entries = [UserFsEntry::new(); 32];
    let mut list = UserFsList {
        entries: entries.as_mut_ptr(),
        max_entries: entries.len() as u32,
        count: 0,
    };

    if fs::list_dir(dir_buf.as_ptr() as *const c_char, &mut list).is_err() {
        return;
    }

    let mut match_indices: [usize; 32] = [0; 32];
    let mut match_count = 0;

    for i in 0..list.count as usize {
        let entry = &entries[i];
        let name_len = entry_name_len(entry);

        if name_len == 1 && entry.name[0] == b'.' {
            continue;
        }
        if name_len == 2 && entry.name[0] == b'.' && entry.name[1] == b'.' {
            continue;
        }
        if dirs_only && !entry.is_directory() {
            continue;
        }

        if name_len >= file_prefix_len
            && &entry.name[..file_prefix_len] == file_prefix
            && match_count < match_indices.len()
        {
            match_indices[match_count] = i;
            match_count += 1;
        }
    }

    if match_count == 0 {
        return;
    }

    if match_count == 1 {
        let entry = &entries[match_indices[0]];
        let name_len = entry_name_len(entry);
        let remaining = name_len - file_prefix_len;
        let suffix = if entry.is_directory() { b'/' } else { b' ' };
        let insert_len = remaining + 1;
        if insert_len <= result.insertion.len() {
            result.insertion[..remaining].copy_from_slice(&entry.name[file_prefix_len..name_len]);
            result.insertion[remaining] = suffix;
            result.insertion_len = insert_len;
        }
    } else {
        let first = &entries[match_indices[0]];
        let first_len = entry_name_len(first);
        let mut common_len = first_len;

        for i in 1..match_count {
            let entry = &entries[match_indices[i]];
            let name_len = entry_name_len(entry);
            let mut j = file_prefix_len;
            while j < common_len && j < name_len && first.name[j] == entry.name[j] {
                j += 1;
            }
            common_len = j;
        }

        if common_len > file_prefix_len {
            let remaining = common_len - file_prefix_len;
            result.insertion[..remaining].copy_from_slice(&first.name[file_prefix_len..common_len]);
            result.insertion_len = remaining;
        }

        result.show_matches = true;
        let mut pos = 0;
        for i in 0..match_count {
            let entry = &entries[match_indices[i]];
            let name_len = entry_name_len(entry);
            if pos + name_len + 3 < result.matches_buf.len() {
                result.matches_buf[pos..pos + name_len].copy_from_slice(&entry.name[..name_len]);
                pos += name_len;
                if entry.is_directory() {
                    result.matches_buf[pos] = b'/';
                    pos += 1;
                }
                result.matches_buf[pos] = b' ';
                pos += 1;
                result.matches_buf[pos] = b' ';
                pos += 1;
            }
        }
        if pos > 0 {
            pos -= 2;
        }
        result.matches_len = pos;
    }
}

fn build_dir_path(
    prefix: &[u8],
    _prefix_len: usize,
    last_slash: Option<usize>,
    cwd: &[u8],
    dir_buf: &mut [u8; 256],
) -> usize {
    let cwd_len = cwd_strlen(cwd);

    if let Some(slash_pos) = last_slash {
        if prefix[0] == b'/' {
            let len = (slash_pos + 1).min(255);
            dir_buf[..len].copy_from_slice(&prefix[..len]);
            return len;
        }
        if cwd_len + slash_pos + 2 >= 255 {
            return 0;
        }
        dir_buf[..cwd_len].copy_from_slice(&cwd[..cwd_len]);
        let mut pos = cwd_len;
        if pos > 0 && dir_buf[pos - 1] != b'/' {
            dir_buf[pos] = b'/';
            pos += 1;
        }
        let path_part = slash_pos + 1;
        dir_buf[pos..pos + path_part].copy_from_slice(&prefix[..path_part]);
        return pos + path_part;
    }

    let len = cwd_len.min(255);
    dir_buf[..len].copy_from_slice(&cwd[..len]);
    len
}

fn entry_name_len(entry: &UserFsEntry) -> usize {
    entry
        .name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(entry.name.len())
}

fn cwd_strlen(cwd: &[u8]) -> usize {
    cwd.iter().position(|&b| b == 0).unwrap_or(cwd.len())
}

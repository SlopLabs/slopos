//! Standalone File Manager Application

use core::str;

use slopos_abi::draw::Color32;

use crate::appkit::{self, ControlFlow, Event, Window, WindowedApp};
use crate::gfx::{self, DrawBuffer};
use crate::syscall::{UserFsEntry, UserFsList, fs};
use crate::theme::*;

const FM_CONTENT_WIDTH: u32 = FM_WIDTH as u32;
const FM_CONTENT_HEIGHT: u32 = (FM_HEIGHT - FM_TITLE_HEIGHT) as u32;
const NAV_ROW_HEIGHT: i32 = 24;

pub struct FileManager {
    current_path: [u8; 128],
    entries: [UserFsEntry; 32],
    entry_count: u32,
    scroll_top: u32,
}

impl FileManager {
    fn new() -> Self {
        let mut fm = Self {
            current_path: [0; 128],
            entries: [UserFsEntry::new(); 32],
            entry_count: 0,
            scroll_top: 0,
        };
        fm.current_path[0] = b'/';
        fm.refresh();
        fm
    }

    fn refresh(&mut self) {
        self.entries = [UserFsEntry::new(); 32];
        let mut list = UserFsList {
            entries: self.entries.as_mut_ptr(),
            max_entries: 32,
            count: 0,
        };
        let _ = fs::list_dir(self.current_path.as_ptr() as *const i8, &mut list);
        self.entry_count = list.count.min(self.entries.len() as u32);
    }

    fn navigate(&mut self, name: &[u8]) {
        if name == b".." {
            let mut len = 0;
            while len < 128 && self.current_path[len] != 0 {
                len += 1;
            }
            if len > 1 {
                let mut i = len - 1;
                while i > 0 && self.current_path[i] != b'/' {
                    self.current_path[i] = 0;
                    i -= 1;
                }
                if i > 0 {
                    self.current_path[i] = 0;
                }
            }
        } else {
            let mut len = 0;
            while len < 128 && self.current_path[len] != 0 {
                len += 1;
            }
            if len + 1 + name.len() + 1 <= 128 {
                if len > 1 || (len == 1 && self.current_path[0] != b'/') {
                    self.current_path[len] = b'/';
                    len += 1;
                } else if len == 0 {
                    self.current_path[0] = b'/';
                    len = 1;
                }
                for (i, &b) in name.iter().enumerate() {
                    self.current_path[len + i] = b;
                }
                self.current_path[len + name.len()] = 0;
            }
        }
        self.refresh();
        self.scroll_top = 0;
    }

    fn handle_click(&mut self, x: i32, y: i32) -> bool {
        if y >= 0 && y < NAV_ROW_HEIGHT {
            if x >= 4 && x < 4 + BUTTON_SIZE {
                self.navigate(b"..");
                return true;
            }
            return false;
        }

        let list_y = y - NAV_ROW_HEIGHT;
        if list_y >= 0 && x >= 0 {
            let idx = (list_y / FM_ITEM_HEIGHT) as u32;
            let entry_idx = self.scroll_top + idx;
            if entry_idx < self.entry_count {
                let entry = self.entries[entry_idx as usize];
                if entry.is_directory() {
                    let mut name_len = 0;
                    while name_len < 64 && entry.name[name_len] != 0 {
                        name_len += 1;
                    }
                    let name = &entry.name[..name_len];
                    self.navigate(name);
                }
                return true;
            }
        }
        false
    }
}

impl WindowedApp for FileManager {
    fn init(&mut self, win: &mut Window) {
        win.set_title("Files");
        win.request_redraw();
    }

    fn on_event(&mut self, win: &mut Window, event: Event) -> ControlFlow {
        match event {
            Event::CloseRequest => return ControlFlow::Exit,
            Event::PointerPress { .. } => {
                let (px, py) = win.pointer();
                if self.handle_click(px, py) {
                    win.request_redraw();
                }
            }
            _ => {}
        }
        ControlFlow::Continue
    }

    fn draw(&mut self, fb: &mut DrawBuffer<'_>) {
        let width = fb.width() as i32;
        let height = fb.height() as i32;

        gfx::fill_rect(fb, 0, 0, width, height, FM_COLOR_BG);

        gfx::fill_rect(fb, 0, 0, width, NAV_ROW_HEIGHT, COLOR_TITLE_BAR);
        gfx::fill_rect(fb, 4, 4, BUTTON_SIZE, BUTTON_SIZE - 8, COLOR_BUTTON);
        gfx::font::draw_string(fb, 8, 4, "^", COLOR_TEXT, COLOR_BUTTON);

        let mut len = 0;
        while len < self.current_path.len() && self.current_path[len] != 0 {
            len += 1;
        }
        let path_str = str::from_utf8(&self.current_path[..len]).unwrap_or("/");
        gfx::font::draw_string(
            fb,
            4 + BUTTON_SIZE + 8,
            4,
            path_str,
            COLOR_TEXT,
            COLOR_TITLE_BAR,
        );

        let list_start_y = NAV_ROW_HEIGHT;
        let available_height = height - NAV_ROW_HEIGHT;
        let max_visible = available_height / FM_ITEM_HEIGHT;

        for i in 0..self.entry_count as usize {
            if i < self.scroll_top as usize {
                continue;
            }
            let row = (i as i32) - self.scroll_top as i32;
            if row >= max_visible {
                break;
            }
            let item_y = list_start_y + (row * FM_ITEM_HEIGHT);
            let entry = &self.entries[i];

            let mut name_len = 0;
            while name_len < 64 && entry.name[name_len] != 0 {
                name_len += 1;
            }
            let name = str::from_utf8(&entry.name[..name_len]).unwrap_or("?");

            let color = if entry.is_directory() {
                Color32::rgb(0x40, 0x80, 0xFF)
            } else {
                COLOR_TEXT
            };
            gfx::font::draw_string(fb, 8, item_y + 2, name, color, FM_COLOR_BG);
        }
    }
}

pub fn file_manager_main(_arg: *mut core::ffi::c_void) -> ! {
    let fm = FileManager::new();
    appkit::run(fm, FM_CONTENT_WIDTH, FM_CONTENT_HEIGHT)
}

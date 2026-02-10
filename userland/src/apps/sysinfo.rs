use core::ffi::c_void;

use slopos_abi::PAGE_SIZE;
use slopos_lib::numfmt;

use crate::appkit::{self, Window, WindowedApp};
use crate::gfx::{self, DrawBuffer};
use crate::syscall::{UserSysInfo, core as sys_core};
use crate::theme::{COLOR_BACKGROUND, COLOR_TEXT};

const SYSINFO_WIDTH: u32 = 360;
const SYSINFO_HEIGHT: u32 = 258;
const MARGIN_X: i32 = 12;
const MARGIN_Y: i32 = 12;
const LINE_HEIGHT: i32 = 18;

pub struct SysinfoApp;

impl WindowedApp for SysinfoApp {
    fn init(&mut self, win: &mut Window) {
        win.set_title("Sysinfo");
        win.request_redraw();
    }

    fn draw(&mut self, fb: &mut DrawBuffer<'_>) {
        let width = fb.width() as i32;
        let height = fb.height() as i32;
        gfx::fill_rect(fb, 0, 0, width, height, COLOR_BACKGROUND);

        let cpu_count = sys_core::get_cpu_count() as u64;
        let current_cpu = sys_core::get_current_cpu() as u64;
        let mut info = UserSysInfo::default();
        let sys_rc = sys_core::sys_info(&mut info);

        let mut line = [0u8; 96];
        let mut y = MARGIN_Y;

        draw_text(fb, MARGIN_X, y, "SLOPOS SYSINFO");
        y += LINE_HEIGHT;

        draw_text(
            fb,
            MARGIN_X,
            y,
            format_line(&mut line, "CPUs available: ", cpu_count, ""),
        );
        y += LINE_HEIGHT;

        draw_text(
            fb,
            MARGIN_X,
            y,
            format_line(&mut line, "Current CPU: ", current_cpu, ""),
        );
        y += LINE_HEIGHT;

        if sys_rc == 0 {
            let total_mib = pages_to_mib(info.total_pages as u64);
            let free_mib = pages_to_mib(info.free_pages as u64);
            let alloc_mib = pages_to_mib(info.allocated_pages as u64);

            draw_text(
                fb,
                MARGIN_X,
                y,
                format_line(&mut line, "Memory total: ", total_mib, " MiB"),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                format_line(&mut line, "Memory free: ", free_mib, " MiB"),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                format_line(&mut line, "Memory alloc: ", alloc_mib, " MiB"),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                format_line(&mut line, "Tasks total: ", info.total_tasks as u64, ""),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                format_line(&mut line, "Tasks active: ", info.active_tasks as u64, ""),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                format_line(&mut line, "Tasks ready: ", info.ready_tasks as u64, ""),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                format_line(
                    &mut line,
                    "Task ctx switches: ",
                    info.task_context_switches,
                    "",
                ),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                format_line(
                    &mut line,
                    "Scheduler switches: ",
                    info.scheduler_context_switches,
                    "",
                ),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                format_line(&mut line, "Scheduler yields: ", info.scheduler_yields, ""),
            );
            y += LINE_HEIGHT;
        } else {
            draw_text(fb, MARGIN_X, y, "System info: unavailable");
            y += LINE_HEIGHT;
        }

        draw_text(fb, MARGIN_X, y, "Drivers: kernel-managed");
    }
}

pub fn sysinfo_main(_arg: *mut c_void) -> ! {
    appkit::run(SysinfoApp, SYSINFO_WIDTH, SYSINFO_HEIGHT)
}

fn draw_text(fb: &mut DrawBuffer<'_>, x: i32, y: i32, text: &str) {
    gfx::font::draw_string(fb, x, y, text, COLOR_TEXT, COLOR_BACKGROUND);
}

fn format_line<'a>(buf: &'a mut [u8; 96], label: &str, value: u64, suffix: &str) -> &'a str {
    let mut idx = 0usize;
    idx = copy_bytes(buf, idx, label.as_bytes());

    let mut num = [0u8; 21];
    let formatted = numfmt::fmt_u64(value, &mut num);
    let digits = formatted.strip_suffix(&[0]).unwrap_or(formatted);
    idx = copy_bytes(buf, idx, digits);

    idx = copy_bytes(buf, idx, suffix.as_bytes());
    core::str::from_utf8(&buf[..idx]).unwrap_or("???")
}

fn pages_to_mib(pages: u64) -> u64 {
    pages.saturating_mul(PAGE_SIZE) / (1024 * 1024)
}

fn copy_bytes(buf: &mut [u8; 96], mut idx: usize, src: &[u8]) -> usize {
    for &b in src {
        if idx >= buf.len() {
            break;
        }
        buf[idx] = b;
        idx += 1;
    }
    idx
}

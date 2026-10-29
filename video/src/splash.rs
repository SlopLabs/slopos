//! The boot splash: the ring and the logo on black ([`slopos_splash_core`]),
//! its segments lighting clockwise as boot steps complete. `/bin/roulette`
//! opens on the same pixels, so the boot hands the Wheel of Fate its screen
//! unchanged.

use slopos_abi::draw::Canvas;
use slopos_abi::video_traits::VideoError;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_splash_core::{self as splash, ALL, Layout};

use crate::framebuffer;
use crate::graphics::{GraphicsContext, GraphicsResult};

struct SplashState {
    active: bool,
    /// Percent of the way through boot.
    progress: u32,
    /// The on-screen log was up for a step, so what the splash last painted
    /// is gone.
    covered: bool,
}

static STATE: SpinLock<SplashState> = SpinLock::new(
    SplashState {
        active: false,
        progress: 0,
        covered: false,
    },
    lock_class!("splash.STATE", LOCK_LEVEL_RESOURCE),
);

fn paint_all(ctx: &mut GraphicsContext, progress: u32) {
    let layout = Layout::for_screen(ctx.width(), ctx.height());
    ctx.clear_canvas(ctx.pixel_format().encode(splash::BACKDROP));
    layout.dial.paint(ctx, &splash::boot_inks(progress), ALL);
    layout.logo.paint(ctx, u8::MAX);
}

/// Paint the whole splash at the progress boot has reached, so a backend
/// upgrade keeps the ring where it was.
pub fn splash_show_boot_screen() -> GraphicsResult<()> {
    let mut state = STATE.lock();
    state.active = true;
    // Never paint over the on-screen kernel log.
    if slopos_ostd::fblog::is_active() {
        state.covered = true;
        return Ok(());
    }
    let mut ctx = GraphicsContext::new()?;
    paint_all(&mut ctx, state.progress);
    state.covered = false;
    Ok(())
}

/// Light the ring to `progress` percent, repainting only the segments that
/// change.
pub fn splash_report_progress(progress: u32) -> GraphicsResult<()> {
    let mut state = STATE.lock();
    if !state.active {
        return Err(VideoError::Invalid);
    }
    let before = state.progress;
    state.progress = progress.min(100);
    if slopos_ostd::fblog::is_active() {
        state.covered = true;
        return Ok(());
    }
    let changed = splash::boot_changed(before, state.progress);
    if changed == 0 && !state.covered {
        return Ok(());
    }
    let mut ctx = GraphicsContext::new()?;
    if state.covered {
        paint_all(&mut ctx, state.progress);
        state.covered = false;
        framebuffer::framebuffer_flush(&[]);
        return Ok(());
    }
    let dial = Layout::for_screen(ctx.width(), ctx.height()).dial;
    let Some(area) = dial.bounds_of(changed) else {
        return Ok(());
    };
    dial.paint(&mut ctx, &splash::boot_inks(state.progress), changed);
    // A GPU backend only reaches the screen once flushed.
    framebuffer::framebuffer_flush(&[area.clip(ctx.width() as i32, ctx.height() as i32)]);
    Ok(())
}

//! Font management syscall handlers (inspired by Linux KDFONTOP).

use slopos_abi::Errno;
use slopos_abi::io::IoBufRead;
use slopos_mm::user_io_buf::UserReadBuf;
use slopos_ostd::klog_info;

static FONT_WRITER_LOCK: slopos_ostd::sync::SpinLock<()> = slopos_ostd::sync::SpinLock::new(
    (),
    slopos_ostd::lock_class!("FONT_WRITER_LOCK", slopos_ostd::sync::LOCK_LEVEL_RESOURCE),
);

fn replace_and_schedule_free(new_atlas: slopos_font::atlas::GlyphAtlas) {
    {
        let _writer = FONT_WRITER_LOCK.lock();
        slopos_font::atlas::replace_global(new_atlas);
    }
    slopos_font::atlas::invoke_font_change_callback();
}

define_syscall!(syscall_font_set
    (ctx, data_ptr: u64, width_raw: u32, height_raw: u32, glyph_count_raw: u32)
    cap(ConsoleConfig)
    -> Result<(), Errno>
{
    let width = width_raw as u16;
    let height = height_raw as u16;
    let glyph_count = glyph_count_raw as usize;

    if data_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    if width == 0 || height == 0 || height > 32 {
        return Err(Errno::EINVAL);
    }
    if glyph_count != slopos_font::GLYPH_COUNT {
        return Err(Errno::EINVAL);
    }

    // Bounds the *user* buffer only; the atlas reads it in chunks. It is
    // also what bounds `width`, which the ABI leaves a full `u16`.
    const MAX_COVERAGE_UPLOAD: usize = (slopos_font::GLYPH_COUNT + 1) * 32 * 32;
    let stride = (width as usize).checked_mul(height as usize).ok_or(Errno::EINVAL)?;
    let data_size = (slopos_font::GLYPH_COUNT + 1)
        .checked_mul(stride)
        .filter(|&size| size <= MAX_COVERAGE_UPLOAD)
        .ok_or(Errno::EINVAL)?;

    let src = UserReadBuf::new(data_ptr, data_size).ok_or(Errno::EFAULT)?;
    let mut builder = slopos_font::atlas::AtlasBuilder::new(width, height).ok_or(Errno::ENOMEM)?;

    let mut at = 0usize;
    for index in 0..builder.chunk_count() {
        let chunk = builder.chunk_mut(index).ok_or(Errno::EINVAL)?;
        if src.copy_out(at, chunk)? != chunk.len() {
            return Err(Errno::EFAULT);
        }
        at += chunk.len();
    }
    let replacement = builder.replacement_mut();
    if src.copy_out(at, replacement)? != replacement.len() {
        return Err(Errno::EFAULT);
    }

    replace_and_schedule_free(builder.finish());
    klog_info!(
        "FONT_SET: applied {}x{} coverage font ({} glyphs + replacement)",
        width,
        height,
        glyph_count,
    );
    Ok(())
});

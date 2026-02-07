# SlopOS Compositor Rendering Gold-Standard Plan

> Generated: 2026-02-07  
> Scope: Deep compositor analysis and practical upgrade roadmap  
> Goal: Clean rendering with minimal artifacts, strong Rust safety, and high performance without Linux-scale overengineering

---

## 1) Current State and Root Causes

### 1.1 Confirmed bottlenecks in current SlopOS pipeline

- Full redraw every frame: `userland/src/compositor.rs:1306` clears full output, then `userland/src/compositor.rs:1310` redraws all windows.
- Full framebuffer copy on present: `video/src/framebuffer.rs:399` performs `copy_nonoverlapping` from SHM to FB.
- Damage exported but not consumed for partial rendering: damage is exported in `video/src/compositor_context.rs:522`, but rendering still redraws full scene.
- Damage lifecycle is incomplete: kernel intentionally does not clear exported damage (`video/src/compositor_context.rs:548`), awaiting explicit acknowledge flow.
- Frame pacing relies on sleep/yield loop: `userland/src/compositor.rs:1448` and `userland/src/compositor.rs:1451`.

### 1.2 Why artifacts appear under fast interaction

- Memory bandwidth is spent on full clear + full scene render + full SHM->FB copy even for tiny updates.
- Present timing is not tightly coupled to hardware refresh, so fast updates can expose mid-update visual artifacts.
- Dirty-region infrastructure exists but does not currently gate copy/render work.

---

## 2) High-Value Patterns to Borrow (and only the good parts)

### 2.1 Rust ecosystem patterns

### Smithay (Rust compositor stack)

- Aggregate and drain per-commit `damage` and `frame_callbacks` after processing/present.
- Keep rendering decision (what changed) separate from presentation decision (when it is shown).
- Use explicit commit-state handling to avoid stale callback/damage state growth.

References:
- https://github.com/Smithay/smithay/blob/master/src/wayland/compositor/mod.rs
- https://github.com/Smithay/smithay/blob/master/src/wayland/compositor/handlers.rs

### Redox Orbital (Rust OS desktop server)

- Keep compositor architecture simple: one server owning composition policy.
- Prioritize deterministic behavior and easy-to-reason buffer flow over feature-heavy protocol surface.

Reference:
- https://gitlab.redox-os.org/redox-os/orbital

### 2.2 Linux/GNU patterns worth copying

- Use damage clips as performance hints (do less work, never less correctness): DRM `FB_DAMAGE_CLIPS` model.
- Iterate damage clips and clip updates to changed rectangles, with full-update fallback when needed.
- Keep distinction clear between frame damage and buffer damage; start with frame damage for simplicity.

References:
- https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/drm_plane.c
- https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/drm_damage_helper.c

---

## 3) Target Architecture for SlopOS (lean, safe, performant)

### 3.1 Rendering model

- Keep current single-compositor process model.
- Move from full redraw to damage-gated redraw:
  - Track global output damage.
  - For each window, compute intersection(window bounds, output damage).
  - Redraw only intersecting regions.

### 3.2 Presentation model

- Move from full SHM->FB copy to damage-aware copy:
  - Copy only damaged rectangles when damage is bounded.
  - Fall back to full copy when damage is marked full/unknown.
- Preserve correctness-first fallback behavior.

### 3.3 Frame pacing model

- Tie `surface_mark_frames_done` to successful present completion, not just loop iteration timing.
- Keep fixed refresh target initially (60 Hz equivalent), then optionally add backend-specific vblank synchronization.
- Maintain one frame clock path (avoid mixed `sleep_ms`/`yield_now` jitter behavior where possible).

### 3.4 Safety model

- Keep unsafe blocks tiny and encapsulated around MMIO/pointer writes.
- Replace global mutable driver state with typed synchronization primitives.
- Remove broad unsafe allowances once wrappers exist.

---

## 4) Implementation Plan

### Phase A - Instrumentation First (quick win)

1. Add frame counters and timers for:
   - full redraw frames
   - partial redraw frames
   - bytes copied to framebuffer per frame
   - frame time p50/p95/p99
2. Log dropped/late frame counts under stress.

Outcome: Objective baseline before behavior changes.

### Phase B - Consume Damage in Render

1. Keep existing damage accumulation in `refresh_windows`.
2. Modify `render` pipeline to:
   - avoid unconditional full clear
   - clear only damaged regions
   - redraw only windows intersecting damaged regions
3. Keep full-redraw fallback for uncertain damage.

Outcome: Major reduction in unnecessary composition work.

### Phase C - Damage-Aware Present Copy

1. Extend present path to receive damage rectangles.
2. Copy only damaged rectangles in `fb_flip_from_shm`.
3. Use full copy when damage is full/overflow/invalid.

Outcome: Significant reduction in memory bandwidth and copy latency.

### Phase D - Frame Callback and Pacing Hygiene

1. Ensure frame callbacks are signaled only after present is accepted.
2. Normalize frame loop pacing into one consistent timing path.
3. Add backend guardrail for optional vblank-aware present in Xe path.

Outcome: Better frame cadence and fewer visible artifacts in fast motion.

### Phase E - Rust Safety Hardening

1. Replace `static mut XE_DEVICE` with synchronized typed state in `drivers/src/xe/mod.rs`.
2. Wrap framebuffer pointer arithmetic in checked helper APIs.
3. Remove or narrow `#![allow(unsafe_op_in_unsafe_fn)]` in `video/src/lib.rs` once wrappers are in place.

Outcome: Stronger compile-time guarantees without sacrificing low-level control.

---

## 5) What to Explicitly Avoid

- Do not import full Linux DRM atomic architecture into SlopOS now.
- Do not add multi-plane/advanced fence infrastructure before fixing single-output dirty-region flow.
- Do not pursue triple buffering before damage-aware render and damage-aware present are validated.

---

## 6) Success Criteria

- Visual: No obvious tearing/artifacts during fast window movement and rapid updates.
- Performance:
  - Reduced average bytes copied per frame under partial-update workloads.
  - Improved frame-time p95 and p99 under stress.
- Correctness: No stale window content after partial redraw/copy transitions.
- Safety: Reduced unsafe surface area in compositor/video hot path.

---

## 7) Verification Matrix

- Stress scenarios:
  - rapid drag/resize of overlapping windows
  - text-heavy updates (many small damage regions)
  - cursor-heavy movement over static background
- Verify:
  - no corruption/artifacts
  - no regressions in frame callbacks
  - stable frame pacing metrics

---

## 8) Priority Order

1. Phase A (instrumentation)
2. Phase B (damage-aware render)
3. Phase C (damage-aware present copy)
4. Phase D (frame pacing/callback hygiene)
5. Phase E (safety hardening)

This ordering yields fast quality gains first, then performance wins, then long-term maintainability.

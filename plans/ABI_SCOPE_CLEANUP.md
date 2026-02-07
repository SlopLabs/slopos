# ABI Crate Scope Cleanup — Extract `gfx` Crate

## Table of Contents

- [Motivation](#motivation)
- [Architecture Reference](#architecture-reference)
- [Phase 0: Create the `gfx` crate skeleton](#phase-0-create-the-gfx-crate-skeleton)
- [Phase 1: Move `draw_primitives`](#phase-1-move-draw_primitives)
- [Phase 2: Move `font_render`](#phase-2-move-font_render)
- [Phase 3: Move `DamageTracker` implementation](#phase-3-move-damagetracker-implementation)
- [Phase 4: Collapse wrapper modules in consumers](#phase-4-collapse-wrapper-modules-in-consumers)
- [Phase 5: Consolidate scattered constants](#phase-5-consolidate-scattered-constants)
- [Verification](#verification)

---

## Motivation

The `abi` crate declares itself as "the single source of truth for all types shared between
kernel and userland" (`#![forbid(unsafe_code)]`). 17 of its 20 modules are pure types,
constants, and trait definitions — exactly right. But 3 modules contain algorithmic
implementation code that does not belong in an ABI contract crate:

| Module | Lines | Content |
|--------|------:|---------|
| `draw_primitives.rs` | 177 | Bresenham line, midpoint circle, scanline triangle, rect fill |
| `font_render.rs` | 104 | Glyph rasterization loop, string layout with wrapping/tabs |
| `damage.rs` (partial) | ~220 | `DamageTracker<N>` with merge_smallest_pair, merge_all_overlapping |

These are consumed by exactly two crates (`video` and `userland`), both of which maintain
thin wrapper modules that delegate to `abi` and then add damage tracking on top. The
wrappers are pure indirection that exists only because the implementation is in the wrong
crate.

This matches a well-known anti-pattern: the "fat interface crate" where types and algorithms
are mixed, forcing all consumers to take a dependency on implementation details they may not
need and causing wrapper proliferation in those that do.

**Reference projects** (Redox OS, Theseus OS, embedded-graphics) all solve this the same
way: a minimal trait/types crate at the bottom, a separate algorithms crate in the middle,
and concrete implementations at the top.

---

## Architecture Reference

### Before (current)

```
abi  (types + traits + ALGORITHMS)
 ↑
 ├── video   (impl DrawTarget, wraps abi algorithms)
 └── userland (impl DrawTarget, wraps abi algorithms)
```

### After (target)

```
abi  (types + traits only)
 ↑
 gfx  [NEW] (algorithms, generic over DrawTarget)
 ↑
 ├── video   (impl DrawTarget, calls gfx directly)
 └── userland (impl DrawTarget, calls gfx directly)
```

### What stays in `abi`

All 17 pure-type modules (unchanged), plus:

- `draw.rs` — `DrawTarget`, `PixelBuffer`, `DamageTracking` traits + `pixel_ops` helpers
  (trivial offset/bounds math, ~70 lines, not algorithms)
- `font.rs` — `FONT_DATA` static bitmap + `get_glyph()` lookup (shared data, not rendering)
- `damage.rs` — `DamageRect` struct + `DamageTracking` trait definition only (the interface)

### What moves to `gfx`

- `draw_primitives.rs` → `gfx/src/primitives.rs`
- `font_render.rs` → `gfx/src/font_render.rs`
- `DamageTracker<N>` impl from `damage.rs` → `gfx/src/damage.rs`

---

## Phase 0: Create the `gfx` crate skeleton

**Goal**: Introduce the new crate with empty modules. Build must pass with no functional changes.

### Steps

1. Create `gfx/Cargo.toml`:
   ```toml
   [package]
   name = "slopos-gfx"
   edition.workspace = true
   version.workspace = true
   license.workspace = true

   [lints]
   workspace = true

   [dependencies]
   slopos-abi = { workspace = true }
   ```

2. Create `gfx/src/lib.rs`:
   ```rust
   //! SlopOS Graphics Algorithms
   //!
   //! Generic drawing primitives, font rendering, and damage tracking
   //! algorithms that operate on any `DrawTarget` implementation.
   //!
   //! This crate sits between the ABI trait definitions (in `slopos-abi`)
   //! and the concrete implementations (in `video` and `userland`).

   #![no_std]
   #![forbid(unsafe_code)]

   pub mod damage;
   pub mod font_render;
   pub mod primitives;
   ```

3. Create empty placeholder files:
   - `gfx/src/primitives.rs` (empty)
   - `gfx/src/font_render.rs` (empty)
   - `gfx/src/damage.rs` (empty)

4. Add to workspace root `Cargo.toml`:
   - Add `"gfx"` to `[workspace].members`
   - Add `slopos-gfx = { path = "gfx" }` to `[workspace.dependencies]`

5. Add `slopos-gfx` dependency to `video/Cargo.toml` and `userland/Cargo.toml`.

6. Run `make build` — must compile cleanly with no functional change.

### Files touched
- `Cargo.toml` (workspace root)
- `gfx/Cargo.toml` (new)
- `gfx/src/lib.rs` (new)
- `gfx/src/primitives.rs` (new, empty)
- `gfx/src/font_render.rs` (new, empty)
- `gfx/src/damage.rs` (new, empty)
- `video/Cargo.toml`
- `userland/Cargo.toml`

---

## Phase 1: Move `draw_primitives`

**Goal**: `abi/src/draw_primitives.rs` is deleted. All code that called
`slopos_abi::draw_primitives::*` now calls `slopos_gfx::primitives::*`.

### Steps

1. **Move the file**: Copy `abi/src/draw_primitives.rs` content into
   `gfx/src/primitives.rs`. Update the import at the top from `crate::draw::DrawTarget`
   to `slopos_abi::draw::DrawTarget`.

2. **Update `video/src/graphics.rs`**: Change all occurrences of
   `use slopos_abi::draw_primitives` or `draw_primitives::*` to
   `use slopos_gfx::primitives` / `slopos_gfx::primitives::*`.

   Affected functions (lines 204-226):
   - `fill_rect()` → `slopos_gfx::primitives::fill_rect()`
   - `draw_rect()` → `slopos_gfx::primitives::rect()`
   - `draw_line()` → `slopos_gfx::primitives::line()`
   - `draw_circle()` → `slopos_gfx::primitives::circle()`
   - `draw_circle_filled()` → `slopos_gfx::primitives::circle_filled()`

3. **Update `userland/src/gfx/primitives.rs`**: Change `use slopos_abi::draw_primitives`
   to `use slopos_gfx::primitives`.

   Affected calls:
   - `draw_primitives::fill_rect()` → `slopos_gfx::primitives::fill_rect()`
   - `draw_primitives::line()` → `slopos_gfx::primitives::line()`
   - `draw_primitives::circle()` → `slopos_gfx::primitives::circle()`
   - `draw_primitives::circle_filled()` → `slopos_gfx::primitives::circle_filled()`
   - `draw_primitives::rect()` → `slopos_gfx::primitives::rect()`

4. **Check for any other consumers**: Grep for `draw_primitives` across the entire
   workspace. There should be no remaining references outside `gfx`.

5. **Delete from abi**: Remove `pub mod draw_primitives;` from `abi/src/lib.rs`. Delete
   `abi/src/draw_primitives.rs`.

6. **Remove re-export**: If `abi/src/lib.rs` has any `pub use draw_primitives::*`, remove it.

7. Run `make build` — must compile. Run `make test` — must pass.

### Files touched
- `abi/src/lib.rs` (remove module declaration)
- `abi/src/draw_primitives.rs` (deleted)
- `gfx/src/primitives.rs` (receives content)
- `video/src/graphics.rs` (update imports)
- `userland/src/gfx/primitives.rs` (update imports)

### Verification
```
grep -r "draw_primitives" --include="*.rs" . | grep -v target/ | grep -v gfx/src/primitives.rs
```
Must return zero results.

---

## Phase 2: Move `font_render`

**Goal**: `abi/src/font_render.rs` is deleted. All code that called
`slopos_abi::font_render::*` now calls `slopos_gfx::font_render::*`.

### Steps

1. **Move the file**: Copy `abi/src/font_render.rs` content into
   `gfx/src/font_render.rs`. Update imports:
   - `crate::draw::DrawTarget` → `slopos_abi::draw::DrawTarget`
   - `crate::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH, get_glyph_or_space}` →
     `slopos_abi::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH, get_glyph_or_space}`

2. **Update `video/src/font.rs`**: Change all `use slopos_abi::font_render` and
   `font_render::*` calls to `use slopos_gfx::font_render`.

   Affected calls:
   - `font_render::draw_char()` → `slopos_gfx::font_render::draw_char()`
   - `font_render::draw_string()` → `slopos_gfx::font_render::draw_string()`
   - `font_render::draw_str()` → `slopos_gfx::font_render::draw_str()`
   - `font_render::string_width()` → `slopos_gfx::font_render::string_width()`
   - `font_render::string_lines()` → `slopos_gfx::font_render::string_lines()`

3. **Update `userland/src/gfx/font.rs`**: Change `use slopos_abi::font_render` to
   `use slopos_gfx::font_render`.

   Affected calls:
   - `font_render::draw_char()` → `slopos_gfx::font_render::draw_char()`
   - `font_render::draw_str()` → `slopos_gfx::font_render::draw_str()`
   - `font_render::str_width()` → `slopos_gfx::font_render::str_width()`
   - `font_render::str_lines()` → `slopos_gfx::font_render::str_lines()`

4. **Check for any other consumers**: Grep for `font_render` across the workspace
   (excluding `gfx/src/font_render.rs` itself). There may be direct callers in
   `video/src/splash.rs`, `video/src/panic_screen.rs`, `video/src/roulette_core.rs`,
   or compositor files. All must be updated.

5. **Delete from abi**: Remove `pub mod font_render;` from `abi/src/lib.rs`. Delete
   `abi/src/font_render.rs`.

6. Run `make build` — must compile. Run `make test` — must pass.

### Files touched
- `abi/src/lib.rs` (remove module declaration)
- `abi/src/font_render.rs` (deleted)
- `gfx/src/font_render.rs` (receives content)
- `video/src/font.rs` (update imports)
- `userland/src/gfx/font.rs` (update imports)
- Any other files found by grep in step 4

### Verification
```
grep -r "slopos_abi::font_render\|abi::font_render" --include="*.rs" . | grep -v target/
```
Must return zero results.

---

## Phase 3: Move `DamageTracker` implementation

**Goal**: `abi/src/damage.rs` retains only the `DamageRect` struct, its methods, the
`DamageTracking` trait, and the two `MAX_*` constants. The `DamageTracker<N>` generic
struct with its merge algorithms moves to `gfx/src/damage.rs`.

This is the most delicate phase because `DamageTracker` is used by both kernel-side
compositor code and userland-side drawing code.

### Steps

1. **Split `abi/src/damage.rs`**:

   What **stays** in `abi/src/damage.rs`:
   - `MAX_DAMAGE_REGIONS` constant
   - `MAX_INTERNAL_DAMAGE_REGIONS` constant
   - `DamageRect` struct with all its methods (`invalid`, `is_valid`, `area`, `union`,
     `combined_area`, `clip`, `intersects`)

   What **moves** to `gfx/src/damage.rs`:
   - `DamageTracker<N>` struct definition
   - `impl<const N: usize> Default for DamageTracker<N>`
   - `impl<const N: usize> DamageTracker<N>` (all methods: `new`, `add`,
     `add_merge_overlapping`, `add_rect`, `merge_smallest_pair`,
     `merge_all_overlapping`, `clear`, `count`, `regions`, `bounding_box`,
     `is_dirty`, `is_empty`, `is_full_damage`, `set_full_damage`,
     `export_to_array`)
   - `InternalDamageTracker` type alias

2. **Update `gfx/src/damage.rs`** imports:
   - `use slopos_abi::damage::{DamageRect, MAX_DAMAGE_REGIONS, MAX_INTERNAL_DAMAGE_REGIONS};`

3. **Update `gfx/src/lib.rs`** to re-export key types for convenience:
   ```rust
   // Re-export DamageTracker types for consumers
   pub use damage::{DamageTracker, InternalDamageTracker};
   ```

4. **Update `abi/src/lib.rs`**: Remove the re-exports of `DamageTracker`,
   `InternalDamageTracker`, `MAX_INTERNAL_DAMAGE_REGIONS` if they appear in the
   `pub use damage::*` line. Keep only `DamageRect` and `MAX_DAMAGE_REGIONS` in the
   re-export.

5. **Update all consumers**. Search for every file that imports `DamageTracker` or
   `InternalDamageTracker`:

   Expected consumers:
   - `video/src/compositor_context.rs` — uses `InternalDamageTracker`
   - `userland/src/gfx/mod.rs` — uses `DamageTracker`
   - `userland/src/compositor.rs` — may use `DamageTracker`
   - `core/src/syscall_services/video.rs` — may use damage types

   For each: change `use slopos_abi::...DamageTracker` to
   `use slopos_gfx::damage::DamageTracker` (or `InternalDamageTracker`).

   Any file that only uses `DamageRect` (no tracker) keeps importing from `slopos_abi`.

6. **Add `slopos-gfx` dependency** to any crate that now needs it and didn't already
   have it. Likely: `core/Cargo.toml` if `core/src/syscall_services/video.rs` uses
   `DamageTracker`. Check each consumer.

7. Run `make build` — must compile. Run `make test` — must pass.

### Files touched
- `abi/src/damage.rs` (trimmed — remove DamageTracker impl)
- `abi/src/lib.rs` (update re-exports)
- `gfx/src/damage.rs` (receives DamageTracker)
- `gfx/src/lib.rs` (add re-exports)
- `video/src/compositor_context.rs` (update imports)
- `userland/src/gfx/mod.rs` (update imports)
- `userland/src/compositor.rs` (update imports if applicable)
- `core/src/syscall_services/video.rs` (update imports if applicable)
- Possibly `core/Cargo.toml` (add gfx dependency)

### Verification
```
grep -r "DamageTracker\|InternalDamageTracker" --include="*.rs" . \
  | grep -v target/ | grep -v gfx/src/damage.rs | grep -v gfx/src/lib.rs
```
All remaining hits must import from `slopos_gfx`, not `slopos_abi`.

---

## Phase 4: Collapse wrapper modules in consumers

**Goal**: Eliminate pure-delegation wrapper functions that existed only because the
algorithms used to live in `abi`. Now that both `video` and `userland` depend on `gfx`
directly, wrappers that add no logic can be removed or simplified.

### 4a: Simplify `video/src/font.rs`

Current state: Functions like `draw_char`, `draw_string`, `draw_str`, `string_width`,
`string_lines` that just forward to `font_render::*` with the same signature.

**Action**: The 1:1 wrappers (`draw_char`, `draw_string`, `draw_str`, `string_width`,
`string_lines`) can be replaced with re-exports:
```rust
pub use slopos_gfx::font_render::{draw_char, draw_string, draw_str, string_width, string_lines};
```

The FFI functions (`font_draw_char_ctx`, `font_draw_string_ctx`) stay — they handle
C-string conversion and framebuffer-ready checks, which is real logic.

### 4b: Simplify `video/src/graphics.rs` free functions

Current state: `draw_pixel`, `fill_rect`, `draw_rect`, `draw_line`, `draw_circle`,
`draw_circle_filled` (lines 197-226) are all one-liners wrapping `gfx::primitives::*`.

**Action**: These are thin convenience wrappers that convert raw RGBA colors via
`pixel_format().convert_color()` before calling the primitives. They serve a purpose
(color conversion), so they stay. But verify each — if any are pure pass-through with
no color conversion, replace with re-export.

### 4c: Simplify `userland/src/gfx/font.rs`

Current state (42 lines): Wrappers that call `font_render::*` then add damage tracking.

**Action**: These wrappers add damage tracking (real logic), so they cannot be eliminated.
But they should now call `slopos_gfx::font_render::*` directly with no intermediate
wrapper. Verify the imports are clean.

### 4d: Simplify `userland/src/gfx/primitives.rs`

Current state (150 lines): Wrappers that call `gfx::primitives::*` then add damage
tracking, plus `blit()` and `scroll_up()`/`scroll_down()` which are userland-specific.

**Action**: Same as 4c — the damage tracking wrappers stay (real logic), but verify
the delegation is now direct to `slopos_gfx::primitives::*` with no unnecessary
indirection layers.

### 4e: Audit all `pub use slopos_abi::*` in consumer crates

After Phases 1-3, the `abi` crate no longer exports `draw_primitives`, `font_render`,
or `DamageTracker`/`InternalDamageTracker`. Verify no consumer has stale imports or
wildcard re-exports that reference removed items.

Run `make build` — must compile. Run `make test` — must pass.

### Files touched
- `video/src/font.rs` (simplify wrappers)
- `video/src/graphics.rs` (verify/simplify)
- `userland/src/gfx/font.rs` (verify imports)
- `userland/src/gfx/primitives.rs` (verify imports)

---

## Phase 5: Consolidate scattered constants

**Goal**: While the abi crate is being cleaned up, address the closely related problem
of duplicated `MAX_*` constants. This phase is independent and can be done in parallel
with or after Phases 1-4.

### 5a: Consolidate `MAX_CPUS`

Currently defined in 3 places:
- `lib/src/percpu.rs:28` — `pub const MAX_CPUS: usize = 256;` (canonical)
- `mm/src/tlb.rs:54` — `pub const MAX_CPUS: usize = 256;` (duplicate)
- `mm/src/page_alloc.rs:71` — `const MAX_CPUS: usize = 256;` (private duplicate)
- `lib/src/pcr.rs:33` — `pub const PCR_MAX_CPUS: usize = 256;` (variant name)

**Action**:
1. Delete the constant from `mm/src/tlb.rs` and `mm/src/page_alloc.rs`.
2. Replace usages with `use slopos_lib::percpu::MAX_CPUS` (or via the re-export
   `slopos_lib::MAX_CPUS`).
3. Change `PCR_MAX_CPUS` in `lib/src/pcr.rs` to `use super::percpu::MAX_CPUS` and
   alias or replace all usages.

### 5b: Consolidate `MAX_PATH_LEN` and `MAX_NAME_LEN`

- `MAX_PATH_LEN`: defined in `fs/src/vfs/mount.rs:5` and `fs/src/fileio.rs:21`
- `MAX_NAME_LEN`: defined in `fs/src/devfs/mod.rs:10` and `fs/src/ramfs/mod.rs:6`

**Action**: Create a constants section in `fs/src/lib.rs` (or `abi/src/fs.rs` if these
are ABI-visible) and have both locations import from there.

### Verification

```
grep -rn "const MAX_CPUS" --include="*.rs" . | grep -v target/
```
Must return exactly one definition (in `lib/src/percpu.rs`).

---

## Verification

After all phases complete:

### Structural checks

1. **`abi` contains no algorithms**: Every `.rs` file in `abi/src/` should contain only:
   - Type definitions (`struct`, `enum`, `type`)
   - Trait definitions (with at most trivial default methods)
   - Constants
   - Pure data (FONT_DATA, syscall numbers)
   - Simple accessor/conversion methods on types

   ```
   # Should find no function bodies longer than ~10 lines in abi:
   grep -c "fn " abi/src/draw_primitives.rs  # File should not exist
   grep -c "fn " abi/src/font_render.rs       # File should not exist
   ```

2. **`gfx` depends only on `abi`**: Check `gfx/Cargo.toml` has exactly one workspace
   dependency.

3. **No stale imports**: `grep -r "slopos_abi::draw_primitives\|slopos_abi::font_render"
   --include="*.rs" . | grep -v target/` returns zero results.

4. **No duplicated `MAX_CPUS`**: Single definition point.

### Functional checks

```bash
make build           # Must compile
make test            # Must pass (all existing tests)
make boot-log        # Must boot and produce expected serial output
```

### Metrics (expected)

| Metric | Before | After |
|--------|--------|-------|
| `abi` crate modules | 20 | 17 (font_render, draw_primitives removed; damage trimmed) |
| `abi` crate lines | ~5,500 | ~5,100 (-400 lines of algorithms) |
| `gfx` crate lines | 0 | ~500 |
| Wrapper boilerplate in video + userland | ~130 lines | ~60 lines |
| `MAX_CPUS` definitions | 4 | 1 |
| Total workspace crates | 12 | 13 |

---

## Non-Goals

- **Do not refactor DrawTarget/PixelBuffer traits**. The trait hierarchy in `abi/src/draw.rs`
  is correct in its current location. Traits are interface contracts, not algorithms.
- **Do not move font DATA**. `FONT_DATA` in `abi/src/font.rs` is a shared static lookup
  table — it belongs in the ABI crate just like syscall number constants do.
- **Do not restructure `video` or `userland` internals** beyond updating imports. This plan
  is scoped to the abi/gfx boundary only.
- **Do not change any public API semantics**. All function signatures remain identical; only
  the crate path changes.

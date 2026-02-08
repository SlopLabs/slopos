# Compositor Hover Registry — Reactive Widget List

> **Status**: Planned
> **Scope**: `userland/src/apps/compositor.rs`
> **Prerequisite for**: [UI_TOOLKIT_DETAILED_PLAN.md](./UI_TOOLKIT_DETAILED_PLAN.md) (this refactor establishes the pattern the toolkit will generalize)

---

## Problem

The compositor has three independent, hand-rolled hover tracking systems that all do the same thing: store previous hover state, compare each frame, emit a damage rect on change. Each system was written at a different time, with different shapes, and the Start button was simply forgotten — causing a visible rendering bug.

### Current hover tracking inventory

| Element | State field | Diff logic | Damage function |
|---------|------------|------------|-----------------|
| Start button | `prev_start_button_hover: bool` | `bool != bool` | `add_start_button_damage()` |
| Start menu items | `prev_start_menu_hover: Option<usize>` | `Option != Option` | `add_start_menu_damage()` |
| Close/minimize buttons | `prev_decoration_hover: DecorationHover` | struct `!=` struct | `add_title_bar_damage()` |
| Taskbar app buttons | **not tracked** | — | — |

Each system:
- Adds 1-2 fields to `WindowManager`
- Adds 5-15 lines of diffing logic in `refresh_windows`
- Adds a dedicated `add_*_damage()` method
- Is easy to forget when adding new interactive elements

### What goes wrong

Adding a new button or interactive region requires changes in 4 separate places:
1. New `prev_*` field on `WindowManager`
2. New initialization in `WindowManager::new()`
3. New diff block in `refresh_windows()`
4. New `add_*_damage()` method

Miss any one of these and the button renders with stale state until a full redraw.

---

## Solution: HoverRegion Registry

Replace all per-element hover tracking with a single fixed-size registry that the compositor populates each frame from the current layout, then auto-diffs against the previous frame.

### Core type

```rust
const MAX_HOVER_REGIONS: usize = 64;

#[derive(Copy, Clone)]
struct HoverRegion {
    id: u32,
    rect: DamageRect,
    hovered: bool,
}

struct HoverRegistry {
    current: [HoverRegion; MAX_HOVER_REGIONS],
    current_count: usize,
    previous: [HoverRegion; MAX_HOVER_REGIONS],
    previous_count: usize,
}
```

### Per-frame lifecycle

```
refresh_windows():
    registry.begin_frame()          // swap current → previous, clear current

    // Registration phase — each draw site registers its region
    registry.register(ID_START_BTN, start_btn_rect, hit_test_start_button())
    for each window:
        registry.register(ID_CLOSE | task_id, close_rect, hit_test_close())
        registry.register(ID_MINIMIZE | task_id, minimize_rect, hit_test_minimize())
    for each menu item:
        registry.register(ID_MENU_ITEM | idx, item_rect, is_hovered)

    // Diff phase — emit damage for any state changes
    for region in registry.diff():
        output_damage.add_rect(region.rect)
```

### ID scheme

Use a simple namespaced u32:

```rust
const HOVER_START_BTN: u32       = 0x0001_0000;
const HOVER_MENU_ITEM_BASE: u32  = 0x0002_0000;  // + item index
const HOVER_CLOSE_BASE: u32      = 0x0003_0000;   // + task_id
const HOVER_MINIMIZE_BASE: u32   = 0x0004_0000;   // + task_id
const HOVER_APP_BTN_BASE: u32    = 0x0005_0000;   // + task_id
```

### What the drawing code sees

The `_clipped` draw functions query the registry for hover state instead of recomputing hit tests:

```rust
// Before (in draw_taskbar_clipped):
let start_hover = self.mouse_x >= start_btn_x && ...;

// After:
let start_hover = self.hover_registry.is_hovered(HOVER_START_BTN);
```

This means hit-testing happens once per frame in `refresh_windows`, and drawing just reads the result. Currently hit-testing is duplicated: once in `refresh_windows` for damage, once in the draw functions for color selection.

---

## Migration Plan

### Step 1: Add `HoverRegistry` type

Add `HoverRegion`, `HoverRegistry` to `compositor.rs`. The registry needs:
- `begin_frame()` — rotates current to previous, resets current count
- `register(id, rect, hovered)` — adds a region to current frame
- `changed_regions()` — iterator over regions whose hover state differs from previous frame (by matching on `id`). Returns the damage rect for each changed region. Also returns damage rects for regions that existed in previous but not in current (element removed) and vice versa (element added).
- `is_hovered(id) -> bool` — lookup current hover state by id

All fixed-size arrays, no allocations, `no_std` compatible.

### Step 2: Register all existing interactive elements

In `refresh_windows`, after the existing window/bounds logic, replace the three hover tracking blocks with registry calls:

**Replace** `prev_start_button_hover` tracking:
```rust
// Before:
if hover != self.prev_start_button_hover {
    self.add_start_button_damage();
    self.prev_start_button_hover = hover;
}

// After:
self.hover_registry.register(HOVER_START_BTN, start_btn_rect, self.hit_test_start_button(fb_h));
```

**Replace** `prev_start_menu_hover` tracking:
```rust
// Before:
if current_hover != self.prev_start_menu_hover { ... }

// After:
for (idx, item) in START_MENU_ITEMS.iter().enumerate() {
    let item_rect = ...;
    let hovered = self.hit_test_start_menu_item(fb_h) == Some(idx);
    self.hover_registry.register(HOVER_MENU_ITEM_BASE | idx as u32, item_rect, hovered);
}
```

**Replace** `prev_decoration_hover` / `DecorationHover` tracking:
```rust
// Before:
let mut current_deco_hover = DecorationHover::default();
// ... 30 lines of manual diffing ...

// After:
for i in 0..self.window_count as usize {
    let window = self.windows[i];
    let close_rect = ...;
    let min_rect = ...;
    self.hover_registry.register(
        HOVER_CLOSE_BASE | window.task_id,
        close_rect,
        self.hit_test_close_button(&window),
    );
    self.hover_registry.register(
        HOVER_MINIMIZE_BASE | window.task_id,
        min_rect,
        self.hit_test_minimize_button(&window),
    );
}
```

**Add missing** taskbar app button hover (currently not tracked at all):
```rust
for i in 0..self.window_count as usize {
    let window = self.windows[i];
    let btn_rect = ...;
    let hovered = hit_test_taskbar_app_button(mouse, btn_rect);
    self.hover_registry.register(HOVER_APP_BTN_BASE | window.task_id, btn_rect, hovered);
}
```

Then emit damage from the diff:
```rust
for rect in self.hover_registry.changed_regions() {
    self.output_damage.add_rect(rect.x0, rect.y0, rect.x1, rect.y1);
}
```

### Step 3: Update draw functions to read from registry

Replace inline hit-test recomputation in draw functions:

| Draw function | Current | After |
|--------------|---------|-------|
| `draw_taskbar_clipped` | `self.mouse_x >= start_btn_x && ...` | `self.hover_registry.is_hovered(HOVER_START_BTN)` |
| `draw_start_menu_clipped` | `self.hit_test_start_menu_item(fb_height) == Some(idx)` | `self.hover_registry.is_hovered(HOVER_MENU_ITEM_BASE \| idx)` |
| `draw_title_bar_clipped` | `self.hit_test_close_button(window)` | `self.hover_registry.is_hovered(HOVER_CLOSE_BASE \| window.task_id)` |
| `draw_title_bar_clipped` | `self.hit_test_minimize_button(window)` | `self.hover_registry.is_hovered(HOVER_MINIMIZE_BASE \| window.task_id)` |

### Step 4: Remove legacy tracking

Delete from `WindowManager`:
- `prev_start_button_hover: bool`
- `prev_start_menu_hover: Option<usize>`
- `prev_decoration_hover: DecorationHover`

Delete from the codebase:
- `struct DecorationHover`
- `fn add_start_button_damage()`
- `fn add_start_menu_damage()` (keep `add_taskbar_damage` — it covers window count changes, not hover)
- All hover diff blocks in `refresh_windows()` (replaced by `changed_regions()` loop)

### Step 5: Add taskbar app button hover

With the registry in place, this is a single `register()` call per button — no new fields, no new diff logic, no new damage function. This demonstrates the value of the system: adding hover to a new element is one line.

---

## Verification

- `make build` — zero warnings
- `make boot VIDEO=1` — manual test: hover Start button, start menu items, close/minimize buttons, taskbar app buttons. All should show immediate hover feedback with no stale artifacts.
- `make test` — existing test harness passes

---

## Relationship to UI Toolkit Plan

This refactor is a stepping stone toward [UI_TOOLKIT_DETAILED_PLAN.md](./UI_TOOLKIT_DETAILED_PLAN.md). The hover registry establishes the core pattern (declare interactive regions → auto-diff → auto-damage) that the full widget system will generalize to all widget state, not just hover. When the toolkit lands, the `HoverRegistry` gets absorbed into the widget tree's event dispatch system.

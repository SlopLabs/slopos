//! The system bar's popover: compositor-owned chrome anchored to a status item.
//!
//! Drawn by the compositor rather than by a client: it renders state the
//! compositor already holds, its light-dismiss pointer grab has to live here
//! whichever process draws the pixels, and it must stay available when
//! `ProtocolBridge::new()` returns `None`. Placement comes from
//! [`slopos_chrome_core::positioner`], not arithmetic here.

use core::fmt::Write;
use std::string::String;

use slopos_appkit::text::ui;
use slopos_chrome_core::netstate::{
    GATEWAY_LABEL, IFACE_NO_CARRIER_LABEL, IFACE_OFF_LABEL, IFACE_SEPARATOR, IfaceState,
    NO_ADDRESS_LABEL, NetPanelModel, PANEL_TITLE, iface_state, indicator_label,
    indicator_state_for,
};
use slopos_chrome_core::positioner::{Positioner, Rect, Size, position};
use slopos_chrome_core::status::StatusKind;
use slopos_chrome_core::toggle::{TOGGLE_OFF, TOGGLE_ON, toggle_geometry};
use slopos_gfx::canvas_ops::rounded_rect_filled;
use slopos_net_core::Ipv4;
use slopos_net_core::render::iface_kind;
use slopos_theme_core::{radius, space};

use crate::gfx::{DamageRect, DrawBuffer};
use crate::syscall::process;
use crate::theme::*;
use std::time::Instant;

/// Width of the network panel. The widest line the model can produce fits the
/// 280 px budget from the second rail; anything longer is cut short by
/// [`draw_fitted`] rather than run under the panel edge.
const PANEL_WIDTH: i32 = 320;

/// Padding inside the panel, on all four edges equally.
const PANEL_PAD: i32 = space::MD;

/// Diameter of the per-interface status dot. Ink, not a gap.
const DOT_D: i32 = space::SM;

/// The step from the panel's own rail to the interface rail. One indivisible
/// rail step, not `DOT_D + something`: the dot merely happens to live in the
/// first half of it.
const DOT_GUTTER: i32 = space::LG;

/// The panel's one hairline weight, for its edge and for the header rule.
const RULE_H: i32 = 1;

const PANEL_RADIUS: i32 = radius::CARD;

/// Gap between the bar's border row and the top of the panel.
const PANEL_GAP_Y: i32 = space::SM;

/// The switch's track. Ink, and at the hit-target minimum.
const SWITCH_W: i32 = 10 * space::XS;
const SWITCH_H: i32 = 5 * space::XS;

/// One line box: the text's line plus a little air, so a menu row is a
/// comfortable target.
fn line_h() -> i32 {
    ui::line_height(TEXT_PX) + space::XS
}

/// Text centred in the line box whose top is `y`.
fn draw_text(
    buf: &mut DrawBuffer,
    x: i32,
    y: i32,
    s: &str,
    fg: slopos_abi::draw::Color32,
    clip: &DamageRect,
) {
    ui::draw_clipped(
        buf,
        x,
        ui::centred_top(y, line_h(), TEXT_PX),
        s,
        TEXT_PX,
        ui::Weight::Regular,
        fg,
        CARD_BG,
        clip,
    );
}

/// Where every element sits, computed once: a `header_h`/`row_h`/`footer_h`
/// trio would leave the rule's position to two expressions that must agree by
/// hand.
struct Layout {
    height: i32,
    title_y: i32,
    status_y: i32,
    switch_y: i32,
    rule_y: i32,
    has_rule: bool,
    rows_y: i32,
    row_pitch: i32,
    gateway_y: i32,
    has_gateway: bool,
}

fn layout(model: &NetPanelModel) -> Layout {
    let lh = line_h();
    let n = model.listed_count() as i32;
    let gw = model.gateway != [0; 4];

    let title_y = PANEL_PAD;
    let status_y = title_y + lh;
    // Centred on the title's line, not the two-line header: the switch acts on
    // the panel, and the title is the panel's own line.
    let switch_y = title_y + ((lh - SWITCH_H) / 2).max(0);
    let header_end = title_y + 2 * lh;

    // A rule with nothing beneath it is a seam, not a division.
    let has_rule = n >= 1 || gw;
    let rule_y = header_end + space::MD;
    let body_y = if has_rule {
        rule_y + RULE_H + space::MD
    } else {
        header_end
    };

    let row_pitch = 2 * lh + space::XS;
    let rows_y = body_y;
    let rows_end = if n >= 1 {
        rows_y + n * 2 * lh + (n - 1) * space::XS
    } else {
        rows_y
    };
    let gateway_y = if n >= 1 {
        rows_end + space::MD
    } else {
        rows_end
    };
    let content_end = if gw { gateway_y + lh } else { rows_end };

    Layout {
        height: content_end + PANEL_PAD,
        title_y,
        status_y,
        switch_y,
        rule_y,
        has_rule,
        rows_y,
        row_pitch,
        gateway_y,
        has_gateway: gw,
    }
}

/// [`draw_text`] of `s` cut short with [`ui::ELLIPSIS`] to fit `max_w`.
fn draw_fitted(
    buf: &mut DrawBuffer,
    x: i32,
    y: i32,
    s: &str,
    max_w: i32,
    fg: slopos_abi::draw::Color32,
    clip: &DamageRect,
) {
    ui::draw_elided(
        buf,
        x,
        ui::centred_top(y, line_h(), TEXT_PX),
        s,
        TEXT_PX,
        ui::Weight::Regular,
        max_w,
        fg,
        CARD_BG,
        clip,
    );
}

/// The dot's colour for each row state; every one clears 3:1 against the panel,
/// the WCAG floor for informational non-text.
fn dot_colour(state: IfaceState) -> slopos_abi::draw::Color32 {
    match state {
        IfaceState::Off => TEXT_SECONDARY,
        IfaceState::NoCarrier => DANGER,
        IfaceState::NoAddress => WARNING,
        IfaceState::Up => SUCCESS,
    }
}

/// How long a requested switch change may stay unconfirmed. The control shows
/// what was asked for immediately and settles when the kernel's own event
/// agrees; at expiry it reverts to whatever the kernel actually says, so a lost
/// completion cannot wedge the control in a busy state forever.
const SWITCH_SETTLE_MS: u128 = 10_000;

/// How long to wait for the kernel to agree before asking again. `ip net on|off`
/// is idempotent, and re-issuing is what makes the control converge: two presses
/// spawn two children whose execution nothing orders, so the pair can land in
/// the wrong order and leave the kernel in the state asked for *first*.
const SWITCH_RETRY_MS: u128 = 750;

/// What a popover shows: the system menu under the bar's icon, or a status
/// item's panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panel {
    System,
    Status(StatusKind),
}

const NETWORK: Panel = Panel::Status(StatusKind::Network);

/// Above the entries while a new base waits on a restart: the compositor and
/// the terminal run from the base they started from until the session
/// restarts.
const RESTART_NOTE: &str = "A new base changes the desktop";

/// What the system panel offers, in its order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Lock,
    SwitchPerson,
    LogOut,
    Restart,
}

impl Choice {
    const ALL: [Choice; 4] = [
        Choice::Lock,
        Choice::SwitchPerson,
        Choice::LogOut,
        Choice::Restart,
    ];

    fn label(self) -> &'static str {
        match self {
            Choice::Lock => "Lock",
            Choice::SwitchPerson => "Switch person",
            Choice::LogOut => "Log out",
            Choice::Restart => "Restart session",
        }
    }

    /// What init is asked for it; locking is the compositor's own.
    pub fn command(self) -> crate::init_control::Command {
        use crate::init_control::Command;
        match self {
            Choice::Lock => Command::Lock,
            Choice::SwitchPerson => Command::Greeter,
            Choice::LogOut => Command::EndSession,
            Choice::Restart => Command::RestartSession,
        }
    }
}

pub struct Popover {
    open: Option<Panel>,
    rect: Rect,
    /// Where it was last frame, so closing or moving repaints what it vacated.
    prev_rect: Rect,
    /// What the switch was last asked for, while the kernel has yet to agree.
    pending: Option<bool>,
    /// The switch changed appearance without the panel moving, which the rect
    /// comparison alone would not ask a repaint for.
    switch_dirty: bool,
    /// When the target was last changed, for [`SWITCH_SETTLE_MS`].
    pending_since: Option<Instant>,
    /// When a command was last spawned, for [`SWITCH_RETRY_MS`].
    issued_at: Option<Instant>,
    /// Composition buffer for a line being built.
    line: String,
    /// The item the panel was anchored under, kept so a resize re-anchors to
    /// the same place instead of to a synthesised rect one pixel off.
    anchor: Rect,
    /// A new base changed the session's programs; the system panel says so
    /// until the session restarts.
    restart_wanted: bool,
    /// The system panel opened on its own; until then a press on it was
    /// meant for what was under the pointer before.
    restart_armed_at: Option<Instant>,
    /// The networking switch is shown: the person may move it unasked.
    switch_offered: bool,
    /// What the person chose in the system panel, for the frame to act on.
    choice: Option<Choice>,
}

impl Popover {
    pub fn new(switch_offered: bool) -> Self {
        Self {
            open: None,
            rect: Rect::EMPTY,
            prev_rect: Rect::EMPTY,
            pending: None,
            pending_since: None,
            issued_at: None,
            switch_dirty: false,
            line: String::new(),
            anchor: Rect::EMPTY,
            restart_wanted: false,
            restart_armed_at: None,
            switch_offered,
            choice: None,
        }
    }

    pub fn take_choice(&mut self) -> Option<Choice> {
        self.choice.take()
    }

    /// Whether the system panel notes that the session wants a restart;
    /// `true` when it did not before.
    pub fn want_restart(&mut self, wanted: bool) -> bool {
        let newly = wanted && !self.restart_wanted;
        self.switch_dirty |= wanted != self.restart_wanted;
        self.restart_wanted = wanted;
        newly
    }

    fn system_lines(&self) -> i32 {
        Choice::ALL.len() as i32 + i32::from(self.restart_wanted)
    }

    /// Where the system panel's entries start.
    fn choices_y(&self, rect: Rect) -> i32 {
        rect.y + PANEL_PAD + i32::from(self.restart_wanted) * line_h()
    }

    /// The popover's rect while it is open; `None` when closed, which
    /// `resolve_cursor_hit` reads as "no grab".
    pub fn rect(&self) -> Option<Rect> {
        if self.open.is_some() && !self.rect.is_empty() {
            Some(self.rect)
        } else {
            None
        }
    }

    /// Open the popover for `panel` under `item`, or close it if that panel
    /// is already the open one.
    pub fn toggle(
        &mut self,
        panel: Panel,
        item: DamageRect,
        model: &NetPanelModel,
        screen_w: u32,
        screen_h: u32,
    ) {
        if self.open == Some(panel) {
            self.dismiss();
            return;
        }
        self.restart_armed_at = None;
        self.open(panel, item, model, screen_w, screen_h);
    }

    /// Open the system panel under `item` unasked, to offer the restart a
    /// new base wants.
    pub fn offer_restart(
        &mut self,
        item: DamageRect,
        model: &NetPanelModel,
        screen_w: u32,
        screen_h: u32,
    ) {
        self.open(Panel::System, item, model, screen_w, screen_h);
        self.restart_armed_at = Some(
            Instant::now() + std::time::Duration::from_millis(slopos_priv_core::consent::ARMING_MS),
        );
    }

    /// Open the popover for `panel` under `item`, whatever is open now.
    pub fn open(
        &mut self,
        panel: Panel,
        item: DamageRect,
        model: &NetPanelModel,
        screen_w: u32,
        screen_h: u32,
    ) {
        let anchor = Rect::new(
            item.x0,
            item.y0,
            item.x1 - item.x0 + 1,
            item.y1 - item.y0 + 1,
        );
        self.anchor = anchor;
        // Sized from the model at open, so the panel does not appear at header
        // height and resize on the next frame.
        let size = match panel {
            NETWORK => Size {
                w: PANEL_WIDTH,
                h: layout(model).height,
            },
            Panel::System => {
                let note = if self.restart_wanted {
                    RESTART_NOTE
                } else {
                    ""
                };
                let text_w = Choice::ALL
                    .iter()
                    .map(|choice| ui::width(choice.label(), TEXT_PX))
                    .fold(ui::width(note, TEXT_PX), i32::max);
                Size {
                    w: text_w + 2 * PANEL_PAD,
                    h: self.system_lines() * line_h() + 2 * PANEL_PAD,
                }
            }
            Panel::Status(_) => Size {
                w: PANEL_WIDTH,
                h: 2 * PANEL_PAD + 2 * line_h(),
            },
        };
        let work_area = work_area(screen_w, screen_h);
        self.rect = position(
            &Positioner::below_bar_item(anchor, size, PANEL_GAP_Y),
            work_area,
        );
        self.open = Some(panel);
    }

    /// Close the popover. Idempotent: a dismiss with nothing open is what a
    /// click on the desktop looks like, and must not manufacture damage.
    pub fn dismiss(&mut self) {
        if self.open.is_some() {
            self.open = None;
        }
    }

    /// Resize the open popover to fit `model`, so the panel is as tall as its
    /// content rather than a guess made at open.
    pub fn fit_to(&mut self, model: &NetPanelModel, screen_w: u32, screen_h: u32) {
        if self.open != Some(NETWORK) || self.rect.is_empty() {
            return;
        }
        let wanted = layout(model).height;
        if wanted == self.rect.h {
            return;
        }
        self.rect = position(
            &Positioner::below_bar_item(
                self.anchor,
                Size {
                    w: self.rect.w,
                    h: wanted,
                },
                PANEL_GAP_Y,
            ),
            work_area(screen_w, screen_h),
        );
    }

    /// The switch's track, shared by the draw and the hit test: two copies of
    /// this arithmetic is how a control comes to ignore a press that visibly
    /// lands on it.
    fn switch_track(rect: Rect, l: &Layout) -> Rect {
        Rect::new(
            rect.right() - PANEL_PAD - SWITCH_W,
            rect.y + l.switch_y,
            SWITCH_W,
            SWITCH_H,
        )
    }

    /// The switch's press target: the track, padded to the header band. Separate
    /// from the draw geometry because a 20 px control with zero hit padding is a
    /// control people miss.
    fn switch_hit(rect: Rect, l: &Layout) -> Rect {
        let track = Self::switch_track(rect, l);
        Rect::new(
            track.x - space::SM,
            rect.y + PANEL_PAD,
            track.w + space::SM + PANEL_PAD,
            line_h(),
        )
    }

    /// The position the switch should draw in: what was asked for while a
    /// request is outstanding, otherwise what the kernel says.
    fn switch_on(&self, model: &NetPanelModel) -> bool {
        self.pending.unwrap_or(model.enabled)
    }

    /// Settle or abandon an outstanding request. Called every frame; the kernel
    /// agreeing is the normal exit, the deadline the one that matters.
    pub fn settle(&mut self, model: &NetPanelModel) {
        let Some(wanted) = self.pending else {
            return;
        };
        if model.enabled == wanted {
            self.clear_pending();
            return;
        }
        if self
            .pending_since
            .is_none_or(|since| since.elapsed().as_millis() >= SWITCH_SETTLE_MS)
        {
            // Given up on: a wrong switch position is worse than a refused one.
            self.clear_pending();
            return;
        }
        if self
            .issued_at
            .is_none_or(|at| at.elapsed().as_millis() >= SWITCH_RETRY_MS)
        {
            self.issue(wanted);
        }
    }

    fn issue(&mut self, wanted: bool) {
        if spawn_ip_net(wanted) {
            self.issued_at = Some(Instant::now());
        }
    }

    fn clear_pending(&mut self) {
        self.pending = None;
        self.pending_since = None;
        self.issued_at = None;
        self.switch_dirty = true;
    }

    /// Route a press inside the panel. Returns whether anything took it; the
    /// mutation itself is spawned, see [`spawn_ip_net`].
    pub fn press(&mut self, x: i32, y: i32, model: &NetPanelModel) -> bool {
        if self.open == Some(Panel::System) {
            let Some(rect) = self.rect().filter(|rect| rect.contains(x, y)) else {
                return false;
            };
            let armed = self.restart_armed_at.is_none_or(|at| Instant::now() >= at);
            let row = (y - self.choices_y(rect)).div_euclid(line_h().max(1));
            let choice = usize::try_from(row)
                .ok()
                .and_then(|row| Choice::ALL.get(row).copied());
            if armed && let Some(choice) = choice {
                self.choice = Some(choice);
                self.dismiss();
            }
            return true;
        }
        // Only the network panel has controls; without this a click on another
        // kind's popover — which draws nothing — reports "taken" and eats it.
        if self.open != Some(NETWORK) {
            return false;
        }
        let Some(rect) = self.rect() else {
            return false;
        };
        if !self.switch_offered || !Self::switch_hit(rect, &layout(model)).contains(x, y) {
            return false;
        }
        // Toggle from what the switch is showing, not from what the kernel last
        // said: during the busy window those differ, and reading the kernel value
        // would re-request the state already being asked for.
        let wanted = !self.switch_on(model);
        if self.pending == Some(wanted) {
            return true;
        }
        // A press during an outstanding request retargets it rather than being
        // dropped, which would silently lose the intent.
        self.pending = Some(wanted);
        self.pending_since = Some(Instant::now());
        self.switch_dirty = true;
        self.issue(wanted);
        true
    }

    /// Rects to repaint: whatever the popover covered last frame and whatever it
    /// covers now, emitted only when the two differ.
    pub fn take_damage(&mut self, out: &mut [DamageRect]) -> usize {
        let current = self.rect().unwrap_or(Rect::EMPTY);
        if current == self.prev_rect && !self.switch_dirty {
            return 0;
        }
        self.switch_dirty = false;
        let mut count = 0usize;
        for rect in [self.prev_rect, current] {
            if !rect.is_empty() && count < out.len() {
                out[count] = DamageRect {
                    x0: rect.x,
                    y0: rect.y,
                    x1: rect.right() - 1,
                    y1: rect.bottom() - 1,
                };
                count += 1;
            }
        }
        self.prev_rect = current;
        count
    }

    pub fn draw(&mut self, buf: &mut DrawBuffer, model: &NetPanelModel, clip: &DamageRect) {
        let Some(rect) = self.rect() else {
            return;
        };
        if self.open == Some(Panel::System) {
            let choices_y = self.choices_y(rect);
            let note = self.restart_wanted;
            buf.with_scissor(*clip, |buf| {
                draw_frame(buf, rect);
                if note {
                    draw_text(
                        buf,
                        rect.x + PANEL_PAD,
                        rect.y + PANEL_PAD,
                        RESTART_NOTE,
                        TEXT_SECONDARY,
                        clip,
                    );
                }
                for (i, choice) in Choice::ALL.iter().enumerate() {
                    draw_text(
                        buf,
                        rect.x + PANEL_PAD,
                        choices_y + i as i32 * line_h(),
                        choice.label(),
                        TEXT_PRIMARY,
                        clip,
                    );
                }
            });
            return;
        }
        if self.open != Some(NETWORK) {
            return;
        }

        let on = self.switch_on(model);
        let busy = self.pending.is_some();
        let offered = self.switch_offered;
        let l = layout(model);
        // Split borrow: the closure composes into `line`, so it cannot hold
        // `&self`.
        let Self { line, .. } = self;

        // rail1 is where the panel speaks about itself, rail2 about an interface.
        let rail1 = rect.x + PANEL_PAD;
        let rail2 = rail1 + DOT_GUTTER;
        let content_w = rect.w - 2 * PANEL_PAD;
        let header_text_w = content_w - SWITCH_W - space::MD;
        let row_text_w = content_w - DOT_GUTTER;
        let gw_value_x = rail1 + ui::width(GATEWAY_LABEL, TEXT_PX) + space::MD;
        let gw_value_w = rect.x + rect.w - PANEL_PAD - gw_value_x;

        buf.with_scissor(*clip, |buf| {
            draw_frame(buf, rect);

            // The colour rule, panel-wide: TEXT_PRIMARY names the thing,
            // TEXT_SECONDARY describes it. With one text size and one weight,
            // colour is the only channel that can say "this matters more".
            // The title is the dimmer of the two; the sentence underneath is
            // what someone who clicked the network indicator came for.
            draw_fitted(
                buf,
                rail1,
                rect.y + l.title_y,
                PANEL_TITLE,
                header_text_w,
                TEXT_SECONDARY,
                clip,
            );
            draw_fitted(
                buf,
                rail1,
                rect.y + l.status_y,
                indicator_label(indicator_state_for(model)),
                header_text_w,
                TEXT_PRIMARY,
                clip,
            );

            // Busy while a request is outstanding, so the control says "asked,
            // not yet confirmed" rather than showing a position the kernel has
            // not agreed to.
            let track = Self::switch_track(rect, &l);
            // Busy keeps the target's own track colour and changes only the
            // knob: a grey track under an on-position knob reads as a disabled
            // control rather than a moving one.
            let track_colour = if on { ACCENT } else { SWITCH_OFF };
            if offered {
                rounded_rect_filled(
                    buf,
                    track.x,
                    track.y,
                    track.w,
                    track.h,
                    track.h / 2,
                    track_colour,
                );
            }
            let geometry = toggle_geometry(track, if on { TOGGLE_ON } else { TOGGLE_OFF });
            let knob_colour = match (busy, on) {
                (false, _) => TEXT_PRIMARY,
                (true, true) => TEXT_ON_ACCENT,
                (true, false) => TEXT_SECONDARY,
            };
            if offered && !geometry.knob.is_empty() {
                rounded_rect_filled(
                    buf,
                    geometry.knob.x,
                    geometry.knob.y,
                    geometry.knob.w,
                    geometry.knob.h,
                    geometry.knob.w / 2,
                    knob_colour,
                );
            }

            // Inset to the text rail rather than full-bleed: a rule that reaches
            // the panel's edges runs into the corner arcs and reads as a window
            // seam instead of a group division.
            if l.has_rule {
                rounded_rect_filled(buf, rail1, rect.y + l.rule_y, content_w, RULE_H, 0, DIVIDER);
            }

            // State is a coloured dot and a plain word, never an RFC 2863
            // spelling: `UNKNOWN` and `LOWERLAYERDOWN` belong in `ip link`, and
            // in a status menu they ask a question instead of answering one.
            for (i, iface) in model.listed_ifaces().enumerate() {
                let row_top = rect.y + l.rows_y + i as i32 * l.row_pitch;
                let state = iface_state(iface);

                // Centred on the name's line, which puts the disc's centre on the
                // cap band's optical centre for this font.
                rounded_rect_filled(
                    buf,
                    rail1,
                    row_top + (line_h() - DOT_D) / 2,
                    DOT_D,
                    DOT_D,
                    DOT_D / 2,
                    dot_colour(state),
                );

                draw_fitted(
                    buf,
                    rail2,
                    row_top,
                    iface.name_str(),
                    row_text_w,
                    TEXT_PRIMARY,
                    clip,
                );

                line.clear();
                let _ = write!(line, "{}{}", iface_kind(iface.kind.abi()), IFACE_SEPARATOR);
                match state {
                    IfaceState::Off => line.push_str(IFACE_OFF_LABEL),
                    IfaceState::NoCarrier => line.push_str(IFACE_NO_CARRIER_LABEL),
                    IfaceState::NoAddress => line.push_str(NO_ADDRESS_LABEL),
                    IfaceState::Up => {
                        let _ = write!(line, "{}/{}", Ipv4(iface.ipv4), iface.prefix_len);
                    }
                }
                // Never dimmed by state: a disabled grey measures 2.5:1 on
                // exactly the line someone opens this panel to read when
                // something is wrong. The dot's hue carries the fault instead.
                draw_fitted(
                    buf,
                    rail2,
                    row_top + line_h(),
                    line,
                    row_text_w,
                    TEXT_SECONDARY,
                    clip,
                );
            }

            // At rail1, not rail2: the gateway is not an interface, and a
            // permanently empty dot gutter reads as an orphaned row.
            if l.has_gateway {
                let y = rect.y + l.gateway_y;
                draw_text(buf, rail1, y, GATEWAY_LABEL, TEXT_SECONDARY, clip);
                line.clear();
                let _ = write!(line, "{}", Ipv4(model.gateway));
                // A fixed key column, not a right rail: right-aligning would open
                // a trough that changes width with the address.
                draw_fitted(buf, gw_value_x, y, line, gw_value_w, TEXT_PRIMARY, clip);
            }
        });
    }
}

/// Two concentric opaque fills, not fill-then-outline: a stroked rounded rect
/// blends its corner arcs against the fill's own antialiased boundary and
/// fades out across each corner.
fn draw_frame(buf: &mut DrawBuffer, rect: Rect) {
    rounded_rect_filled(buf, rect.x, rect.y, rect.w, rect.h, PANEL_RADIUS, CARD_EDGE);
    rounded_rect_filled(
        buf,
        rect.x + RULE_H,
        rect.y + RULE_H,
        rect.w - 2 * RULE_H,
        rect.h - 2 * RULE_H,
        PANEL_RADIUS - RULE_H,
        CARD_BG,
    );
}

/// The region a popover may occupy: the screen below the bar, inset on the other
/// three sides. No inset on top — the gap below the bar is the positioner's
/// offset, and insetting here as well would count it twice.
fn work_area(screen_w: u32, screen_h: u32) -> Rect {
    let top = SYSTEM_BAR_HEIGHT + 1;
    Rect::new(
        PANEL_PAD,
        top,
        screen_w as i32 - 2 * PANEL_PAD,
        screen_h as i32 - top - PANEL_PAD,
    )
}

/// Ask `/bin/ip` to move the master networking switch, through a privd
/// connection this process makes: the request is attested as the
/// compositor's, which the policy lets turn networking on and off without
/// asking. The compositor itself never holds `NetAdmin`, which would widen
/// the process that already owns the framebuffer and every input event.
///
/// Returns whether the child started; the operation's own result arrives later
/// as a `net_monitor` event.
fn spawn_ip_net(enable: bool) -> bool {
    let Ok(privd) = crate::privd::Connection::open() else {
        return false;
    };
    let verb: &[u8] = if enable { b"on\0" } else { b"off\0" };
    let argv: [*const u8; 3] = [b"ip\0".as_ptr(), b"net\0".as_ptr(), verb.as_ptr()];
    let env = format!("{}={PRIVD_AT}\0", slopos_priv_core::ENV_FD);
    let envp = [env.as_ptr()];
    // stdout and stderr only: `ip` reads nothing, and handing it the
    // compositor's stdin would put a child on the console's input.
    let actions = [
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
        process::clone_fd(privd.raw(), PRIVD_AT),
    ];
    let tid = process::spawn_path_with_env(
        b"/bin/ip",
        &argv,
        &envp,
        slopos_abi::task::TaskPriority::Normal,
        slopos_abi::task::TASK_FLAG_USER_MODE,
        &actions,
        0,
    );
    tid > 0
}

/// Where `ip` finds the compositor's privd connection.
const PRIVD_AT: i32 = 3;

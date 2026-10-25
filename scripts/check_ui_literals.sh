#!/usr/bin/env bash
# Fail the build when an app draws with a raw colour, a font size, or a
# number for spacing or an inset, instead of the theme's roles and tokens
# (theme-core, plans/ui-overhaul.md).
#
# Scope:
#   - every app: each file under apps/, and each module of userland/src/apps/
#     one of whose files implements appkit's `App` or calls `run_app`. All
#     four checks.
#   - appkit itself, its tests aside: constructors only, since its widgets
#     are where the roles become pixels and their geometry is the toolkit's.
#
# Checks:
#   1 colour     a `Color32` constructor (`::rgb`, `::new`, `::WHITE`,
#                `::BLACK`, the tuple) or alias, an `EncodedPixel`; in an app
#                also a colour derived by `with_alpha` or `mix`, or a theme
#                table named (`DARK`) rather than `theme::current()`.
#                `Color32::TRANSPARENT` is no colour.
#   2 font-size  text sized by hand: a `text::ui` sizing call or its import,
#                a `font_size` or `size_px`, a `TextStyle` literal.
#   3 spacing    a stack's `spacing:` or a spacer's `Length::Px` holding an
#                integer literal other than 0.
#   4 inset      `EdgeInsets::{all,symmetric,new}` whose arguments, over as
#                many lines as the call takes, hold an integer literal other
#                than 0, or an `EdgeInsets { .. }` literal.
#   5 allowlist  an entry in scripts/gates/ui-literals.txt that matches no
#                finding: a dead exemption.
#
# It reads spellings, not values: a number reached through a constant or a
# helper's parameter passes. A file whose colours are its content (a syntax
# palette, a checkerboard) is exempt from one check by a line in the
# allowlist, with its reason.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_ui_literals "$@"

ALLOWLIST="$REPO_ROOT/scripts/gates/ui-literals.txt"
APP_MARK='impl(<[^>]*>)?[[:space:]]+([A-Za-z_]+::)*App(<[^>]*>)?[[:space:]]+for|run_app[[:space:]]*\('

# `<scope>\t<path>` for each file in scope: `app` or `kit`.
classify() {
    local root="$1" list="$2" path name apps
    apps="$(
        cd "$root"
        printf '%s\n' "$list" | grep '^userland/src/apps/' | while IFS= read -r path; do
            [ -f "$path" ] || continue
            if grep -Eq "$APP_MARK" "$path"; then
                name="${path#userland/src/apps/}"
                name="${name%%/*}"
                printf '%s\n' "${name%.rs}"
            fi
        done | LC_ALL=C sort -u
    )"
    printf '%s\n' "$list" | while IFS= read -r path; do
        case "$path" in
            apps/*) printf 'app\t%s\n' "$path" ;;
            appkit/src/tests.rs | appkit/src/tests/*) ;;
            appkit/src/*) printf 'kit\t%s\n' "$path" ;;
            userland/src/apps/*)
                name="${path#userland/src/apps/}"
                name="${name%%/*}"
                name="${name%.rs}"
                if printf '%s\n' "$apps" | grep -qxF "$name"; then
                    printf 'app\t%s\n' "$path"
                fi
                ;;
        esac
    done
}

# Findings as `<tag>\t<check>\t<path>:<line>: <text>`.
scan_sources() {
    local root="$1" classified="$2"
    cd "$root"
    printf '%s\n' "$classified" | while IFS="	" read -r scope file; do
        [ -n "$file" ] && [ -f "$file" ] || continue
        awk -v fname="$file" -v scope="$scope" '
            function hit(tag, check, at, what) {
                printf "%s\t%s\t%s:%d: %s\n", tag, check, fname, at, what
            }
            # Whether `s` holds an integer literal, decimal or hex, that is
            # not zero.
            function nonzero(s,   lit) {
                while (match(s, /(^|[^A-Za-z0-9_.])(0[xX][0-9A-Fa-f_]+|[0-9][0-9_]*)/)) {
                    lit = substr(s, RSTART, RLENGTH)
                    s = substr(s, RSTART + RLENGTH)
                    sub(/^[^0-9]/, "", lit)
                    sub(/^0[xX]/, "", lit)
                    gsub(/[_0]/, "", lit)
                    if (lit != "") return 1
                }
                return 0
            }
            # Feeds `s` to the open call until its parentheses close, and
            # answers what of `s` is left after it.
            function feed(s,   i, c) {
                for (i = 1; i <= length(s) && depth > 0; i++) {
                    c = substr(s, i, 1)
                    if (c == "(") depth++
                    else if (c == ")") depth--
                    if (depth > 0) args = args c
                }
                if (depth == 0) {
                    if (nonzero(args)) hit(4, "inset", call_at, call_text)
                    calling = 0
                }
                return substr(s, i)
            }
            # `s` with its comments gone and what its string and char
            # literals hold blanked, an open block comment or string carried
            # onto the next line. Block comments nest, as in Rust.
            function code(s,   out, i, j, k, n, c, d) {
                out = ""
                n = length(s)
                for (i = 1; i <= n; i++) {
                    c = substr(s, i, 1)
                    d = substr(s, i + 1, 1)
                    if (inblock) {
                        if (c == "/" && d == "*") { inblock++; i++ }
                        else if (c == "*" && d == "/") { inblock--; i++ }
                        continue
                    }
                    if (raw >= 0) {
                        if (c == "\"" && substr(s, i + 1, raw) == hashes(raw)) {
                            i += raw
                            raw = -1
                            out = out c
                        }
                        continue
                    }
                    if (instr) {
                        if (c == "\\") { i++; continue }
                        if (c == "\"") { instr = 0; out = out c }
                        continue
                    }
                    if (c == "/" && d == "/") break
                    if (c == "/" && d == "*") { inblock = 1; i++; out = out " "; continue }
                    if (c == "r" && substr(s, i - 1, 1) !~ /[A-Za-z0-9_]/ \
                        || c == "r" && substr(s, i - 1, 1) == "b" && substr(s, i - 2, 1) !~ /[A-Za-z0-9_]/) {
                        for (k = 0; substr(s, i + 1 + k, 1) == "#"; k++) ;
                        if (substr(s, i + 1 + k, 1) == "\"") {
                            raw = k
                            i += k + 1
                            out = out "\""
                            continue
                        }
                    }
                    if (c == "\"") { instr = 1; out = out c; continue }
                    if (c == "\047" && d == "\\") {
                        j = index(substr(s, i + 2), "\047")
                        if (j) { i += j + 1; out = out " "; continue }
                    }
                    if (c == "\047" && substr(s, i + 2, 1) == "\047") { i += 2; out = out " "; continue }
                    out = out c
                }
                return out
            }
            function hashes(k,   h) {
                h = ""
                while (k-- > 0) h = h "#"
                return h
            }
            BEGIN {
                raw = -1
                SIZING = "(width|width_weighted|line_height|ascent|centred_top|baseline_in|elide_end|elided_width|draw_elided|prefix_fitting|draw_clipped)"
            }
            {
                text = $0
                line = code($0)
            }
            line ~ /Color32::(rgb|new|WHITE|BLACK)([^A-Za-z0-9_]|$)/ \
                || line ~ /(^|[^A-Za-z0-9_])(Color32|EncodedPixel)[[:space:]]*\(/ \
                || line ~ /(^|[^A-Za-z0-9_])Color32[[:space:]]+as[[:space:]]/ {
                hit(1, "colour", NR, text)
            }
            scope != "app" { next }
            line ~ /(^|[^A-Za-z0-9_])(with_alpha|mix)[[:space:]]*\(/ \
                || line ~ /(^|[^A-Za-z0-9_])(DARK([^A-Za-z0-9_]|$)|contrast::)/ { hit(1, "colour", NR, text) }
            line ~ "(^|[^A-Za-z0-9_])ui::" SIZING "[[:space:]]*[(]" \
                || line ~ /(^|[^A-Za-z0-9_])(font_size|size_px)([^A-Za-z0-9_]|$)/ \
                || line ~ /(^|[^A-Za-z0-9_])TextStyle[[:space:]]*\{/ { hit(2, "font-size", NR, text) }
            !importing && line ~ /^[[:space:]]*(pub([[:space:]]*\([^)]*\))?[[:space:]]+)?use[[:space:]]/ {
                importing = 1
                import = ""
                import_at = NR
                import_text = text
            }
            importing {
                import = import " " line
                if (index(line, ";")) {
                    importing = 0
                    if (import ~ /(^|[^A-Za-z0-9_])ui::/ \
                        && (import ~ /ui::[[:space:]]*\*/ \
                            || import ~ "(^|[^A-Za-z0-9_])" SIZING "([^A-Za-z0-9_]|$)")) {
                        hit(2, "font-size", import_at, import_text)
                    }
                }
            }
            {
                rest = line
                while (match(rest, /(^|[^A-Za-z0-9_])spacing:/)) {
                    value = substr(rest, RSTART + RLENGTH)
                    rest = value
                    sub(/[,}].*$/, "", value)
                    if (nonzero(value)) {
                        hit(3, "spacing", NR, text)
                        break
                    }
                }
            }
            line ~ /(^|[^A-Za-z0-9_])size:[[:space:]]*Length::Px[[:space:]]*\(/ {
                value = line
                sub(/.*Length::Px[[:space:]]*\(/, "", value)
                sub(/\).*$/, "", value)
                if (nonzero(value)) hit(3, "spacing", NR, text)
            }
            {
                rest = line
                if (calling) rest = feed(rest)
                while (!calling && match(rest, /EdgeInsets::(all|symmetric|new)[[:space:]]*\(/)) {
                    calling = 1
                    depth = 1
                    args = ""
                    call_at = NR
                    call_text = text
                    rest = feed(substr(rest, RSTART + RLENGTH))
                }
            }
            line ~ /(^|[^A-Za-z0-9_])EdgeInsets[[:space:]]*\{/ { hit(4, "inset", NR, text) }
        ' "$file" || { echo "check_ui_literals: awk failed on $file" >&2; exit 2; }
    done
}

# The findings no allowlist entry covers, then a tag-5 finding for each
# entry that covered none.
apply_allowlist() {
    local findings="$1" allowlist="$2"
    awk -F '\t' -v list="$allowlist" '
        BEGIN {
            n = 0
            while ((getline entry < list) > 0) {
                if (entry ~ /^[[:space:]]*(#|$)/) continue
                split(entry, f, /[[:space:]]+/)
                n++
                check[n] = f[1]
                path[n] = f[2]
                used[n] = 0
                text[n] = entry
            }
        }
        $0 == "" { next }
        {
            file = $3
            sub(/:[0-9]+: .*$/, "", file)
            for (i = 1; i <= n; i++) {
                if (check[i] == $2 && path[i] == file) {
                    used[i] = 1
                    next
                }
            }
            print
        }
        END {
            for (i = 1; i <= n; i++) {
                if (!used[i]) printf "5\tallowlist\t%s: matches nothing\n", text[i]
            }
        }
    ' <(printf '%s\n' "$findings")
}

run_gate() {
    local root="$1" list="$2" allowlist="$3"
    local classified
    classified="$(classify "$root" "$list")"
    if ! printf '%s\n' "$classified" | grep -q '^app	userland/src/apps/'; then
        echo "check_ui_literals: no appkit app found under userland/src/apps/ —" >&2
        echo "  the scope rule found nothing to scan, so refusing to report OK." >&2
        exit 2
    fi
    local findings
    findings="$(scan_sources "$root" "$classified")" || exit 2
    apply_allowlist "$findings" "$allowlist"
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------
if [ "$GATE_SELF_TEST" -eq 1 ]; then
    gate_selftest_begin check_ui_literals

    cat > "$(gate_fixture apps/src/painted.rs)" <<'FIXTURE'
const A: Color32 = Color32::rgb(1, 2, 3);
const B: Color32 = slopos_abi::draw::Color32::new(1, 2, 3, 4);
const C: Color32 = Color32::WHITE;
fn d() -> Color32 { Color32(0xff00_0000) }
fn e(c: &mut Color32) {
    *c = Color32::rgb(1, 2, 3);
}
use slopos_abi::draw::Color32 as Rgb;
const F: EncodedPixel = EncodedPixel(0xff12_3456);
const G: Color32 = with_alpha(Color32::TRANSPARENT, 0xff);
fn h() -> Color32 { slopos_appkit::theme::DARK.danger }
fn i(s: &str) -> i32 { ui::width(s, BODY.size) }
use slopos_appkit::text::ui::{BULLET, width};
use slopos_appkit::text::ui::*;
fn j() { let font_size = 14; }
fn k() { let size_px = 12; }
const L: TextStyle = TextStyle { size: 14, weight: Weight::Regular, line_height: 20 };
fn m() { Node::VStack { spacing: 8, children, align }; }
fn n() { Node::VStack { spacing: space::SM + 2, children, align }; }
fn o() { Node::VStack { spacing: 0x8, children, align }; }
fn p() { Node::Spacer { size: Length::Px(8) }; }
fn q() { let p = EdgeInsets::all(16); }
fn r() { let p = EdgeInsets::new(4, space::SM, 0, 0); }
fn s() { let p = EdgeInsets::all(0x10); }
fn t() {
    let p = EdgeInsets::symmetric(
        24,
        space::MD,
    );
}
fn u() {
    let p = EdgeInsets::new(
        pad(0),
        12,
        0,
        0,
    );
}
fn v() { let p = EdgeInsets { top: 1, right: 0, bottom: 0, left: 0 }; }
fn w() { let g = "/usr/share/*"; }
const X: Color32 = Color32::rgb(1, 2, 3);
fn y(h: &str) { let u = format!("http://{h}"); let c = Color32::rgb(1, 2, 3); }
fn z() { let q = '"'; let c = Color32::WHITE; }
fn aa<'a>(s: &'a str) -> Color32 { Color32::BLACK }
fn ab() { let p = (EdgeInsets::all(space::SM), EdgeInsets::all(4)); }
fn ac(a: Color32, b: Color32) -> f64 { contrast::ratio(a, b) }
use slopos_appkit::text::ui::{
    BULLET,
    width,
};
use slopos_appkit::text::{self, ui::line_height};
const AD: &str = r#"a"b"#;
const AE: Color32 = Color32::rgb(1, 2, 3);
fn af() { let a = Node::VStack { spacing: 0, children }; let b = Node::HStack { spacing: 6, children }; }
FIXTURE

    cat > "$(gate_fixture apps/src/themed.rs)" <<'FIXTURE'
// const A: Color32 = Color32::rgb(1, 2, 3);
/* spacing: 8 */ fn after_a_block_comment() {}
/*
 * Color32::rgb(1, 2, 3)
 * EdgeInsets::all(16)
 */
/// Doc: `Color32::WHITE`, spacing: 8.
fn a(theme: &Theme) -> Color32 { let c: Color32 = theme.text; c }
fn b(c: Color32) -> Color32 { theme::current().accent }
const C: Color32 = Color32::TRANSPARENT;
fn d() { Node::VStack { spacing: 0, children, align }; }
fn e() { Node::HStack { spacing: space::SM, children, align }; }
fn f() { let p = EdgeInsets::new(space::SM, 0, 0, space::SM); }
fn g() { let p = EdgeInsets::ZERO; let q = inset::PAGE; }
fn h() { Node::Spacer { size: Length::Px(space::SM) }; }
fn i() { Node::SizedBox { width: Some(Length::Px(140)), height: None, child }; }
fn j() { TableColumnWidth::Fixed(80); let darkness = 2; }
fn k() {
    let p = EdgeInsets::symmetric(
        space::XL,
        0,
    );
    let rows = vec![
        1,
        2,
    ];
}
fn l(ctx: &mut PaintContext) { ctx.draw_text_in(0, 0, "x", TextRole::Body, ctx.theme().text); }
use slopos_appkit::text::ui;
fn m() -> String { format!("{}{}", ui::ELLIPSIS, slopos_appkit::text::ui::BULLET) }
fn n() -> ui::Weight { ui::Weight::Regular }
fn o() -> &'static str { "Color32::rgb(1, 2, 3) spacing: 8 EdgeInsets::all(16)" }
const P: &str = "a string that runs on: \
    spacing: 8, Color32::WHITE";
fn q() -> char { '\'' }
FIXTURE

    cat > "$(gate_fixture userland/src/apps/demo/mod.rs)" <<'FIXTURE'
impl App for Demo {}
FIXTURE
    cat > "$(gate_fixture userland/src/apps/demo/view.rs)" <<'FIXTURE'
const A: Color32 = Color32::rgb(1, 2, 3);
FIXTURE
    cat > "$(gate_fixture userland/src/apps/generic/mod.rs)" <<'FIXTURE'
impl<T: Clone> slopos_appkit::App for Generic<T> {}
const A: Color32 = Color32::rgb(1, 2, 3);
FIXTURE
    cat > "$(gate_fixture userland/src/apps/gallery.rs)" <<'FIXTURE'
pub fn main() -> ! { slopos_appkit::run_app(Gallery, 1, 1) }
fn a() { Node::VStack { spacing: 4, children, align }; }
FIXTURE
    cat > "$(gate_fixture userland/src/apps/compositor/frame.rs)" <<'FIXTURE'
const A: Color32 = Color32::rgb(1, 2, 3);
fn b() { Node::VStack { spacing: 8, children, align }; }
FIXTURE

    cat > "$(gate_fixture appkit/src/widgets/painter.rs)" <<'FIXTURE'
const A: Color32 = Color32::new(0, 0, 0, 40);
fn b(theme: &Theme) { let font_size = 14; let p = EdgeInsets::all(16); with_alpha(theme.text, 0x40); }
FIXTURE
    cat > "$(gate_fixture appkit/src/tests/fixture.rs)" <<'FIXTURE'
const A: Color32 = Color32::rgb(1, 2, 3);
FIXTURE

    cat > "$(gate_fixture apps/src/palette.rs)" <<'FIXTURE'
const KEYWORD: Color32 = Color32::rgb(1, 2, 3);
fn a() { Node::VStack { spacing: 8, children, align }; }
FIXTURE
    fixture_allowlist="$(gate_fixture allowlist.txt)"
    cat > "$fixture_allowlist" <<'FIXTURE'
# A comment, and a blank line, are no entries.

colour apps/src/palette.rs a syntax palette
colour apps/src/gone.rs a file that no longer exists
FIXTURE

    fixture_files="$(gate_collect_rs_files "$GATE_FIXTURE_ROOT")"
    gate_expect_enumerator "$GATE_FIXTURE_ROOT" "$fixture_files"
    GATE_FINDINGS="$(run_gate "$GATE_FIXTURE_ROOT" "$fixture_files" "$fixture_allowlist")"

    gate_expect 1 18 "constructors, a deref, an alias, a raw pixel, a derived colour, a theme table and contrast maths in apps, after a string holding /*, //, a quote, a lifetime or a raw string's quote, one in each appkit app's module, one in appkit"
    gate_expect 2 8 "a ui:: sizing call, importing one or all of them on one line, over several or nested, font_size, size_px and a TextStyle literal"
    gate_expect 3 7 "a number, token arithmetic, hex, the second stack on a line and a spacer's size in an app, a single-file appkit app, and the allowlisted file's other check"
    gate_expect 4 7 "all, new, hex, a split symmetric, a split new with a nested call, the second call on a line, and a struct literal"
    gate_expect 5 1 "the entry for a file that no longer exists"
    gate_expect_silent 'themed\.rs|compositor/|appkit/src/tests/|painter\.rs:2|palette\.rs:1:' \
        "comments, nested ones, strings, raw ones, roles, the current theme, TRANSPARENT, zero, tokens, sizes, a list of numbers, the ellipsis and bullet, non-app modules, appkit's own geometry and tests, and the allowlisted check"

    gate_selftest_end
fi

# ---------------------------------------------------------------------------
# Real run
# ---------------------------------------------------------------------------
file_list="$(gate_collect_rs_files "$REPO_ROOT")"
gate_require_nonempty check_ui_literals "$REPO_ROOT" "$file_list"

offenders="$(run_gate "$REPO_ROOT" "$file_list" "$ALLOWLIST" | cut -f2-)"

if [ -n "$offenders" ]; then
    echo "check_ui_literals: an app picks its own colour, size or spacing:" >&2
    echo "$offenders" | sed 's/^/    /' >&2
    echo "  Name a role instead: a Tone or a Surface, a TextRole, a theme::space or" >&2
    echo "  theme::inset token. A file whose colours are its content takes a line in" >&2
    echo "  scripts/gates/ui-literals.txt saying why; an entry that matches nothing" >&2
    echo "  is taken away." >&2
    exit 1
fi

echo "check_ui_literals: OK — apps draw with the theme's roles and tokens"

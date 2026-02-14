#!/usr/bin/env sh

set -eu

fallback_width="${QEMU_FB_WIDTH:-1920}"
fallback_height="${QEMU_FB_HEIGHT:-1080}"
policy="${QEMU_FB_AUTO_POLICY:-primary}"
preferred_output="${QEMU_FB_AUTO_OUTPUT:-}"

is_positive_int() {
    case "$1" in
        ''|*[!0-9]*) return 1 ;;
        0) return 1 ;;
        *) return 0 ;;
    esac
}

sanitize_mode() {
    printf '%s' "$1" | tr -cd '0-9x'
}

collect_candidates_linux() {
    out_file="$1"

    if command -v xrandr >/dev/null 2>&1; then
        xrandr --current 2>/dev/null | awk '
            $0 ~ /^[^[:space:]].* connected/ {
                name = $1
                primary = ($0 ~ / connected primary /) ? 1 : 0
                if (match($0, /[0-9]+x[0-9]+\+[0-9]+\+[0-9]+/)) {
                    geom = substr($0, RSTART, RLENGTH)
                    split(geom, parts, /[+x]/)
                    if (parts[1] > 0 && parts[2] > 0) {
                        print name, parts[1], parts[2], primary
                    }
                }
            }
        ' >>"$out_file"
    fi

    if command -v wlr-randr >/dev/null 2>&1; then
        wlr-randr 2>/dev/null | awk '
            /^[^[:space:]]/ {
                current_output = $1
                sub(/:$/, "", current_output)
            }
            /current/ {
                for (i = 1; i <= NF; i++) {
                    if ($i ~ /^[0-9]+x[0-9]+$/) {
                        split($i, wh, "x")
                        if (wh[1] > 0 && wh[2] > 0 && current_output != "") {
                            print current_output, wh[1], wh[2], 0
                            break
                        }
                    }
                }
            }
        ' >>"$out_file"
    fi
}

collect_candidates_macos() {
    out_file="$1"
    if command -v system_profiler >/dev/null 2>&1; then
        system_profiler SPDisplaysDataType 2>/dev/null | awk '
            /Resolution:/ {
                w = $2
                h = $4
                if (w ~ /^[0-9]+$/ && h ~ /^[0-9]+$/) {
                    main = (is_main == 1) ? 1 : 0
                    print display, w, h, main
                    is_main = 0
                }
            }
            /Main Display: Yes/ { is_main = 1 }
            /^[[:space:]]+[A-Za-z0-9].*:/ {
                line = $0
                gsub(/^[[:space:]]+/, "", line)
                if (line !~ /^Resolution:/ && line !~ /^Main Display:/ && line !~ /^UI Looks like:/) {
                    display = line
                    sub(/:$/, "", display)
                }
            }
        ' >>"$out_file"
    fi
}

choose_mode_from_candidates() {
    candidates_file="$1"

    if [ -n "$preferred_output" ]; then
        mode="$(awk -v wanted="$preferred_output" '$1 == wanted { print $2 "x" $3; exit }' "$candidates_file")"
        if [ -n "$mode" ]; then
            printf '%s\n' "$mode"
            return 0
        fi
    fi

    case "$policy" in
        first)
            mode="$(awk 'NF >= 3 { print $2 "x" $3; exit }' "$candidates_file")"
            ;;
        max)
            mode="$(awk 'NF >= 3 { area = $2 * $3; if (area > best) { best = area; mode = $2 "x" $3 } } END { if (mode != "") print mode }' "$candidates_file")"
            ;;
        primary|*)
            mode="$(awk '$4 == 1 { print $2 "x" $3; exit }' "$candidates_file")"
            if [ -z "$mode" ]; then
                mode="$(awk 'NF >= 3 { area = $2 * $3; if (area > best) { best = area; mode = $2 "x" $3 } } END { if (mode != "") print mode }' "$candidates_file")"
            fi
            ;;
    esac

    if [ -n "$mode" ]; then
        printf '%s\n' "$mode"
        return 0
    fi

    return 1
}

detect_mode_linux() {
    candidates_file="$(mktemp)"
    trap 'rm -f "$candidates_file"' EXIT INT TERM

    collect_candidates_linux "$candidates_file"

    if mode="$(choose_mode_from_candidates "$candidates_file" 2>/dev/null)"; then
        printf '%s\n' "$mode"
        return 0
    fi

    if command -v xdpyinfo >/dev/null 2>&1; then
        mode="$(xdpyinfo 2>/dev/null | awk '/dimensions:/ { print $2; exit }')"
        if [ -n "$mode" ]; then
            printf '%s\n' "$mode"
            return 0
        fi
    fi

    return 1
}

detect_mode_macos() {
    candidates_file="$(mktemp)"
    trap 'rm -f "$candidates_file"' EXIT INT TERM

    collect_candidates_macos "$candidates_file"
    if mode="$(choose_mode_from_candidates "$candidates_file" 2>/dev/null)"; then
        printf '%s\n' "$mode"
        return 0
    fi

    return 1
}

mode=""
os_name="$(uname -s 2>/dev/null || echo unknown)"

case "$os_name" in
    Linux)
        mode="$(detect_mode_linux || true)"
        ;;
    Darwin)
        mode="$(detect_mode_macos || true)"
        ;;
esac

mode="$(sanitize_mode "$mode")"

if [ -n "$mode" ] && [ "${mode#*x}" != "$mode" ]; then
    width="${mode%x*}"
    height="${mode#*x}"
else
    width="$fallback_width"
    height="$fallback_height"
fi

if ! is_positive_int "$width"; then
    width="$fallback_width"
fi

if ! is_positive_int "$height"; then
    height="$fallback_height"
fi

if ! is_positive_int "$fallback_width"; then
    fallback_width="1920"
fi

if ! is_positive_int "$fallback_height"; then
    fallback_height="1080"
fi

if ! is_positive_int "$width"; then
    width="$fallback_width"
fi

if ! is_positive_int "$height"; then
    height="$fallback_height"
fi

printf '%s %s\n' "$width" "$height"

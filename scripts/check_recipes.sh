#!/usr/bin/env bash
# Hold every recipe under toolchain/recipes/ to the shape build_recipes.sh
# promises: a pinned upstream tarball built by a template, and nothing else.
#
#   - one `sha256` of 64 lowercase hex digits and one `https://` `url` that
#     names the recipe's `version`, so what is built is what was reviewed;
#   - a `license` and a `template` the driver knows (`cmake`, `openssl`);
#   - every `depends` names another recipe;
#   - no file in the recipe's directory but `recipe` and the `config` it
#     declares, and never a `*.patch` or `*.diff`: a build that needs an edit
#     to upstream is a slibc or kernel finding, fixed there;
#   - a NOTICE.md entry naming `toolchain/recipes/<name>/`;
#   - a recipe that has been built carries the stamp
#     `build_recipes.sh --print-stamp` computes for it now, so a prefix left
#     behind by other inputs is not graded as this tree's. Asked of the
#     driver rather than recomputed here. Nothing built is the CI case and
#     skips this half.
#
# Usage: check_recipes.sh
#        check_recipes.sh --self-test

set -euo pipefail

SELF="check_recipes"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

TEMPLATES="cmake openssl"

fail() {
    echo "$SELF: $*" >&2
    exit 1
}

values() {
    sed -n "s/^$2=\\(.*\\)\$/\\1/p" "$1"
}

# The one value of `key`, or a failure naming the recipe.
single() {
    local file="$1" key="$2" name="$3" found
    found="$(values "$file" "$key")"
    [ -n "$found" ] || fail "$name: no $key"
    [ "$(printf '%s\n' "$found" | wc -l)" -eq 1 ] || fail "$name: more than one $key"
    printf '%s\n' "$found"
}

check_recipe() {
    local root="$1" name="$2"
    local dir="$root/toolchain/recipes/$name" file="$root/toolchain/recipes/$name/recipe"
    [ -f "$file" ] || fail "$name: no recipe file"

    local version url sha256 template config dep
    # `|| exit`: a self-test runs this under `if`, where `set -e` is off.
    version="$(single "$file" version "$name")" || exit 1
    url="$(single "$file" url "$name")" || exit 1
    sha256="$(single "$file" sha256 "$name")" || exit 1
    single "$file" license "$name" >/dev/null
    template="$(single "$file" template "$name")" || exit 1

    [[ "$sha256" =~ ^[0-9a-f]{64}$ ]] || fail "$name: sha256 is not 64 lowercase hex digits"
    [[ "$url" == https://* ]] || fail "$name: url is not https: $url"
    [[ "$url" == *"$version"* ]] || fail "$name: url does not name version $version: $url"
    case " $TEMPLATES " in
        *" $template "*) ;;
        *) fail "$name: unknown template '$template' (known: $TEMPLATES)" ;;
    esac
    for dep in $(values "$file" depends); do
        [ -f "$root/toolchain/recipes/$dep/recipe" ] || fail "$name: depends on $dep, which is no recipe"
    done

    config="$(values "$file" config)"
    local entry base
    while IFS= read -r -d '' entry; do
        base="${entry#"$dir"/}"
        case "$base" in
            *.patch | *.diff) fail "$name: carries a patch ($base); recipes build upstream unmodified" ;;
        esac
        [ "$base" = recipe ] && continue
        [ -n "$config" ] && [ "$base" = "$config" ] && continue
        fail "$name: $base is neither the recipe nor its declared config"
    done < <(find "$dir" -mindepth 1 -print0)
    [ -z "$config" ] || [ -f "$dir/$config" ] || fail "$name: declared config $config is missing"

    grep -qF "\`toolchain/recipes/$name/\`" "$root/NOTICE.md" ||
        fail "$name: no NOTICE.md entry naming \`toolchain/recipes/$name/\`"
}

# Each built recipe's stamp against the driver's answer for the tree now.
check_stamps() {
    local root="$1" out="$2" maker="$3"
    local built=() name
    for name in "${@:4}"; do
        [ -f "$out/$name/stamp" ] && built+=("$name")
    done
    [ "${#built[@]}" -gt 0 ] || { echo "nothing built"; return 0; }

    local want
    want="$("$maker" --print-stamp "${built[@]}")" ||
        fail "built recipes in $out, but $(basename "$maker") --print-stamp failed"
    for name in "${built[@]}"; do
        local expect have
        expect="$(printf '%s\n' "$want" | awk -v n="$name" '$1 == n { print $2 }')"
        have="$(cat "$out/$name/stamp")"
        [ -n "$expect" ] || fail "$name: the driver printed no stamp for it"
        [ "$have" = "$expect" ] ||
            fail "$name: built from other inputs (stamp $have, now $expect) — run just recipes"
    done
    echo "${#built[@]} built, stamps current"
}

check_tree() {
    local root="$1" out="$2"
    local names=() dir
    for dir in "$root"/toolchain/recipes/*/; do
        [ -d "$dir" ] || continue
        names+=("$(basename "$dir")")
    done
    [ "${#names[@]}" -gt 0 ] || fail "no recipes under $root/toolchain/recipes"
    local stray
    stray="$(find "$root/toolchain/recipes" -mindepth 1 -maxdepth 1 ! -type d -printf '%f\n')"
    [ -z "$stray" ] || fail "toolchain/recipes holds files outside any recipe: $stray"

    local name
    for name in "${names[@]}"; do
        check_recipe "$root" "$name"
    done
    local built
    built="$(check_stamps "$root" "$out" "$root/scripts/build_recipes.sh" "${names[@]}")" || exit 1
    echo "$SELF: OK — ${#names[@]} recipes (${names[*]}); $built"
}

self_test() {
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN

    local r="$tmp/toolchain/recipes"
    mkdir -p "$r/alpha" "$r/beta" "$tmp/out" "$tmp/scripts"
    cat >"$r/alpha/recipe" <<'EOF'
# a comment
version=1.2.3
url=https://example.org/alpha-1.2.3.tar.xz
sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
license=MIT
template=cmake
arg=-DX=ON
EOF
    cat >"$r/beta/recipe" <<'EOF'
version=4.5
url=https://example.org/beta-4.5.tar.gz
sha256=fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210
license=Apache-2.0
template=openssl
config=target.conf
depends=alpha
EOF
    : >"$r/beta/target.conf"
    printf '`toolchain/recipes/alpha/` and `toolchain/recipes/beta/`\n' >"$tmp/NOTICE.md"
    printf '#!/bin/sh\nshift\nfor n; do echo "$n good"; done\n' >"$tmp/scripts/build_recipes.sh"
    chmod +x "$tmp/scripts/build_recipes.sh"

    expect_ok() {
        local out
        out="$(check_tree "$tmp" "$tmp/out" 2>&1)" ||
            fail "--self-test: $1 was rejected: $out"
        case "$out" in
            "$SELF: OK"*) ;;
            *) fail "--self-test: $1 printed more than its OK line: $out" ;;
        esac
    }
    expect_reject() {
        if (check_tree "$tmp" "$tmp/out" >/dev/null 2>&1); then
            fail "--self-test: $1 was accepted"
        fi
    }
    # Applies a sed expression to alpha's recipe, expects a rejection, undoes.
    reject_edit() {
        cp "$r/alpha/recipe" "$tmp/alpha.saved"
        sed -i "$1" "$r/alpha/recipe"
        expect_reject "$2"
        cp "$tmp/alpha.saved" "$r/alpha/recipe"
    }

    expect_ok "a well-formed pair of recipes with nothing built"

    reject_edit 's/^sha256=.*/sha256=0123456789abcdef/' "a short sha256"
    reject_edit 's/^sha256=0/sha256=G/' "a sha256 with a non-hex digit"
    reject_edit 's/^sha256=01/sha256=AB/' "an uppercase sha256"
    reject_edit '/^sha256=/d' "a recipe with no sha256"
    reject_edit 's/^sha256=.*/&\n&/' "a recipe with two sha256 lines"
    reject_edit 's|^url=https:|url=http:|' "a plain-http url"
    reject_edit 's|alpha-1.2.3|alpha-1.2.4|' "a url naming another version"
    reject_edit 's/^template=.*/template=autotools/' "an unknown template"
    reject_edit '/^license=/d' "a recipe with no license"
    reject_edit '$ a depends=gamma' "a dependency on no recipe"

    : >"$r/alpha/0001-fix.patch"
    expect_reject "a recipe carrying a .patch"
    rm "$r/alpha/0001-fix.patch"
    : >"$r/alpha/fix.diff"
    expect_reject "a recipe carrying a .diff"
    rm "$r/alpha/fix.diff"
    : >"$r/alpha/extra.cmake"
    expect_reject "an undeclared file beside a recipe"
    rm "$r/alpha/extra.cmake"
    mkdir "$r/alpha/patches"
    expect_reject "a directory beside a recipe"
    rmdir "$r/alpha/patches"
    rm "$r/beta/target.conf"
    expect_reject "a declared config that is missing"
    : >"$r/beta/target.conf"
    : >"$r/README"
    expect_reject "a stray file in the recipes directory"
    rm "$r/README"

    printf '`toolchain/recipes/alpha/`\n' >"$tmp/NOTICE.md"
    expect_reject "a recipe with no NOTICE.md entry"
    printf '`toolchain/recipes/alpha/` and `toolchain/recipes/beta/`\n' >"$tmp/NOTICE.md"

    mkdir -p "$tmp/out/alpha"
    echo good >"$tmp/out/alpha/stamp"
    expect_ok "a built recipe whose stamp is current"
    echo stale >"$tmp/out/alpha/stamp"
    expect_reject "a built recipe with a stale stamp"
    echo good >"$tmp/out/alpha/stamp"
    printf '#!/bin/sh\nexit 1\n' >"$tmp/scripts/build_recipes.sh"
    expect_reject "a built recipe whose driver cannot print a stamp"
    echo "$SELF: --self-test OK"
}

case "${1:-}" in
    --self-test) self_test ;;
    "")
        BUILD_DIR="${BUILD_DIR:-$REPO_ROOT/builddir}"
        check_tree "$REPO_ROOT" "${SLOPOS_RECIPES_DIR:-$BUILD_DIR/slopos-recipes}"
        ;;
    *) echo "usage: $SELF.sh | --self-test" >&2; exit 2 ;;
esac

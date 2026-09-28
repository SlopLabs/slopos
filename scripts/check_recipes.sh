#!/usr/bin/env bash
# Hold every recipe under toolchain/recipes/ to the shape build_recipes.sh
# promises: a pinned upstream tarball built by a template, and nothing else.
#
#   - one lowercase-hex `sha256` and one `https://` `url` naming `version`;
#   - a `license`, a known `template`, at least one `soname`, no unread key;
#   - at most one `depends` line, naming other recipes, acyclic;
#   - no file but `recipe` and its declared `config`; never a patch: an edit
#     to upstream is a slibc or kernel finding;
#   - every `arg` picks among upstream's options and carries no code, since a
#     flag, CMake script, launcher or search root edits what is built with
#     every file pristine. A `cmake` arg is `-D<NAME>=<value>`: `CMAKE_*`
#     names from `CMAKE_ARG_NAMES`, project names outside `PROJECT_ARG_DENY`
#     (bar `PROJECT_ARG_ALLOW`), values a word or an `/etc` path. An `openssl`
#     arg is `no-*`, `enable-*`, `shared`, `threads` or `--openssldir=/etc/..`;
#   - an `openssl` `config`, which `Configure` evaluates as Perl, is data: one
#     `%targets` entry for `target`, `CONFIG_FIELDS` only, non-interpolating
#     strings, and only benign flags;
#   - a NOTICE.md entry naming `toolchain/recipes/<name>/`;
#   - a built recipe carries the stamp `build_recipes.sh --print-stamp` gives
#     now (skipped when nothing is built, as in CI).
#
# The driver fails any build that changes the unpacked tree.
#
# Usage: check_recipes.sh
#        check_recipes.sh --self-test

set -euo pipefail

SELF="check_recipes"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

TEMPLATES="cmake openssl"
RECIPE_KEYS="version url sha256 license template depends soname arg config target"

CMAKE_ARG_NAMES='^CMAKE_(BUILD_TYPE|POSITION_INDEPENDENT_CODE|(REQUIRE|DISABLE)_FIND_PACKAGE_[A-Za-z0-9_]+|INSTALL_(BINDIR|SBINDIR|LIBEXECDIR|SYSCONFDIR|DATAROOTDIR|DATADIR|INCLUDEDIR|DOCDIR|MANDIR))$'
PROJECT_ARG_DENY='(FLAGS|DEFINITIONS|DEFINES|INCLUDE|LAUNCHER|COMPILER|LINKER|TOOLCHAIN|COMMAND|SCRIPT|MODULE|EXECUTABLE|PROGRAM|FETCHCONTENT|_DIR|_ROOT|_PATH$|_LIBRAR(Y|IES)(_RELEASE|_DEBUG)?$|_FILE$|_HINTS?$)'
PROJECT_ARG_ALLOW='^[A-Z0-9]+_CA_PATH$'
WORD='^[A-Za-z0-9_][A-Za-z0-9_.,+:-]*$'
ETC_PATH='^/etc(/[A-Za-z0-9_+-][A-Za-z0-9_.+-]*)+$'
RELATIVE_DIR='^[a-z0-9_]+(/[a-z0-9_+-][a-z0-9_.+-]*)*$'
FILE_NAME='^[A-Za-z0-9_][A-Za-z0-9_.-]*$'
SONAME='^lib[A-Za-z0-9_+-]+\.so(\.[0-9]+)*$'
CONFIG_FIELDS="inherit_from bn_ops asm_arch perlasm_scheme thread_scheme dso_scheme shared_target CFLAGS cflags CXXFLAGS cxxflags cppflags lib_cppflags lflags ex_libs shared_cflag shared_ldflag"

# Parses the config without running it. `$1` the file, `$2` the target it must
# define, `$3` the fields it may set.
read -r -d '' CONFIG_GRAMMAR <<'PERL' || true
use strict;
use warnings;
my ($file, $want, $fields) = @ARGV;
my %word_field = map { $_ => 1 } qw(inherit_from bn_ops asm_arch perlasm_scheme thread_scheme dso_scheme shared_target);
my %field = map { $_ => 1 } split ' ', $fields;
my %func = map { $_ => 1 } qw(picker threads add add_before);
sub bad { print STDERR "  $file: $_[0]\n"; exit 1 }
open(my $fh, '<', $file) or bad("cannot be read");
my $src = do { local $/; <$fh> };
my @tok;
pos($src) = 0;
while (pos($src) < length $src) {
    next if $src =~ /\G\s+/gc || $src =~ /\G#[^\n]*/gc;
    if ($src =~ /\G"((?:[^"\\\$\@]|\\[\\\$\@"])*)"/gc) {
        (my $s = $1) =~ s/\\(.)/$1/g;
        push @tok, ['str', $s];
    } elsif ($src =~ /\G'([^'\\]*)'/gc) {
        push @tok, ['str', $1];
    } elsif ($src =~ /\G(=>|%targets\b|[(){}\[\],;=])/gc) {
        push @tok, ['p', $1];
    } elsif ($src =~ /\G([A-Za-z_][A-Za-z0-9_]*)/gc) {
        push @tok, ['word', $1];
    } else {
        (my $at = substr($src, pos($src), 32)) =~ s/\n.*//s;
        bad("'$at' is neither a plain string nor punctuation of a target definition");
    }
}
my $i = 0;
sub peek { $i < @tok ? $tok[$i] : ['eof', 'the end of the file'] }
sub take { my $t = peek(); $i++; $t }
sub is { my $t = peek(); $t->[0] eq $_[0] && $t->[1] eq $_[1] }
sub expect { my $t = take(); bad("expected '$_[1]', found '$t->[1]'") unless $t->[0] eq $_[0] && $t->[1] eq $_[1] }
sub comma { take() if is('p', ',') }
sub text {
    my ($name, $s) = @_;
    for my $w (split ' ', $s) {
        if ($word_field{$name}) {
            next if $w =~ /^[A-Za-z0-9_][A-Za-z0-9_.+-]*$/;
            bad("'$w' in $name is not a word");
        }
        next if $w =~ /^-(?:O[0-3sz]?|g[0-3]?|pthread|m64|fPIC|fpic|fPIE|fpie)$/;
        next if $w =~ /^-W(?:no-)?[a-z][a-z0-9-]*$/;
        next if $w =~ /^-D[A-Z_][A-Z0-9_]*(?:=[0-9]+)?$/;
        next if $w =~ /^-l[a-z0-9_]+$/;
        next if $w =~ /^-Wl,-z,[a-z]+$/;
        next if $w =~ /^-Wl,-rpath,'\$\$ORIGIN'$/;
        bad("'$w' in $name is not a flag a target definition may carry");
    }
}
sub value {
    my ($name) = @_;
    my $t = take();
    if ($t->[0] eq 'str') {
        text($name, $t->[1]);
    } elsif ($t->[0] eq 'p' && $t->[1] eq '[') {
        until (is('p', ']')) { value($name); comma() }
        expect('p', ']');
    } elsif ($t->[0] eq 'word' && $func{$t->[1]}) {
        expect('p', '(');
        until (is('p', ')')) {
            if (peek()->[0] eq 'word' && $i + 1 < @tok && $tok[$i + 1][1] eq '=>') { take(); take() }
            value($name);
            comma();
        }
        expect('p', ')');
    } else {
        bad("'$t->[1]' in $name is not a string, a list or one of " . join(', ', sort keys %func));
    }
}
expect('word', 'my');
expect('p', '%targets');
expect('p', '=');
expect('p', '(');
my @names;
until (is('p', ')')) {
    my $t = take();
    bad("'$t->[1]' is where a target's name, a string, belongs") unless $t->[0] eq 'str';
    push @names, $t->[1];
    expect('p', '=>');
    expect('p', '{');
    until (is('p', '}')) {
        my $k = take();
        bad("'$k->[1]' is not a field a target definition may set here") unless $k->[0] eq 'word' && $field{$k->[1]};
        expect('p', '=>');
        value($k->[1]);
        comma();
    }
    expect('p', '}');
    comma();
}
expect('p', ')');
expect('p', ';');
bad("text follows the %targets definition") unless peek()->[0] eq 'eof';
bad("defines " . join(', ', map { "'$_'" } @names) . "; the recipe's target is '$want'")
    unless @names == 1 && $names[0] eq $want;
PERL

fail() {
    echo "$SELF: $*" >&2
    exit 1
}

values() {
    sed -n "s/^$2=\\(.*\\)\$/\\1/p" "$1"
}

single() {
    local file="$1" key="$2" name="$3" found
    found="$(values "$file" "$key")"
    [ -n "$found" ] || fail "$name: no $key"
    [ "$(printf '%s\n' "$found" | wc -l)" -eq 1 ] || fail "$name: more than one $key"
    printf '%s\n' "$found"
}

check_cmake_arg() {
    local name="$1" arg="$2" var value
    [[ "$arg" =~ ^-D([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] ||
        fail "$name: arg '$arg' is not -D<NAME>=<value>, the one form a cmake recipe passes"
    var="${BASH_REMATCH[1]}"
    value="${BASH_REMATCH[2]}"
    if [[ "$var" == CMAKE_* ]]; then
        [[ "$var" =~ $CMAKE_ARG_NAMES ]] ||
            fail "$name: arg '$arg' sets $var, which is not one of the CMake variables a recipe may set"
        if [[ "$var" == CMAKE_INSTALL_* ]]; then
            [[ "$value" =~ $RELATIVE_DIR ]] ||
                fail "$name: arg '$arg' names an install directory that is not relative to the prefix"
            return 0
        fi
    elif [[ "$var" =~ $PROJECT_ARG_DENY && ! "$var" =~ $PROJECT_ARG_ALLOW ]]; then
        fail "$name: arg '$arg' names a flag, file, program or search root"
    fi
    [[ "$value" =~ $WORD || "$value" =~ $ETC_PATH ]] ||
        fail "$name: arg '$arg' has a value that is neither a word nor a path under /etc"
}

check_openssl_arg() {
    local name="$1" arg="$2"
    case "$arg" in
        shared | threads) return 0 ;;
    esac
    [[ "$arg" =~ ^(no|enable)-[a-z0-9][a-z0-9_-]*$ ]] && return 0
    [[ "$arg" =~ ^--openssldir=(.*)$ ]] && [[ "${BASH_REMATCH[1]}" =~ $ETC_PATH ]] && return 0
    fail "$name: arg '$arg' is not an OpenSSL feature switch (no-*, enable-*, shared, threads) or an --openssldir under /etc"
}

check_recipe() {
    local root="$1" name="$2"
    local dir="$root/toolchain/recipes/$name" file="$root/toolchain/recipes/$name/recipe"
    [ -f "$file" ] || fail "$name: no recipe file"

    local line key
    while IFS= read -r line; do
        case "$line" in "" | "#"*) continue ;; esac
        key="${line%%=*}"
        [ "$key" != "$line" ] && case " $RECIPE_KEYS " in *" $key "*) true ;; *) false ;; esac ||
            fail "$name: '$line' is not one of the keys the driver reads ($RECIPE_KEYS)"
    done <"$file"

    local version url sha256 template config target dep soname arg
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
    [ "$(values "$file" depends | wc -l)" -le 1 ] || fail "$name: more than one depends"
    for dep in $(values "$file" depends); do
        [ -f "$root/toolchain/recipes/$dep/recipe" ] || fail "$name: depends on $dep, which is no recipe"
    done
    [ -n "$(values "$file" soname)" ] || fail "$name: no soname"
    while IFS= read -r soname; do
        [[ "$soname" =~ $SONAME ]] || fail "$name: soname '$soname' is not lib<name>.so[.<n>...]"
    done < <(values "$file" soname)

    config="$(values "$file" config)"
    target="$(values "$file" target)"
    if [ "$template" = openssl ]; then
        config="$(single "$file" config "$name")" || exit 1
        target="$(single "$file" target "$name")" || exit 1
        [[ "$config" =~ $FILE_NAME ]] || fail "$name: config '$config' is not a file name beside the recipe"
        [[ "$target" =~ $FILE_NAME ]] || fail "$name: target '$target' is not a target name"
    else
        [ -z "$config$target" ] || fail "$name: config and target are for the openssl template, not $template"
    fi
    while IFS= read -r arg; do
        "check_${template}_arg" "$name" "$arg"
    done < <(values "$file" arg)

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
    if [ -n "$config" ]; then
        [ -f "$dir/$config" ] || fail "$name: declared config $config is missing"
        command -v perl >/dev/null 2>&1 || fail "$name: perl is needed to read $config"
        perl -e "$CONFIG_GRAMMAR" "$dir/$config" "$target" "$CONFIG_FIELDS" ||
            fail "$name: $config is not a data-only target definition"
    fi

    grep -qF "\`toolchain/recipes/$name/\`" "$root/NOTICE.md" ||
        fail "$name: no NOTICE.md entry naming \`toolchain/recipes/$name/\`"
}

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
    declare -A state=()
    check_cycle() {
        local at="$1" dep
        case "${state[$at]:-}" in
            done) return 0 ;;
            active) fail "$at: the recipes depend on each other in a cycle through it" ;;
        esac
        state[$at]=active
        for dep in $(values "$root/toolchain/recipes/$at/recipe" depends); do
            check_cycle "$dep"
        done
        state[$at]=done
    }
    for name in "${names[@]}"; do
        check_cycle "$name"
    done
    local built
    built="$(check_stamps "$root" "$out" "$root/scripts/build_recipes.sh" "${names[@]}")" || exit 1
    echo "$SELF: OK — ${#names[@]} recipes (${names[*]}); $built"
}

self_test() {
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN

    # alpha and beta take the tree's real args and target definition.
    local r="$tmp/toolchain/recipes" real
    mkdir -p "$r/alpha" "$r/beta" "$tmp/out" "$tmp/scripts"
    cat >"$r/alpha/recipe" <<'EOF'
# a comment
version=1.2.3
url=https://example.org/alpha-1.2.3.tar.xz
sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
license=MIT
template=cmake
soname=libalpha.so.1
EOF
    cat >"$r/beta/recipe" <<'EOF'
version=4.5
url=https://example.org/beta-4.5.tar.gz
sha256=fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210
license=Apache-2.0
template=openssl
config=target.conf
depends=alpha
soname=libbeta.so.3
EOF
    for real in "$REPO_ROOT"/toolchain/recipes/*/recipe; do
        case "$(values "$real" template)" in
            cmake) grep '^arg=' "$real" >>"$r/alpha/recipe" ;;
            openssl)
                grep '^arg=\|^target=' "$real" >>"$r/beta/recipe"
                cp "$(dirname "$real")/$(values "$real" config)" "$r/beta/target.conf"
                ;;
        esac
    done
    [ -f "$r/beta/target.conf" ] || fail "--self-test: the tree has no openssl recipe to take a target definition from"
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
    # A rejection for another reason would read the same, so check the reason.
    expect_reject() {
        local why
        if why="$(check_tree "$tmp" "$tmp/out" 2>&1 >/dev/null)"; then
            fail "--self-test: $1 was accepted"
        fi
        [ -z "${2:-}" ] || printf '%s\n' "$why" | grep -qF -- "$2" ||
            fail "--self-test: $1 was rejected, but not for '$2': $why"
    }
    reject_in() {
        cp "$1" "$tmp/saved"
        sed -i "$2" "$1"
        expect_reject "$3" "${4:-}"
        cp "$tmp/saved" "$1"
    }
    reject_edit() {
        reject_in "$r/alpha/recipe" "$@"
    }
    reject_beta() {
        reject_in "$r/beta/recipe" "$@"
    }
    reject_conf() {
        reject_in "$r/beta/target.conf" "$@"
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
    reject_edit '$ a depends=alpha' "a recipe that depends on itself" "in a cycle"
    reject_edit '$ a depends=beta' "two recipes that depend on each other" "in a cycle"
    reject_beta '$ a depends=alpha' "a second depends line" "more than one depends"
    reject_edit '/^soname=/d' "a recipe with no soname" "no soname"
    reject_edit 's|^soname=.*|soname=../../libz.so|' "a soname that is a path" "is not lib<name>.so"
    reject_edit '$ a cflags=-include /etc/shim.h' "a key the driver does not read" "not one of the keys"
    reject_edit '$ a config=x.conf' "a config on a cmake recipe" "are for the openssl template"

    # Each of these edits what is built without a file.
    reject_edit '$ a arg=-DCMAKE_C_FLAGS=-include /etc/shim.h -Dregcomp_l=my_regcomp' \
        "a header forced in through CMAKE_C_FLAGS" "not one of the CMake variables"
    reject_edit '$ a arg=-DCMAKE_PROJECT_INCLUDE=/etc/evil.cmake' \
        "a CMake script run inside project()" "not one of the CMake variables"
    reject_edit '$ a arg=-DCMAKE_TOOLCHAIN_FILE=/etc/other.cmake' \
        "a second toolchain file" "not one of the CMake variables"
    reject_edit '$ a arg=-DCMAKE_C_COMPILER_LAUNCHER=/usr/bin/sed' \
        "a compiler launcher" "not one of the CMake variables"
    reject_edit '$ a arg=-DCMAKE_MODULE_PATH=/etc/modules' \
        "a Find-module override" "not one of the CMake variables"
    reject_edit '$ a arg=-C/etc/init.cmake' "a -C cache script" "is not -D<NAME>=<value>"
    reject_edit '$ a arg=-DFOO:FILEPATH=/etc/x' "a typed -D" "is not -D<NAME>=<value>"
    reject_edit '$ a arg=-DZLIB_DIR=/etc/evil' "a package config search root" "names a flag, file"
    reject_edit '$ a arg=-DOPENSSL_ROOT_DIR=/etc/ssl' "a find root for a host package" "names a flag, file"
    reject_edit '$ a arg=-DOPENSSL_ROOT=/etc/ssl' "a find root without _DIR" "names a flag, file"
    reject_edit '$ a arg=-DFETCHCONTENT_SOURCE_DIR_FOO=/etc/x' "a FetchContent source override" "names a flag, file"
    reject_edit '$ a arg=-DFETCHCONTENT_FULLY_DISCONNECTED=ON' "a FetchContent setting" "names a flag, file"
    reject_edit '$ a arg=-DFOO_SOURCE_DIR_BAR=/etc/x' "a source directory override" "names a flag, file"
    reject_edit '$ a arg=-DFOO_PATH=/etc/x' "a search path" "names a flag, file"
    reject_edit '$ a arg=-DZLIB_LIBRARY_RELEASE=/etc/x' "a release library override" "names a flag, file"
    reject_edit '$ a arg=-DZLIB_LIBRARY_DEBUG=/etc/x' "a debug library override" "names a flag, file"
    reject_edit '$ a arg=-DEXTRA_CFLAGS=O2' "a project's flags variable" "names a flag, file"
    reject_edit '$ a arg=-DFOO=-include/etc/shim.h' "a flag passed as a value" "neither a word nor a path"
    reject_edit '$ a arg=-DFOO=ON;-include;/etc/shim.h' "a CMake list smuggling a flag" "neither a word nor a path"
    reject_edit '$ a arg=-DFOO=/usr/lib/libz.so' "a host path" "neither a word nor a path"
    reject_edit '$ a arg=-DFOO=/etc/../usr/lib' "a path climbing out of /etc" "neither a word nor a path"
    reject_edit '$ a arg=-DCMAKE_INSTALL_BINDIR=../bin' "an install directory outside the prefix" \
        "not relative to the prefix"

    reject_beta '$ a arg=-include /etc/shim.h' "a header forced in through Configure" "not an OpenSSL feature switch"
    reject_beta '$ a arg=-Dregcomp_l=my_regcomp' "a macro renaming a function" "not an OpenSSL feature switch"
    reject_beta '$ a arg=CFLAGS=-include/etc/shim.h' "a CFLAGS assignment" "not an OpenSSL feature switch"
    reject_beta '$ a arg=--config=/etc/evil.conf' "a second target definition" "not an OpenSSL feature switch"
    reject_beta '$ a arg=--openssldir=/usr/lib/ssl' "an openssldir on the host" "not an OpenSSL feature switch"
    reject_beta 's|^config=.*|config=../alpha/recipe|' "a config outside the recipe" "is not a file name"
    reject_beta '/^target=/d' "an openssl recipe with no target" "no target"

    reject_conf '1 i system("sed -i s/foo/bar/ crypto/x.c");' "a config that runs a command" "expected 'my'"
    reject_conf '$ a do "/etc/evil.pl";' "a config that loads more Perl" "text follows"
    reject_conf '/=> {/a CC => "sh -c evil",' "a compiler command" "'CC' is not a field"
    reject_conf '/=> {/a defines => add("regcomp_l=my_regcomp"),' "a define list" "'defines' is not a field"
    reject_conf '/=> {/a includes => [ "/etc/shim" ],' "an include directory" "'includes' is not a field"
    reject_conf '/=> {/a cppflags => "-include /etc/shim.h",' "a forced header" "not a flag a target definition may carry"
    reject_conf '/=> {/a cppflags => "-Dregcomp_l=my_regcomp",' "a macro renaming a function" \
        "not a flag a target definition may carry"
    reject_conf '/=> {/a cflags => "-I/etc/evil",' "a shadowing include path" "not a flag a target definition may carry"
    reject_conf '/=> {/a lflags => "-Wl,--wrap=regcomp",' "a symbol wrapped at link time" \
        "not a flag a target definition may carry"
    reject_conf '/=> {/a cflags => "@{[ system(q(true)) ]}",' "an interpolated command" "neither a plain string"
    reject_conf '/=> {/a cflags => `true`,' "a backtick command" "neither a plain string"
    reject_conf '/=> {/a cflags => sub { system("true") },' "a code reference" "'sub' in cflags is not a string"
    reject_conf '/=> {/a perlasm_scheme => "../../evil",' "a path where a word belongs" "is not a word"
    reject_conf 's/^\( *\)"[^"]*" => {/\1"other" => {/' "a definition of another target" "the recipe's target is"

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
    mv "$r/beta/target.conf" "$tmp/target.conf"
    expect_reject "a declared config that is missing"
    mv "$tmp/target.conf" "$r/beta/target.conf"
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

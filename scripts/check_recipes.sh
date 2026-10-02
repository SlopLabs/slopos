#!/usr/bin/env bash
# Hold every recipe under toolchain/recipes/ to the shape build_recipes.sh
# promises: a pinned upstream tarball built by a template, patched at most to
# name the target.
#
#   - one lowercase-hex `sha256` and one `https://` `url` naming `version`;
#   - a `license`, at least one `license_file` naming a file inside the
#     tarball, a known `template`, at least one `soname` or `program`, no
#     unread key;
#   - at most one `depends` line, naming other recipes, acyclic;
#   - no file but `recipe`, its declared `config` and its declared `patch`es;
#   - a patch has the shape of one that only teaches the project the target,
#     which review holds it to: it is named `NNNN-slopos-<what>.patch`; it
#     creates or extends files and never deletes, renames or changes the mode
#     of one; each run of removed lines is replaced where it stood, line for
#     line, by added lines that start with them; it patches each file it
#     names in one section, with at least one hunk; and every hunk names
#     SlopOS in a line it adds or in the path of the file it creates. A recipe
#     under a licence this tree may not hold the text of (GPL-2.0-only, CDDL)
#     carries none, since a diff's context is upstream's own lines. Anything
#     more an upstream build needs is a slibc or kernel finding;
#   - a patch that touches `Modules/Platform/` only creates files there, and
#     exactly the files of `toolchain/cmake/Platform/`, byte for byte, so the
#     platform CMake learns is the one the recipes configure with;
#   - every `arg` picks among upstream's options and carries no code, since a
#     flag, CMake script, launcher or search root edits what is built with
#     every file pristine. A `cmake` arg is `-D<NAME>=<value>`: `CMAKE_*`
#     names from `CMAKE_ARG_NAMES`, project names outside `PROJECT_ARG_DENY`
#     (bar `PROJECT_ARG_ALLOW`) and `PROJECT_ARTIFACT_DENY` (bar a
#     `PROJECT_SWITCH` set `ON` or `OFF`, which builds a program or library
#     or does not rather than naming one), values a word or an `/etc` path.
#     A `meson`
#     arg is the same shape: built-in options from `MESON_ARG_NAMES`, project
#     options outside `MESON_PROJECT_DENY`. An `openssl` arg is `no-*`,
#     `enable-*`, `shared`, `threads` or `--openssldir=/etc/..`. An
#     `autotools` arg is `--enable-<name>` or `--with-<name>`, with at most a
#     word for its value, `--disable-<name>` or `--without-<name>`, or
#     `<prefix>_cv_<name>=<word>`, a configure test's answer, other than one
#     for a program, a path, a precious variable or the system names the
#     template owns;
#   - an `openssl` `config`, which `Configure` evaluates as Perl, is data: one
#     `%targets` entry for `target`, `CONFIG_FIELDS` only, non-interpolating
#     strings, and only benign flags;
#   - a NOTICE.md entry naming `toolchain/recipes/<name>/`;
#   - a built recipe carries the stamp `build_recipes.sh --print-stamp` gives
#     now (skipped when nothing is built, as in CI);
#   - every object a built recipe bound to GPL-2.0-only installs (no exception
#     from `GPL2_LINKING_EXCEPTIONS`) reaches, through `DT_NEEDED`, only
#     libraries a recipe or the sysroot provides under a licence that code may
#     be combined with (`GPL2_COMPATIBLE`), and nothing in that closure lacks a
#     symbol table or defines, even locally, a symbol an incompatible recipe's
#     library exports: git links no OpenSSL, dynamically or as a static copy.
#
# The driver fails any build that changes the patched tree.
#
# Usage: check_recipes.sh
#        check_recipes.sh --self-test

set -euo pipefail

SELF="check_recipes"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

TEMPLATES="cmake meson openssl autotools"
RECIPE_KEYS="version url sha256 license license_file template depends soname program arg patch config target"

CMAKE_ARG_NAMES='^CMAKE_(BUILD_TYPE|POSITION_INDEPENDENT_CODE|(REQUIRE|DISABLE)_FIND_PACKAGE_[A-Za-z0-9_]+|USE_SYSTEM_[A-Z0-9_]+|INSTALL_(BINDIR|SBINDIR|LIBEXECDIR|SYSCONFDIR|DATAROOTDIR|DATADIR|INCLUDEDIR|DOCDIR|MANDIR)|DOC_DIR)$'
# CMake's own project names its install directories without `INSTALL_`.
CMAKE_DIR_NAMES='^CMAKE_(INSTALL_[A-Z]+|DOC_DIR)$'
AUTOTOOLS_SWITCH='^--(enable|disable|with|without)-([a-z0-9][a-z0-9_-]*)(=(.*))?$'
AUTOTOOLS_CACHE='^([a-z][a-z0-9]*)_cv_([A-Za-z0-9_]+)=(.*)$'
AUTOTOOLS_CACHE_DENY='^(prog_|path_|env_|host$|build$|target$)'
PATCH_NAME='^[0-9]{4}-slopos-[a-z0-9-]+\.patch$'
PROJECT_ARG_DENY='(FLAGS|DEFINITIONS|DEFINES|INCLUDE|LAUNCHER|COMPILER|LINKER|TOOLCHAIN|COMMAND|SCRIPT|MODULE|EXECUTABLE|FETCHCONTENT|_DIR|_ROOT|_PATH$|_FILE$|_HINTS?$)'
PROJECT_ARG_ALLOW='^[A-Z0-9]+_CA_PATH$'
PROJECT_ARTIFACT_DENY='(PROGRAM|_LIBRAR(Y|IES)(_RELEASE|_DEBUG)?$)'
PROJECT_SWITCH='^(USE|ENABLE|BUILD|WITH)_'
MESON_BUILTIN='^(prefix|bindir|datadir|includedir|infodir|libdir|licensedir|libexecdir|localedir|localstatedir|mandir|sbindir|sharedstatedir|sysconfdir|auto_features|backend|genvslite|buildtype|debug|default_library|default_both_libraries|errorlogs|install_umask|layout|optimization|prefer_static|stdsplit|strip|unity|unity_size|warning_level|werror|wrap_mode|force_fallback_for|vsenv|pkg_config_path|cmake_prefix_path)$|^(b|c|cpp|objc|objcpp|fortran|d|rust|cuda|cython|java|vala|nasm|masm|swift|python)_'
MESON_ARG_NAMES='^(auto_features|b_ndebug)$'
MESON_PROJECT_DENY='(args|flags|define|include|launcher|compiler|linker|toolchain|command|script|path|dir|file|bin|prefix|editor|pager|environment|shell|exe|program|tool)'
WORD='^[A-Za-z0-9_][A-Za-z0-9_.,+:-]*$'
ETC_PATH='^/etc(/[A-Za-z0-9_+-][A-Za-z0-9_.+-]*)+$'
RELATIVE_DIR='^[a-z0-9_]+(/[a-z0-9_+-][a-z0-9_.+-]*)*$'
FILE_NAME='^[A-Za-z0-9_][A-Za-z0-9_.-]*$'
TARBALL_FILE='^[A-Za-z0-9_][A-Za-z0-9_.+-]*(/[A-Za-z0-9_][A-Za-z0-9_.+-]*)*$'
SONAME='^lib[A-Za-z0-9_+-]+\.so(\.[0-9]+)*$'
PROGRAM='^(bin|sbin|libexec)(/[A-Za-z0-9_][A-Za-z0-9_.+-]*)+$'
CONFIG_FIELDS="inherit_from bn_ops asm_arch perlasm_scheme thread_scheme dso_scheme shared_target CFLAGS cflags CXXFLAGS cxxflags cppflags lib_cppflags lflags ex_libs shared_cflag shared_ldflag"
# SPDX ids. `A OR B` qualifies when either side does; `X WITH E` when X does,
# since an exception only adds permission, and for the one pairing whose
# exception grants exactly this.
GPL2_COMPATIBLE="MIT Zlib ISC BSD-2-Clause BSD-3-Clause curl GPL-2.0-only GPL-2.0-or-later LGPL-2.1-only LGPL-2.1-or-later"
GPL2_COMPATIBLE_WITH="Apache-2.0 WITH LLVM-exception"
# Exceptions that let a GPL-2.0-only work be combined with other licences.
GPL2_LINKING_EXCEPTIONS="GCC-exception-2.0"
# What the target sysroot links beside the recipes.
SYSROOT_LIBS="libc.so:MIT OR Apache-2.0
libc++.so:Apache-2.0 WITH LLVM-exception"

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
        if [[ "$var" =~ $CMAKE_DIR_NAMES ]]; then
            [[ "$value" =~ $RELATIVE_DIR ]] ||
                fail "$name: arg '$arg' names an install directory that is not relative to the prefix"
            return 0
        fi
    elif [[ "$var" =~ $PROJECT_ARG_DENY && ! "$var" =~ $PROJECT_ARG_ALLOW ]] ||
        [[ "$var" =~ $PROJECT_ARTIFACT_DENY && ! ("$var" =~ $PROJECT_SWITCH && "$value" =~ ^(ON|OFF)$) ]]; then
        fail "$name: arg '$arg' names a flag, file, program or search root"
    fi
    [[ "$value" =~ $WORD || "$value" =~ $ETC_PATH ]] ||
        fail "$name: arg '$arg' has a value that is neither a word nor a path under /etc"
}

check_meson_arg() {
    local name="$1" arg="$2" var value
    [[ "$arg" =~ ^-D([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] ||
        fail "$name: arg '$arg' is not -D<name>=<value>, the one form a meson recipe passes"
    var="${BASH_REMATCH[1]}"
    value="${BASH_REMATCH[2]}"
    if [[ "$var" =~ $MESON_BUILTIN ]]; then
        [[ "$var" =~ $MESON_ARG_NAMES ]] ||
            fail "$name: arg '$arg' sets $var, which is not one of the meson built-in options a recipe may set"
    elif [[ "${var,,}" =~ $MESON_PROJECT_DENY ]]; then
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

check_autotools_arg() {
    local name="$1" arg="$2" kind value
    if [[ "$arg" =~ $AUTOTOOLS_SWITCH ]]; then
        kind="${BASH_REMATCH[1]}"
        value="${BASH_REMATCH[3]-}"
        [ -z "$value" ] || [[ "$kind" == enable || "$kind" == with ]] ||
            fail "$name: arg '$arg' gives a value to a switch that turns something off"
        [ -z "$value" ] || [[ "${value#=}" =~ $WORD ]] ||
            fail "$name: arg '$arg' has a value that is not a word"
        return 0
    fi
    if [[ "$arg" =~ $AUTOTOOLS_CACHE ]]; then
        kind="${BASH_REMATCH[2]}"
        value="${BASH_REMATCH[3]}"
        [[ ! "$kind" =~ $AUTOTOOLS_CACHE_DENY ]] ||
            fail "$name: arg '$arg' answers for a program, a path or a system name"
        [[ "$value" =~ $WORD ]] ||
            fail "$name: arg '$arg' has a value that is not a word"
        return 0
    fi
    fail "$name: arg '$arg' is not an --enable/--disable/--with/--without switch or a <prefix>_cv_<name> answer"
}

# Holds a unified diff to the one shape a target patch has. Arguments: the
# patch, and the toolchain/cmake/Platform directory whose files a patch
# creating `Modules/Platform/` files must reproduce.
read -r -d '' PATCH_GRAMMAR <<'PERL' || true
use strict;
use warnings;
my ($file, $platform) = @ARGV;
sub bad { print STDERR "  $file: $_[0]\n"; exit 1 }
open(my $fh, '<', $file) or bad("cannot be read");
my @lines = <$fh>;
chomp @lines;
my (%created, %seen, $path, $creating, @ops, $in_hunk, $old_left, $new_left, $header, $hunks);
sub finish_hunk {
    return unless $in_hunk;
    bad("a hunk in $path ends early") if $old_left != 0 || $new_left != 0;
    my @added = map { $_->[1] } grep { $_->[0] eq '+' } @ops;
    for (my $at = 0; $at < @ops;) {
        if ($ops[$at][0] ne '-') { $at++; next }
        my (@gone, @came);
        push @gone, $ops[$at++][1] while $at < @ops && $ops[$at][0] eq '-';
        push @came, $ops[$at++][1] while $at < @ops && $ops[$at][0] eq '+';
        for my $n (0 .. $#gone) {
            (my $stem = $gone[$n]) =~ s/\s+$//;
            bad("$path loses '$gone[$n]', which the line added in its place does not start with")
                unless $n < @came && index($came[$n], $stem) == 0;
        }
    }
    bad("a hunk in $path does not name the target")
        unless ($creating && $path =~ /slopos/i) || grep { /slopos/i } @added;
    $created{$path} .= join('', map { "$_\n" } @added) if $creating;
    @ops = ();
    $in_hunk = 0;
}
for (my $i = 0; $i < @lines; $i++) {
    my $l = $lines[$i];
    if ($in_hunk && ($old_left > 0 || $new_left > 0)) {
        if ($l =~ /^-(.*)$/) { push @ops, ['-', $1]; $old_left--; next }
        if ($l =~ /^\+(.*)$/) { push @ops, ['+', $1]; $new_left--; next }
        if ($l =~ /^ (.*)$/ || $l eq '') { push @ops, [' ', $1 // '']; $old_left--; $new_left--; next }
        if ($l =~ /^\\ No newline/) { next }
        bad("'$l' is neither context nor an added or removed line");
    }
    if ($l =~ /^\\ No newline/) { next }
    if ($l =~ /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/) {
        finish_hunk();
        bad("a hunk before any file") unless defined $path;
        $hunks++;
        ($old_left, $new_left, $in_hunk) = (defined $2 ? $2 : 1, defined $4 ? $4 : 1, 1);
        next;
    }
    finish_hunk();
    if ($l =~ /^diff --git /) {
        bad("'$header' has no diff after it: a file made empty, or changed in mode alone")
            if defined $header;
        $header = $l;
        next;
    }
    if ($l =~ /^index [0-9a-f]+\.\.[0-9a-f]+(?: 100644)?$/) { next }
    if ($l =~ /^new file mode 100644$/) { next }
    if ($l =~ /^(deleted file mode|old mode|new mode|similarity index|rename from|rename to|copy from|copy to|Binary files|GIT binary patch)/) {
        bad("'$l': a target patch creates and extends files, nothing else");
    }
    if ($l =~ /^--- (\S+)/) {
        bad("$path is given no hunk") if defined $path && !$hunks;
        ($header, $hunks) = (undef, 0);
        my $old = $1;
        my $next = $i + 1 < @lines ? $lines[$i + 1] : '';
        bad("'$l' is not followed by its '+++' line") unless $next =~ /^\+\+\+ (\S+)/;
        my $new = $1;
        bad("$old is deleted") if $new eq '/dev/null';
        bad("'$new' is not a b/ path inside the tree")
            unless $new =~ m{^b/([A-Za-z0-9_.+/-]+)$} && $1 !~ m{(^|/)\.{0,2}(/|$)};
        $path = $1;
        $creating = $old eq '/dev/null';
        bad("$path is renamed from $old") if !$creating && $old ne "a/$path";
        bad("$path is patched in two sections") if $seen{$path}++;
        bad("$path is CMake's own platform file, which a patch only adds to")
            if !$creating && $path =~ m{^Modules/Platform/};
        $i++;
        next;
    }
    bad("'$l' is not part of a unified diff");
}
finish_hunk();
bad("'$header' has no diff after it: a file made empty, or changed in mode alone") if defined $header;
bad("no file is patched") unless defined $path;
bad("$path is given no hunk") unless $hunks;
my @platform_files = grep { m{^Modules/Platform/} } sort keys %created;
if (@platform_files) {
    opendir(my $dh, $platform) or bad("$platform cannot be read");
    my @want = sort grep { !/^\./ } readdir($dh);
    my @have = map { (my $n = $_) =~ s{^Modules/Platform/}{}; $n } @platform_files;
    bad("creates Modules/Platform/{" . join(',', @have) . "}, not toolchain/cmake/Platform's {" . join(',', @want) . "}")
        unless "@have" eq "@want";
    for my $name (@want) {
        open(my $pf, '<', "$platform/$name") or bad("$platform/$name cannot be read");
        my $body = do { local $/; <$pf> };
        bad("Modules/Platform/$name is not toolchain/cmake/Platform/$name")
            unless $body eq $created{"Modules/Platform/$name"};
    }
}
PERL

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

    local version url sha256 license template config target dep soname program arg
    # `|| exit`: a self-test runs this under `if`, where `set -e` is off.
    version="$(single "$file" version "$name")" || exit 1
    url="$(single "$file" url "$name")" || exit 1
    sha256="$(single "$file" sha256 "$name")" || exit 1
    license="$(single "$file" license "$name")" || exit 1
    [ -n "$(values "$file" license_file)" ] || fail "$name: no license_file"
    local text
    while IFS= read -r text; do
        [[ "$text" =~ $TARBALL_FILE ]] || fail "$name: license_file '$text' is not a path inside the tarball"
    done < <(values "$file" license_file)
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
    [ -n "$(values "$file" soname)$(values "$file" program)" ] || fail "$name: no soname or program"
    while IFS= read -r soname; do
        [[ "$soname" =~ $SONAME ]] || fail "$name: soname '$soname' is not lib<name>.so[.<n>...]"
    done < <(values "$file" soname)
    while IFS= read -r program; do
        [[ "$program" =~ $PROGRAM ]] || fail "$name: program '$program' is not a path under bin/, sbin/ or libexec/"
    done < <(values "$file" program)

    local patch
    # A diff carries upstream's own lines, which this tree may not hold under
    # a licence GPL-3.0-or-later code cannot take in.
    if [ -n "$(values "$file" patch)" ] && [[ "$license" =~ GPL-2\.0-only|GPL-2\.0$|CDDL ]]; then
        fail "$name: a patch copies $license lines into this tree"
    fi
    while IFS= read -r patch; do
        [[ "$patch" =~ $PATCH_NAME ]] || fail "$name: patch '$patch' is not named NNNN-slopos-<what>.patch"
        [ -f "$dir/$patch" ] || fail "$name: declared patch $patch is missing"
        command -v perl >/dev/null 2>&1 || fail "$name: perl is needed to read $patch"
        perl -e "$PATCH_GRAMMAR" "$dir/$patch" "$root/toolchain/cmake/Platform" ||
            fail "$name: $patch does more than teach the project the target"
    done < <(values "$file" patch)

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
        [ "$base" = recipe ] && continue
        [ -n "$config" ] && [ "$base" = "$config" ] && continue
        values "$file" patch | grep -qxF -- "$base" && continue
        fail "$name: $base is neither the recipe nor a file it declares"
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

# Whether some `OR` alternative of the SPDX expression `$1` has every `AND`
# term satisfy the predicate `$2`. A parenthesised expression has none.
some_alternative_all() {
    local alt term held
    case "$1" in *"("*) return 1 ;; esac
    while IFS= read -r alt; do
        held=1
        while IFS= read -r term; do
            "$2" "$term" || held=0
        done < <(printf '%s\n' "$alt" | sed 's/ AND /\n/g')
        [ "$held" -eq 0 ] || return 0
    done < <(printf '%s\n' "$1" | sed 's/ OR /\n/g')
    return 1
}

term_gpl2_compatible() {
    [ "$1" != "$GPL2_COMPATIBLE_WITH" ] || return 0
    case " $GPL2_COMPATIBLE " in *" ${1%% WITH *} "*) return 0 ;; esac
    return 1
}

term_free_of_gpl2_only() {
    case "$1" in
        GPL-2.0-only | GPL-2.0) return 1 ;;
        "GPL-2.0-only WITH "* | "GPL-2.0 WITH "*)
            case " $GPL2_LINKING_EXCEPTIONS " in *" ${1##* WITH } "*) return 0 ;; esac
            return 1
            ;;
    esac
}

gpl2_compatible() {
    some_alternative_all "$1" term_gpl2_compatible
}

binds_gpl2_only() {
    ! some_alternative_all "$1" term_free_of_gpl2_only
}

sysroot_licence() {
    printf '%s\n' "$SYSROOT_LIBS" | awk -F: -v lib="$1" '$1 == lib { print $2; exit }'
}

needed_of() {
    readelf -d "$1" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p'
}

# Every function and datum `$1` defines, local ones included: a copy linked
# with hidden visibility or behind a version script defines only those.
definitions() {
    readelf -sW "$1" | awk '
        NF >= 8 && $1 ~ /:$/ && $7 != "UND" && ($4 == "FUNC" || $4 == "OBJECT") {
            name = $8; sub(/@.*/, "", name); sub(/\..*/, "", name); print name }' |
        LC_ALL=C sort -u
}

has_symtab() {
    readelf -SW "$1" | grep -q ' \.symtab '
}

check_closures() {
    local root="$1" out="$2" name soname rel file lib lic from needed copied objects=0 bad=0
    local -A owner=() licence=() seen=() exports=() defined=()
    local queue=() closure=()
    for name in "${@:3}"; do
        licence[$name]="$(values "$root/toolchain/recipes/$name/recipe" license)"
        for soname in $(values "$root/toolchain/recipes/$name/recipe" soname); do
            owner[$soname]="$name"
        done
    done
    local bound=()
    for name in "${@:3}"; do
        [ -f "$out/$name/stamp" ] && [ -f "$out/$name/manifest" ] || continue
        binds_gpl2_only "${licence[$name]}" && bound+=("$name")
    done
    [ "${#bound[@]}" -gt 0 ] || { echo "no GPL-2.0-only object built"; return 0; }
    command -v readelf >/dev/null 2>&1 || fail "readelf is needed to read what ${bound[*]} link"
    # What a static copy of an incompatible library would define.
    for soname in "${!owner[@]}"; do
        name="${owner[$soname]}"
        gpl2_compatible "${licence[$name]}" || [ ! -e "$out/prefix/lib/$soname" ] ||
            exports[$soname]="$(readelf --dyn-syms -W "$out/prefix/lib/$soname" | awk '
                NF >= 8 && $1 ~ /:$/ && $7 != "UND" && ($5 == "GLOBAL" || $5 == "WEAK") {
                    name = $8; sub(/@.*/, "", name); print name }' | LC_ALL=C sort -u)"
    done
    for name in "${bound[@]}"; do
        while IFS= read -r rel; do
            file="$out/prefix/$rel"
            [ -f "$file" ] && [ ! -L "$file" ] && [ "$(od -An -c -N4 "$file" | tr -d ' ')" = '177ELF' ] ||
                continue
            objects=$((objects + 1))
            seen=()
            closure=("$file")
            needed="$(needed_of "$file")" || {
                echo "  $name: $rel cannot be read" >&2
                bad=1
                continue
            }
            mapfile -t queue < <(printf '%s' "$needed" | sed '/^$/d')
            while [ "${#queue[@]}" -gt 0 ]; do
                lib="${queue[0]}"
                queue=("${queue[@]:1}")
                [ -z "${seen[$lib]:-}" ] || continue
                seen[$lib]=1
                lic="$(sysroot_licence "$lib")"
                if [ -n "$lic" ]; then
                    from="the sysroot"
                elif [ -n "${owner[$lib]:-}" ]; then
                    from="${owner[$lib]}"
                    lic="${licence[$from]}"
                    if needed="$(needed_of "$out/prefix/lib/$lib" 2>/dev/null)"; then
                        closure+=("$out/prefix/lib/$lib")
                        mapfile -t -O "${#queue[@]}" queue < <(printf '%s' "$needed" | sed '/^$/d')
                    else
                        echo "  $name: $rel reaches $lib, which $from declares but has not installed" >&2
                        bad=1
                    fi
                else
                    echo "  $name: $rel reaches $lib, which neither a recipe nor the sysroot provides" >&2
                    bad=1
                    continue
                fi
                gpl2_compatible "$lic" || {
                    echo "  $name: $rel reaches $lib ($from, $lic), which GPL-2.0-only code cannot be combined with" >&2
                    bad=1
                }
            done
            for lib in "${closure[@]}"; do
                has_symtab "$lib" || {
                    echo "  $name: ${lib#"$out/prefix/"} carries no symbol table, so what it copied cannot be read" >&2
                    bad=1
                    continue
                }
                [ -n "${defined[$lib]+set}" ] || defined[$lib]="$(definitions "$lib")"
                for soname in "${!exports[@]}"; do
                    [ "$(basename "$lib")" != "$soname" ] || continue
                    copied="$(LC_ALL=C comm -12 <(printf '%s\n' "${defined[$lib]}") \
                        <(printf '%s\n' "${exports[$soname]}") | sed '/^$/d' | head -n 3 | tr '\n' ' ')"
                    [ -z "$copied" ] || {
                        echo "  $name: ${lib#"$out/prefix/"} defines ${copied% }, which ${owner[$soname]}'s $soname exports: a static copy of code GPL-2.0-only code cannot be combined with" >&2
                        bad=1
                    }
                done
            done
        done <"$out/$name/manifest"
    done
    [ "$bad" -eq 0 ] || fail "a GPL-2.0-only recipe links what its licence does not allow"
    echo "$objects GPL-2.0-only objects link compatible code only"
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
    local built closures
    built="$(check_stamps "$root" "$out" "$root/scripts/build_recipes.sh" "${names[@]}")" || exit 1
    closures="$(check_closures "$root" "$out" "${names[@]}")" || exit 1
    echo "$SELF: OK — ${#names[@]} recipes (${names[*]}); $built; $closures"
}

self_test() {
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN

    # alpha, beta, delta and gamma take the tree's real args and target
    # definition; alpha carries a patch shaped as a target patch is.
    local r="$tmp/toolchain/recipes" real
    mkdir -p "$r/alpha" "$r/beta" "$r/delta" "$r/gamma" "$tmp/out" "$tmp/scripts" \
        "$tmp/toolchain/cmake/Platform"
    printf 'set(SLOPOS 1)\n' >"$tmp/toolchain/cmake/Platform/SlopOS.cmake"
    cat >"$r/alpha/recipe" <<'EOF'
# a comment
version=1.2.3
url=https://example.org/alpha-1.2.3.tar.xz
sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
license=MIT
license_file=LICENSE
template=cmake
soname=libalpha.so.1
patch=0001-slopos-port.patch
EOF
    cat >"$r/alpha/0001-slopos-port.patch" <<'EOF'
diff --git a/src/os.h b/src/os.h
--- a/src/os.h
+++ b/src/os.h
@@ -1,3 +1,4 @@
 #if defined(__linux__) || \
-    defined(__FreeBSD__)
+    defined(__FreeBSD__) || \
+    defined(__slopos__)
 #endif
diff --git a/Modules/Platform/SlopOS.cmake b/Modules/Platform/SlopOS.cmake
new file mode 100644
--- /dev/null
+++ b/Modules/Platform/SlopOS.cmake
@@ -0,0 +1 @@
+set(SLOPOS 1)
EOF
    cat >"$r/gamma/recipe" <<'EOF'
version=2.0
url=https://example.org/gamma-2.0.tar.gz
sha256=1111111111111111111111111111111111111111111111111111111111111111
license=GPL-3.0-or-later
license_file=COPYING
template=autotools
program=bin/gamma
EOF
    cat >"$r/beta/recipe" <<'EOF'
version=4.5
url=https://example.org/beta-4.5.tar.gz
sha256=fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210
license=Apache-2.0
license_file=LICENSE.txt
template=openssl
config=target.conf
depends=alpha
soname=libbeta.so.3
EOF
    cat >"$r/delta/recipe" <<'EOF'
version=0.9
url=https://example.org/delta-0.9.tar.xz
sha256=abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789
license=GPL-2.0-only
license_file=COPYING
license_file=docs/LGPL-2.1
template=meson
depends=alpha
program=bin/delta
EOF
    for real in "$REPO_ROOT"/toolchain/recipes/*/recipe; do
        case "$(values "$real" template)" in
            cmake) grep '^arg=' "$real" >>"$r/alpha/recipe" ;;
            meson) grep '^arg=' "$real" >>"$r/delta/recipe" ;;
            autotools) grep '^arg=' "$real" >>"$r/gamma/recipe" ;;
            openssl)
                grep '^arg=\|^target=' "$real" >>"$r/beta/recipe"
                cp "$(dirname "$real")/$(values "$real" config)" "$r/beta/target.conf"
                ;;
        esac
    done
    [ -f "$r/beta/target.conf" ] || fail "--self-test: the tree has no openssl recipe to take a target definition from"
    notice() {
        printf '`toolchain/recipes/alpha/`, `toolchain/recipes/beta/`, `toolchain/recipes/delta/`, `toolchain/recipes/gamma/`\n' >"$tmp/NOTICE.md"
    }
    notice
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
    reject_delta() {
        reject_in "$r/delta/recipe" "$@"
    }
    reject_conf() {
        reject_in "$r/beta/target.conf" "$@"
    }
    reject_gamma() {
        reject_in "$r/gamma/recipe" "$@"
    }
    reject_patch() {
        reject_in "$r/alpha/0001-slopos-port.patch" "$@"
    }
    # The whole patch from stdin, for a shape no sed of the fixture makes.
    reject_patch_as() {
        cp "$r/alpha/0001-slopos-port.patch" "$tmp/saved"
        cat >"$r/alpha/0001-slopos-port.patch"
        expect_reject "$1" "$2"
        cp "$tmp/saved" "$r/alpha/0001-slopos-port.patch"
    }

    expect_ok "a well-formed pair of recipes with nothing built"

    reject_edit 's/^sha256=.*/sha256=0123456789abcdef/' "a short sha256"
    reject_edit 's/^sha256=0/sha256=G/' "a sha256 with a non-hex digit"
    reject_edit 's/^sha256=01/sha256=AB/' "an uppercase sha256"
    reject_edit '/^sha256=/d' "a recipe with no sha256"
    reject_edit 's/^sha256=.*/&\n&/' "a recipe with two sha256 lines"
    reject_edit 's|^url=https:|url=http:|' "a plain-http url"
    reject_edit 's|alpha-1.2.3|alpha-1.2.4|' "a url naming another version"
    reject_edit 's/^template=.*/template=scons/' "an unknown template"
    reject_edit '/^license=/d' "a recipe with no license"
    reject_edit '/^license_file=/d' "a recipe with no licence text" "no license_file"
    reject_edit 's|^license_file=.*|license_file=/etc/LICENSE|' "an absolute licence path" "not a path inside the tarball"
    reject_edit 's|^license_file=.*|license_file=../LICENSE|' "a licence path climbing out" "not a path inside the tarball"
    reject_edit 's|^license_file=.*|license_file=a/../../LICENSE|' "a licence path climbing out midway" "not a path inside the tarball"
    reject_edit '$ a depends=epsilon' "a dependency on no recipe"
    reject_edit '$ a depends=alpha' "a recipe that depends on itself" "in a cycle"
    reject_edit '$ a depends=beta' "two recipes that depend on each other" "in a cycle"
    reject_beta '$ a depends=alpha' "a second depends line" "more than one depends"
    reject_edit '/^soname=/d' "a recipe with no soname or program" "no soname or program"
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
    reject_edit '$ a arg=-DUSE_ZLIB_LIBRARY=/etc/libz.so' "a switch-shaped name given a path" "names a flag, file"
    reject_edit '$ a arg=-DWITH_PERL_EXECUTABLE=perl' "a switch-shaped name given a program" "names a flag, file"
    reject_edit '$ a arg=-DFOO_EXECUTABLE=ON' "a program name given a boolean" "names a flag, file"
    reject_edit '$ a arg=-DUSE_COMPILER_LAUNCHER=ON' "a switch that turns on a launcher" "names a flag, file"
    reject_edit '$ a arg=-DBUILD_FETCHCONTENT_DEPS=ON' "a switch that fetches sources" "names a flag, file"
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

    reject_delta '$ a arg=-Dc_args=-include/etc/shim.h' "a header forced in through c_args" \
        "not one of the meson built-in options"
    reject_delta '$ a arg=-Dc_link_args=-Wl,--wrap=regcomp' "a symbol wrapped at link time" \
        "not one of the meson built-in options"
    reject_delta '$ a arg=-Dprefix=/etc' "a second prefix" "not one of the meson built-in options"
    reject_delta '$ a arg=-Dwrap_mode=forcefallback' "a bundled dependency" "not one of the meson built-in options"
    reject_delta '$ a arg=-Dpkg_config_path=/etc/pc' "a pkg-config search root" "not one of the meson built-in options"
    reject_delta '$ a arg=-Dbuild.c_args=x' "a build-machine option" "is not -D<name>=<value>"
    reject_delta '$ a arg=-Dzlib:tests=true' "a subproject option" "is not -D<name>=<value>"
    reject_delta '$ a arg=--cross-file=/etc/other.ini' "a second cross file" "is not -D<name>=<value>"
    reject_delta '$ a arg=-Dsane_tool_path=/etc/bin' "a project search path" "names a flag, file"
    reject_delta '$ a arg=-Ddefault_pager=cat' "a project program" "names a flag, file"
    reject_delta '$ a arg=-Dcontrib=[completion]' "a meson array literal" "neither a word nor a path"
    reject_delta 's|^program=.*|program=../bin/delta|' "a program outside the prefix" "not a path under bin/"
    reject_delta 's|^program=.*|program=/bin/delta|' "an absolute program path" "not a path under bin/"
    reject_delta 's|^program=.*|program=share/delta|' "a program outside bin/ and libexec/" "not a path under bin/"
    reject_delta '$ a target=x' "a target on a meson recipe" "are for the openssl template"

    reject_gamma '$ a arg=--prefix=/etc' "a second prefix" "is not an --enable"
    reject_gamma '$ a arg=CFLAGS=-include/etc/shim.h' "a CFLAGS assignment" "is not an --enable"
    reject_gamma '$ a arg=CC=gcc' "a compiler" "is not an --enable"
    reject_gamma '$ a arg=ac_cv_prog_CC=gcc' "a program found by a cache answer" "answers for a program"
    reject_gamma '$ a arg=ac_cv_path_PERL=perl' "a path found by a cache answer" "answers for a program"
    reject_gamma '$ a arg=ac_cv_host=x86_64-pc-linux-gnu' "a system name the template owns" "answers for a program"
    reject_gamma '$ a arg=ac_cv_env_CFLAGS_value=-O0' "a precious variable" "answers for a program"
    reject_gamma '$ a arg=--with-curses=/usr/lib' "a host path as a switch's value" "not a word"
    reject_gamma '$ a arg=--enable-foo=-include/etc/shim.h' "a flag as a switch's value" "not a word"
    reject_gamma '$ a arg=--disable-foo=yes' "a value on a switch that turns something off" "turns something off"
    reject_gamma '$ a arg=bash_cv_x=a;b' "a cache answer that is not a word" "not a word"
    reject_edit '$ a arg=-DCMAKE_DOC_DIR=../doc' "a documentation directory outside the prefix" \
        "not relative to the prefix"

    reject_patch_as "a patch that deletes a line" "loses '    b();'" <<'EOF'
--- a/src/os.h
+++ b/src/os.h
@@ -1,3 +1,3 @@
 a(); /* slopos */
-    b();
+/* slopos */
 c();
EOF
    reject_patch_as "two removed lines one added line extends" "loses '}'" <<'EOF'
--- a/src/os.h
+++ b/src/os.h
@@ -1,3 +1,2 @@
 a();
-}
-}
+} /* slopos */
EOF
    reject_patch_as "a line moved past another" "loses '#endif'" <<'EOF'
--- a/src/os.h
+++ b/src/os.h
@@ -1,2 +1,2 @@
-#endif
 b();
+#endif /* slopos */
EOF
    reject_patch_as "a hunk that names the target only in context" "does not name the target" <<'EOF'
--- a/src/os.h
+++ b/src/os.h
@@ -1,1 +1,2 @@
 #if defined(__slopos__)
+#define PORTED 1
EOF
    reject_patch_as "an edit to CMake's own platform file" "a patch only adds to" <<'EOF'
--- a/Modules/Platform/UnixPaths.cmake
+++ b/Modules/Platform/UnixPaths.cmake
@@ -1,1 +1,2 @@
 set(UNIX 1)
+set(SLOPOS_EXTRA 1)
EOF
    reject_patch_as "one file patched in two sections" "patched in two sections" <<'EOF'
--- a/src/os.h
+++ b/src/os.h
@@ -1,1 +1,2 @@
 a();
+b(); /* slopos */
--- a/src/os.h
+++ b/src/os.h
@@ -2,1 +2,2 @@
 b(); /* slopos */
+c(); /* slopos */
EOF
    reject_patch_as "a path with a '.' component" "not a b/ path" <<'EOF'
--- a/src/./os.h
+++ b/src/./os.h
@@ -1,1 +1,2 @@
 a();
+b(); /* slopos */
EOF
    reject_patch_as "a hunk before any file" "a hunk before any file" <<'EOF'
@@ -1,1 +1,2 @@
 a();
+b(); /* slopos */
EOF
    reject_patch_as "a '---' line with no '+++'" "is not followed by its '+++' line" <<'EOF'
--- a/src/os.h
@@ -1,1 +1,2 @@
EOF
    reject_patch_as "a hunk that ends early" "ends early" <<'EOF'
--- a/src/os.h
+++ b/src/os.h
@@ -1,2 +1,3 @@
 a();
+b(); /* slopos */
EOF
    reject_patch_as "a line outside any diff" "is not part of a unified diff" <<'EOF'
Subject: [PATCH] port
EOF
    reject_patch_as "an empty patch" "no file is patched" </dev/null
    reject_patch_as "a file created empty" "has no diff after it" <<'EOF'
diff --git a/src/slopos.h b/src/slopos.h
new file mode 100644
index 0000000..e69de29
diff --git a/src/os.h b/src/os.h
--- a/src/os.h
+++ b/src/os.h
@@ -1,1 +1,2 @@
 a();
+b(); /* slopos */
EOF
    reject_patch_as "a file created empty at the end" "has no diff after it" <<'EOF'
--- a/src/os.h
+++ b/src/os.h
@@ -1,1 +1,2 @@
 a();
+b(); /* slopos */
diff --git a/src/slopos.h b/src/slopos.h
new file mode 100644
EOF
    reject_patch_as "a file header with no hunk" "is given no hunk" <<'EOF'
--- /dev/null
+++ b/src/slopos.h
--- a/src/os.h
+++ b/src/os.h
@@ -1,1 +1,2 @@
 a();
+b(); /* slopos */
EOF
    reject_delta '$ a patch=0001-slopos-port.patch' "a patch on a GPL-2.0-only recipe" "copies GPL-2.0-only lines"
    reject_patch_as "a mode change" "creates and extends files" <<'EOF'
diff --git a/configure b/configure
old mode 100644
new mode 100755
EOF
    reject_patch_as "a new file created executable" "is not part of a unified diff" <<'EOF'
diff --git a/src/slopos.c b/src/slopos.c
new file mode 100755
--- /dev/null
+++ b/src/slopos.c
@@ -0,0 +1 @@
+int x;
EOF
    reject_patch 's/defined(__slopos__)/defined(__plan9__)/' "a hunk that does not name the target" \
        "does not name the target"
    reject_patch 's/^+    defined(__FreeBSD__) || \\$/+    defined(__NetBSD__) || \\/' \
        "a line rewritten rather than extended" "which the line added in its place does not start with"
    reject_patch 's|^+++ b/src/os.h$|+++ /dev/null|' "a patch that deletes a file" "is deleted"
    reject_patch 's|^--- a/src/os.h$|--- a/src/old.h|' "a patch that renames a file" "is renamed from"
    reject_patch '0,/^new file mode 100644$/s//deleted file mode 100644/' "a deleted-file header" \
        "creates and extends files"
    reject_patch 's|^+++ b/src/os.h$|+++ b/../etc/os.h|' "a path climbing out of the tree" "not a b/ path"
    reject_patch 's/^+set(SLOPOS 1)$/+set(SLOPOS 2)/' "a platform file unlike toolchain/cmake's" \
        "is not toolchain/cmake/Platform/SlopOS.cmake"
    printf 'set(SLOPOS_C 1)\n' >"$tmp/toolchain/cmake/Platform/SlopOS-C.cmake"
    expect_reject "a platform module the patch does not create" "not toolchain/cmake/Platform's"
    rm "$tmp/toolchain/cmake/Platform/SlopOS-C.cmake"
    cp "$r/alpha/0001-slopos-port.patch" "$tmp/saved"
    printf '%s\n' 'diff --git a/Modules/Platform/Extra.cmake b/Modules/Platform/Extra.cmake' \
        'new file mode 100644' '--- /dev/null' '+++ b/Modules/Platform/Extra.cmake' '@@ -0,0 +1 @@' \
        '+set(SLOPOS_EXTRA 1)' >>"$r/alpha/0001-slopos-port.patch"
    expect_reject "a platform module toolchain/cmake does not carry" "not toolchain/cmake/Platform's"
    cp "$tmp/saved" "$r/alpha/0001-slopos-port.patch"
    reject_edit 's/^patch=.*/patch=port.patch/' "a patch named other than NNNN-slopos-*" "is not named NNNN-slopos"
    reject_edit 's/^patch=.*/patch=0002-slopos-other.patch/' "a declared patch that is missing" "is missing"
    reject_edit '/^patch=/d' "a patch the recipe does not declare" "neither the recipe nor a file it declares"

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

    : >"$r/alpha/0002-slopos-fix.patch"
    expect_reject "an undeclared patch beside a recipe"
    rm "$r/alpha/0002-slopos-fix.patch"
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
    notice

    mkdir -p "$tmp/out/alpha"
    echo good >"$tmp/out/alpha/stamp"
    expect_ok "a built recipe whose stamp is current"
    echo stale >"$tmp/out/alpha/stamp"
    expect_reject "a built recipe with a stale stamp"
    echo good >"$tmp/out/alpha/stamp"
    printf '#!/bin/sh\nexit 1\n' >"$tmp/scripts/build_recipes.sh"
    expect_reject "a built recipe whose driver cannot print a stamp"
    printf '#!/bin/sh\nshift\nfor n; do echo "$n good"; done\n' >"$tmp/scripts/build_recipes.sh"

    # delta is GPL-2.0-only; alpha is MIT, beta Apache-2.0.
    local lld llc prefix="$tmp/out/prefix"
    lld="$("$SCRIPT_DIR/llvm_tool.sh" rust-lld)" || fail "--self-test: no rust-lld to link its objects with"
    llc="$("$SCRIPT_DIR/llvm_tool.sh" llc)" || fail "--self-test: no llc to make its objects with"
    : >"$tmp/empty.ll"
    printf 'define void @beta_api() {\n  ret void\n}\n' >"$tmp/beta.ll"
    printf 'define hidden void @beta_api() {\n  ret void\n}\n' >"$tmp/hidden.ll"
    for unit in empty beta hidden; do
        "$llc" -filetype=obj -mtriple=x86_64-unknown-linux-gnu "$tmp/$unit.ll" -o "$tmp/$unit.o"
    done
    # `$1` the object, `$2` its soname or empty for a program, `$3` its code,
    # then what it needs.
    make_object() {
        local path="$1" soname="$2" code="$3"
        shift 3
        mkdir -p "$(dirname "$path")"
        "$lld" -flavor gnu -shared ${soname:+-soname "$soname"} "$tmp/$code.o" "$@" -o "$path"
    }
    make_object "$tmp/sysroot/libc.so" libc.so empty
    make_object "$tmp/sysroot/libc++.so" libc++.so empty
    make_object "$prefix/lib/libbeta.so.3" libbeta.so.3 beta "$tmp/sysroot/libc.so"
    make_object "$prefix/lib/libalpha.so.1" libalpha.so.1 empty "$tmp/sysroot/libc.so"
    make_object "$tmp/mystery/libmystery.so.1" libmystery.so.1 empty
    mkdir -p "$tmp/out/delta" "$tmp/out/beta" "$prefix/share/delta"
    echo good >"$tmp/out/delta/stamp"
    echo good >"$tmp/out/beta/stamp"
    printf '%s\n' bin/delta share/delta/README >"$tmp/out/delta/manifest"
    printf '%s\n' lib/libbeta.so.3 >"$tmp/out/beta/manifest"
    echo "not an object" >"$prefix/share/delta/README"
    delta_needs() {
        make_object "$prefix/bin/delta" "" empty "$@"
    }
    as_delta() {
        sed -i "s/^license=.*/license=$1/" "$r/delta/recipe"
    }
    as_beta() {
        sed -i "s/^license=.*/license=$1/" "$r/beta/recipe"
    }

    delta_needs "$prefix/lib/libalpha.so.1" "$tmp/sysroot/libc.so"
    expect_ok "a GPL-2.0-only program linking an MIT library and the C library"
    delta_needs "$tmp/sysroot/libc++.so"
    expect_ok "a GPL-2.0-only program linking the C++ runtime"
    delta_needs "$prefix/lib/libbeta.so.3"
    expect_reject "a GPL-2.0-only program linking an Apache-2.0 library" \
        "reaches libbeta.so.3 (beta, Apache-2.0)"
    as_beta "MIT AND Apache-2.0"
    expect_reject "a library under an AND with an incompatible side" "cannot be combined with"
    as_beta "Apache-2.0 OR GPL-2.0-or-later"
    expect_ok "a GPL-2.0-only program linking a library dual-licensed GPL-2.0-or-later"
    as_beta "MIT AND BSD-3-Clause"
    expect_ok "a GPL-2.0-only program linking a library under two compatible licences"
    as_beta "(MIT OR Apache-2.0)"
    expect_reject "a library under an expression the gate cannot read" "cannot be combined with"
    as_beta "Apache-2.0"
    as_delta "GPL-2.0-only WITH GCC-exception-2.0"
    expect_ok "a program whose exception allows the combination"
    as_delta "GPL-2.0-only WITH Autoconf-exception-2.0"
    expect_reject "a program whose exception does not allow the combination" "cannot be combined with"
    as_delta "GPL-2.0"
    expect_reject "a program under the deprecated GPL-2.0 id" "cannot be combined with"
    as_delta "GPL-2.0-only AND BSD-3-Clause"
    expect_reject "a program partly GPL-2.0-only" "cannot be combined with"
    as_delta "GPL-2.0-only OR MIT"
    expect_ok "a program a distributor may take as MIT"
    as_delta "GPL-2.0-only"
    make_object "$prefix/lib/libalpha.so.1" libalpha.so.1 empty "$prefix/lib/libbeta.so.3"
    delta_needs "$prefix/lib/libalpha.so.1"
    expect_reject "a GPL-2.0-only program reaching an Apache-2.0 library through another" \
        "bin/delta reaches libbeta.so.3 (beta, Apache-2.0)"
    make_object "$prefix/bin/delta" "" beta "$tmp/sysroot/libc.so"
    expect_reject "a GPL-2.0-only program carrying a static copy of an Apache-2.0 library" \
        "bin/delta defines beta_api, which beta's libbeta.so.3 exports"
    make_object "$prefix/bin/delta" "" hidden "$tmp/sysroot/libc.so"
    expect_reject "a static copy linked with hidden visibility" \
        "bin/delta defines beta_api, which beta's libbeta.so.3 exports"
    make_object "$prefix/bin/delta" "" beta "$tmp/sysroot/libc.so" --strip-all
    expect_reject "a GPL-2.0-only program with no symbol table to read" "carries no symbol table"
    make_object "$prefix/lib/libalpha.so.1" libalpha.so.1 beta "$tmp/sysroot/libc.so"
    delta_needs "$prefix/lib/libalpha.so.1"
    expect_reject "a library a GPL-2.0-only program reaches carrying a static copy" \
        "lib/libalpha.so.1 defines beta_api, which beta's libbeta.so.3 exports"
    make_object "$prefix/lib/libalpha.so.1" libalpha.so.1 empty "$tmp/sysroot/libc.so"
    delta_needs "$tmp/mystery/libmystery.so.1"
    expect_reject "a GPL-2.0-only program linking a library nothing here provides" \
        "neither a recipe nor the sysroot"
    printf '\177ELF, and nothing else\n' >"$prefix/bin/delta"
    expect_reject "a GPL-2.0-only object readelf cannot read" "bin/delta cannot be read"
    rm "$prefix/lib/libalpha.so.1"
    make_object "$tmp/alpha.so" libalpha.so.1 empty
    delta_needs "$tmp/alpha.so"
    expect_reject "a GPL-2.0-only program linking a library its recipe did not install" \
        "alpha declares but has not installed"
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

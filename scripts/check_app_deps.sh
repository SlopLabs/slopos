#!/usr/bin/env bash
# Fail when a crate under apps/ depends on anything but the crates an app is
# written against: appkit, editor-core and image. The kernel ABI, the drawing
# internals or the compositor's connection beneath appkit change when the
# base does, and an app naming one would have to change with it.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_app_deps "$@"

ALLOWED="slopos-appkit slopos-editor-core slopos-image"

# `<tag>\t<package>: <dependency>` for each dependency, but a dev one, of a
# package under <root>/apps that is not allowed; tag 2 when none is there.
scan() {
    local root="$1"
    # Run from the repository so its toolchain pin picks the cargo.
    (cd "$REPO_ROOT" && "${CARGO:-cargo}" metadata --no-deps --format-version 1 --offline \
        --manifest-path "$root/Cargo.toml") |
        python3 -I -c '
import json, os, sys
apps = os.path.join(os.path.realpath(sys.argv[1]), "apps") + os.sep
allowed = set(sys.argv[2].split())
found = 0
for pkg in json.load(sys.stdin)["packages"]:
    if not os.path.realpath(pkg["manifest_path"]).startswith(apps):
        continue
    found += 1
    for dep in pkg["dependencies"]:
        if dep["kind"] != "dev" and dep["name"] not in allowed:
            print("1\t%s: %s" % (pkg["name"], dep["name"]))
if not found:
    print("2\tno package under apps/")
' "$root" "$ALLOWED"
}

if [ "$GATE_SELF_TEST" -eq 1 ]; then
    gate_selftest_begin check_app_deps

    cat > "$(gate_fixture Cargo.toml)" <<'FIXTURE'
[workspace]
members = ["apps", "other"]
resolver = "2"
FIXTURE
    cat > "$(gate_fixture apps/Cargo.toml)" <<'FIXTURE'
[package]
name = "fixture-apps"
version = "0.1.0"
edition = "2024"

[dependencies]
slopos-appkit = "0.1"
slopos-editor-core = "0.1"
slopos-image = "0.1"
slopos-abi = "0.1"
gfx = { package = "slopos-gfx", version = "0.1" }

[target.'cfg(unix)'.dependencies]
slopos-cabi = "0.1"

[build-dependencies]
slopos-windowing = "0.1"

[dev-dependencies]
slopos-protocol = "0.1"
FIXTURE
    cat > "$(gate_fixture other/Cargo.toml)" <<'FIXTURE'
[package]
name = "fixture-other"
version = "0.1.0"
edition = "2024"

[dependencies]
slopos-userland = "0.1"
FIXTURE
    : > "$(gate_fixture apps/src/lib.rs)"
    : > "$(gate_fixture other/src/lib.rs)"

    GATE_FINDINGS="$(scan "$GATE_FIXTURE_ROOT")"
    gate_expect 1 4 "a direct, renamed, target and build dependency"
    gate_expect 2 0
    gate_expect_silent 'slopos-(appkit|editor-core|image|protocol|userland)$' \
        "the allowed crates, a dev dependency and a crate outside apps/"

    rm -rf "${GATE_FIXTURE_ROOT:?}/apps"
    printf '[workspace]\nmembers = ["other"]\nresolver = "2"\n' > "$GATE_FIXTURE_ROOT/Cargo.toml"
    GATE_FINDINGS="$(scan "$GATE_FIXTURE_ROOT")"
    gate_expect 2 1 "no apps to hold"

    gate_selftest_end
fi

findings="$(scan "$REPO_ROOT")"
if printf '%s\n' "$findings" | grep -q '^2	'; then
    echo "check_app_deps: no crate found under apps/, so refusing to report OK" >&2
    exit 2
fi
if [ -n "$findings" ]; then
    echo "check_app_deps: an app depends on more than appkit, editor-core and image:" >&2
    printf '%s\n' "$findings" | cut -f2- | sed 's/^/  /' >&2
    exit 1
fi
echo "check_app_deps: OK — apps depend on appkit, editor-core and image alone"

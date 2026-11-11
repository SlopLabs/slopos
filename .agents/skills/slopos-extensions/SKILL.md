---
name: slopos-extensions
description: >
  Extensions: verity-checked images shown read-only at /opt/<name>, with their grants, users, services, actions, commands and settings.
  Use when a task touches fs/src/extension, extension-core, verity-core, userland/src/apps/extctl, init's extensions.rs, userland/src/activation.rs, tools/extension, assets/privd/extensions, or mentions ext_activate, extctl, an activation, a grant, a ceiling, a sidecar or the extension store.
---

## Read first

- `docs/architecture/extensions`: how an image is installed, activated and run, and why
- `docs/abi/extension-images`: exact image, declaration, sidecar, record and settings formats
- `docs/development/packaging-an-extension`: packing, signing and installing one, step by step
- `docs/abi/privd-actions`: the principals and levels an extension's action uses

## Rules a change must keep

- The kernel checks no signature. It serves only bytes the root its `ExtensionUpdate` caller vouched for covers; a block the tree refuses fails that read with `EIO`.
- Hold a grant to its record, its key's ceilings and the base's ceiling at install and at every activation, and never grant what `ceiling::NEVER` lists.
- `slopos_extension_core::volume`'s reader and the kernel's `extension::index` refuse the same shapes; loosen neither alone.
- Locks nest activation, then its device (`EXT_VERITY`), then the filesystem holding the image file.
- Every extension program starts `AT_SECURE` (`exec_image`), whoever starts it; do not make it conditional on the caller.
- Keep `EXT: image block <n> fails its tree` rate-limited: any reader of an activation can provoke it.
- Store writers never rename: lock `EXTENSION_STORE.lock`, write the next generation, seal it through the descriptor that wrote it, read it back, then remove older ones. Only a sealed file with uid 0's owner and mode is believed.
- A mechanism reads the files a request names as uid 0 without `DacOverride`; settings arrive on the requester's standard input.
- Init and privd take uids from records, never from a name lookup, and start a program only after checking the opened file is of the activation its declaration was read from.
- Read `SO_PEEREXTENSION` before `SO_PEERPROGRAM`, so a label that lapses between the reads gives no program.
- A change to what init carries across a re-execution takes a new `extensions::CARRIED` header; an image without the header gets no services handed on.
- `extctl` touches only its own links into `/opt` in `/opt/bin`, and no privd action takes `--root`.

## Checks

- `just test 'slopos_fs::tests::extensions::*'`: activation, the verifying device, the pool
- `just test '*utest_extension*'`: installs, services and actions end to end
- `just test-host`: `extension-core`, `verity-core` and `tools/extension` unit tests

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.

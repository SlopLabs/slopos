# apps

The apps shipped outside the base. They change on their own schedule, so they reach the system through `libc.so` and appkit alone. Read `appkit/GUIDE.md` before writing or changing one, then the `slopos-desktop` skill.

- A crate here depends on `slopos-appkit`, `slopos-editor-core` and `slopos-image` and nothing else (`xtask lint app-deps`). What an app needs from below appkit, appkit re-exports.
- An app names a `Tone`, a `Surface`, a `TextRole`, a spacing step or an inset, never a colour or a number (`xtask lint ui-literals`). A file whose colours are its content is exempted by a line of `scripts/gates/ui-literals.txt`, which fails once it matches nothing.
- Build the window from appkit's components; they own spacing, focus, overflow and hit targets. Do not draw a control by hand.
- Words follow `theme-core::words`: the glossary's terms, a label's and a sentence's shape, sizes as a person reads them.
- An app says why the computer refused a file through `appkit::Refusal`, never by looking at the path.
- A program here is dynamically linked against `libc.so` through `/lib/ld-slopos.so.1` and holds no `syscall` instruction (`xtask programs`, after every host userland build).
- A dock entry is `share/slopos/apps/<id>.app` with an icon drawn by `just icons`; the window's app id matches the entry's.

Checks: `just test '*utest_apps*,*utest_appkit'`, `just test-host` (icons, theme, words), `just check-framekernel-gates`, `just ui-shots` to look at it.

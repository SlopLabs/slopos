# Sloped

The SlopOS editor. Tabs, a file tree, syntax highlighting, find and replace,
a command palette, and every edit undoable.

Start it from the dock, or from a shell:

```
editor            # the working directory, in an empty buffer
editor file.rs    # one file, with its folder in the sidebar
editor /src       # a folder
```

## Keys

### Files

| Chord | What it does |
| --- | --- |
| `Ctrl+N` | New file |
| `Ctrl+O` | Open a file by path |
| `Ctrl+Shift+O` | Open a folder in the sidebar |
| `Ctrl+P` | Go to a file by name, anywhere in the tree |
| `Ctrl+S` | Save |
| `Ctrl+Shift+S` | Save as |
| `Ctrl+W` | Close the tab |
| `Ctrl+Tab` | Next tab (`Ctrl+Shift+Tab` for the previous one) |

### Editing

| Chord | What it does |
| --- | --- |
| `Ctrl+Z` / `Ctrl+Y` | Undo, redo |
| `Ctrl+X` / `Ctrl+C` / `Ctrl+V` | Cut, copy, paste |
| `Ctrl+A` | Select the whole file |
| `Ctrl+L` | Select the line |
| `Ctrl+D` | Duplicate the line |
| `Ctrl+K` | Delete the line |
| `Alt+Up` / `Alt+Down` | Move the line |
| `Ctrl+/` | Comment or uncomment |
| `Tab` / `Shift+Tab` | Indent, outdent |

### Finding

| Chord | What it does |
| --- | --- |
| `Ctrl+F` | Find |
| `Ctrl+H` | Find and replace |
| `Enter` / `Shift+Enter` | Next match, previous match |
| `Enter` in the replace field | Replace this match and go to the next |
| `Ctrl+Enter` | Replace every match |
| `F3` / `Shift+F3` | Next, previous — with the bar closed |
| `Ctrl+G` | Go to a line number |

### The window

| Chord | What it does |
| --- | --- |
| `Ctrl+Shift+P` | The command palette: every command, by name |
| `Ctrl+B` | Show or hide the sidebar |
| `Escape` | Close whatever is open |
| `Ctrl+Backspace` | In a prompt: back one path segment, or one word |

Drag the line between the sidebar and the code to resize it. A double click
selects a word, a triple click selects the line, and a drag selects a range.

## What it knows

Syntax highlighting covers Rust, C, TOML, JSON, Markdown, shell and Python.
Indentation is read from the file rather than assumed: a tab-indented file
indents with tabs, and a file indented two spaces stays that way.

Line endings survive a round trip — a file that arrived with CRLF is saved with
CRLF — and so does a missing final newline. A file with *mixed* endings and a
CRLF majority is normalised to CRLF; one with an LF majority is left exactly as
it is, carriage returns and all. A carriage return that is not a line ending is
content either way.

A save writes a sibling file, flushes it to the disk and renames it over the
target, so a save that fails part way leaves the old file intact rather than a
truncated one. A file with a NUL byte in its first 8 KiB is refused rather than
opened as replacement characters that saving would then write back.

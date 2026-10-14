#!/usr/bin/env python3
"""Canonicalise clang's -fdump-record-layouts-complete output.

Usage: libc_layouts.py <include dir> < dump > canonical

Reads the dump on stdin and prints, for every struct/union defined under
<include dir>, sorted by record:

    <record> size <n> align <n>
    <record> <dotted.member.path> <offset>     (one per named field, in order)

<record> is `struct X` / `union X`, or `typedef X` for an anonymous record
named by a typedef. Field types are dropped: their spelling varies across
clang releases and the offsets already carry what the ABI depends on.
Exit 2 on anything it cannot name.
"""

import os
import re
import sys

HEADING = "*** Dumping AST Record Layout"
ROW = re.compile(r"^\s*(\S*) \| (.*)$")
UNNAMED = re.compile(
    r"^\((?:anonymous|unnamed)(?: struct| union)? at (.+):(\d+):(\d+)\)$"
)
IDENT = re.compile(r"[A-Za-z_]\w*")
DECLARATOR = re.compile(r"([A-Za-z_]\w*)(?!\s*[\w(])")
TAG_DEF = re.compile(
    r"\b(struct|union)\s+(?:__attribute__\s*\(\(.*?\)\)\s*)*([A-Za-z_]\w*)\s*\{", re.S
)
ANON_TYPEDEF = re.compile(r"\btypedef\s+(struct|union)\s*(?=\{)")
COMMENT = re.compile(r"/\*.*?\*/|//[^\n]*", re.S)
ANON_MEMBER = re.compile(r"\((?:anonymous|unnamed)[^()]* at [^()]*\)\s*$")
COMPILER_RECORDS = {"struct __va_list_tag", "struct __NSConstantString_tag"}


def die(message):
    print(f"libc_layouts: {message}", file=sys.stderr)
    sys.exit(2)


def blank_comments(text):
    # Same length, so offsets computed from line:col stay valid.
    return COMMENT.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), text)


def typedef_name(text, start, where):
    """Name of the typedef closing the anonymous record whose keyword is at
    `start`: the first identifier after its matching `}`. None when no
    typedef opens it."""
    segment = re.split(r"[;{}]", text[:start])[-1]
    if not re.search(r"\btypedef\b", segment):
        return None
    opening = text.find("{", start)
    if opening < 0:
        die(f"no body for the record at {where}")
    depth = 0
    for i in range(opening, len(text)):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                end = text.find(";", i)
                tail = text[i + 1 : end if end >= 0 else None]
                # The declarator is the first identifier at paren depth 0 that
                # is not itself called: `} __slibc_aligned(8) siginfo_t;`.
                for name in DECLARATOR.finditer(tail):
                    if tail[: name.start()].count("(") == tail[: name.start()].count(")"):
                        return name.group(1)
                die(f"no typedef name closes the record at {where}")
    die(f"unbalanced braces after {where}")


class Headers:
    def __init__(self, root):
        self.root = os.path.realpath(root)
        self.text = {}
        self.tags = set()
        self.typedefs = set()
        for directory, _, files in os.walk(self.root):
            for name in sorted(files):
                if not name.endswith(".h"):
                    continue
                path = os.path.join(directory, name)
                text = blank_comments(open(path, encoding="utf-8").read())
                self.text[path] = text
                self.tags.update(m.group(1) + " " + m.group(2) for m in TAG_DEF.finditer(text))
                for m in ANON_TYPEDEF.finditer(text):
                    self.typedefs.add(typedef_name(text, m.start(1), path))

    def owns(self, path):
        return os.path.realpath(path).startswith(self.root + os.sep)

    def at(self, path, line, col):
        text = self.text.get(os.path.realpath(path))
        if text is None:
            die(f"{path} is not a header under {self.root}")
        lines = text.split("\n")
        if line > len(lines):
            die(f"{path} has no line {line}")
        offset = sum(len(l) + 1 for l in lines[: line - 1]) + col - 1
        return typedef_name(text, offset, f"{path}:{line}:{col}")


def record_key(heading, headers):
    """Canonical name of a dumped record, or None when it is not ours."""
    if "::" in heading:
        return None  # an anonymous member, already expanded in its parent
    kind, _, rest = heading.partition(" ")
    if kind not in ("struct", "union"):
        die(f"unrecognised record heading: {heading!r}")
    unnamed = UNNAMED.match(rest)
    if unnamed:
        path, line, col = unnamed.group(1), int(unnamed.group(2)), int(unnamed.group(3))
        if not headers.owns(path):
            return None
        name = headers.at(path, line, col)
        # clang 18 heads a nested anonymous member without its parent's name.
        return None if name is None else "typedef " + name
    if not IDENT.fullmatch(rest):
        die(f"unrecognised record heading: {heading!r}")
    if kind + " " + rest in headers.tags:
        return kind + " " + rest
    if rest in headers.typedefs:
        return "typedef " + rest
    if kind + " " + rest in COMPILER_RECORDS:
        return None
    die(f"{heading!r} is in the dump but defined in no header under {headers.root}")


def parse(stream, headers):
    records = {}
    lines = stream.read().split("\n")
    i = 0
    seen = 0
    while i < len(lines):
        if lines[i].strip() != HEADING:
            i += 1
            continue
        seen += 1
        i += 1
        while i < len(lines) and not lines[i].strip():
            i += 1
        row = ROW.match(lines[i]) if i < len(lines) else None
        if not row:
            die(f"no record heading after dump marker {seen}")
        key = record_key(row.group(2).strip(), headers)
        fields = []
        stack = []
        size = align = None
        i += 1
        while i < len(lines):
            row = ROW.match(lines[i])
            if not row:
                die(f"unterminated layout for {key or 'a skipped record'}")
            offset, text = row.groups()
            i += 1
            if text.startswith("["):
                s = re.search(r"\bsizeof=(\d+)", text)
                a = re.search(r"(?<![A-Za-z])align=(\d+)", text)
                if not (s and a):
                    die(f"no sizeof/align for {key}: {text!r}")
                size, align = s.group(1), a.group(1)
                break
            depth = (len(text) - len(text.lstrip(" "))) // 2
            body = text.lstrip(" ")
            if depth < 1 or not offset:
                die(f"malformed field row in {key}: {text!r}")
            anonymous = body.endswith(" ") or " " not in body or ANON_MEMBER.search(body)
            name = "" if anonymous else body.rsplit(" ", 1)[1]
            stack = stack[: depth - 1] + [name]
            if name:
                fields.append(f"{key} {'.'.join(n for n in stack if n)} {offset}")
        if size is None:
            die(f"layout for {key} ends without its size")
        if key is None:
            continue
        block = [f"{key} size {size} align {align}"] + fields
        if key in records and records[key] != block:
            die(f"two different layouts are both named {key}")
        records[key] = block
    if seen == 0:
        die("no record layouts in the dump")
    for key in sorted(records):
        print("\n".join(records[key]))


def main():
    if len(sys.argv) != 2 or not os.path.isdir(sys.argv[1]):
        die("usage: libc_layouts.py <include dir> < dump")
    parse(sys.stdin, Headers(sys.argv[1]))


if __name__ == "__main__":
    main()

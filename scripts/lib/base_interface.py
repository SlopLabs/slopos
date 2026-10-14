#!/usr/bin/env python3
"""The private call surface of slopos-abi, read from rustdoc's JSON.

Usage: base_interface.py --json <slopos_abi.json> --src-root <dir>
                         --gate-file <file> --min-calls N --min-lines N
                         (check | print | record)

The surface is a set of canonical lines: one `call` line per private syscall
(number, name and the signature from its docs), and one line per abi item the
calls reach — named in a signature, linked from a call's docs, matched by a
`root` glob in the gate file, or reached from any of those through field,
variant, alias and constant types. Lines carry no rustdoc ids and no docs, so
prose and formatting never move the digest; only the shape does.

check   prints `<tag>\\t<message>` findings, then `info\\t<calls>\\t<lines>\\t<digest>`;
        exit 1 on findings.
print   the sorted surface lines.
record  appends `interface <BASE_INTERFACE> <digest>` when that is the next row.
Exit 2 when the input cannot be read as the parser expects.
"""

import argparse
import fnmatch
import hashlib
import json
import os
import re
import sys
import traceback

FORMAT_VERSION = 61
CRATE = "slopos_abi"
NUMBERS = (CRATE, "syscall", "numbers")
CALL_PREFIX = "::".join(NUMBERS) + "::SYSCALL_"
EXCLUDED_PREFIX = CRATE + "::base_interface"
RANGE_BOUNDS = {"SYSCALL_PRIVATE_BASE", "SYSCALL_PRIVATE_END", "SYSCALL_PRIVATE_TABLE_SIZE"}
TYPE_KINDS = {"struct", "enum", "union", "type_alias", "constant"}
ITEM_KINDS = TYPE_KINDS | {"function", "static"}
IDENT_PATH = re.compile(r"[A-Za-z_]\w*(?:::[A-Za-z_]\w*)*")
INT_VALUE = re.compile(r"^(-?[0-9_]+)(?:[iu](?:8|16|32|64|128|size))?$")
ROW = re.compile(r"^interface ([0-9]+) ([0-9a-f]{64})$")
PUNCT = ("..=", "...", "<<=", ">>=", "::", "->", "=>", "==", "!=", "<=", ">=", "&&", "||",
         "<<", ">>", "..", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=")


class Unreadable(Exception):
    pass


def int_value(text):
    m = INT_VALUE.match(text or "")
    return int(m.group(1).replace("_", "")) if m else None


# ---------------------------------------------------------------------------
# Source text, for the items whose shape rustdoc does not evaluate: comments
# dropped, tokens joined by single spaces, trailing commas removed — so a
# reformat or a comment edit leaves the text unchanged.
# ---------------------------------------------------------------------------

def rust_tokens(src):
    out, i, n = [], 0, len(src)
    while i < n:
        c = src[i]
        if c.isspace():
            i += 1
        elif src.startswith("//", i):
            j = src.find("\n", i)
            i = n if j < 0 else j
        elif src.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if src.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif src.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
        elif c.isalnum() or c == "_":
            j = i
            while j < n and (src[j].isalnum() or src[j] == "_"):
                j += 1
            word = src[i:j]
            if word in ("r", "br", "cr") and j < n and src[j] in "#\"":
                k = j
                while k < n and src[k] == "#":
                    k += 1
                if k < n and src[k] == '"':
                    close = '"' + "#" * (k - j)
                    end = src.find(close, k + 1)
                    end = n if end < 0 else end + len(close)
                    out.append(src[i:end])
                    i = end
                    continue
            if word in ("b", "c") and j < n and src[j] in "\"'":
                end = string_end(src, j)
                out.append(src[i:end])
                i = end
                continue
            out.append(word)
            i = j
        elif c == '"':
            end = string_end(src, i)
            out.append(src[i:end])
            i = end
        elif c == "'":
            if i + 1 < n and src[i + 1] == "\\":
                end = string_end(src, i)
            elif i + 2 < n and src[i + 2] == "'":
                end = i + 3
            else:
                end = i + 1
                while end < n and (src[end].isalnum() or src[end] == "_"):
                    end += 1
            out.append(src[i:end])
            i = end
        else:
            p = next((p for p in PUNCT if src.startswith(p, i)), c)
            out.append(p)
            i += len(p)
    return [t for k, t in enumerate(out)
            if not (t == "," and k + 1 < len(out) and out[k + 1] in (")", "]", "}"))]


def string_end(src, i):
    quote, j = src[i], i + 1
    while j < len(src):
        if src[j] == "\\":
            j += 2
        elif src[j] == quote:
            return j + 1
        else:
            j += 1
    return len(src)


# ---------------------------------------------------------------------------
# The crate, as rustdoc sees it.
# ---------------------------------------------------------------------------

class Crate:
    def __init__(self, doc, src_root):
        self.index = doc["index"]
        self.src_root = src_root
        self.files = {}
        self.paths = {}
        for key, entry in doc["paths"].items():
            self.paths[int(key)] = entry
        self.local = {}
        for ident, entry in self.paths.items():
            if entry["crate_id"] == 0 and str(ident) in self.index:
                self.local[ident] = "::".join(entry["path"])
        # Inherent members (`Type::NAME`, `Type::method`): not in `paths`.
        self.members = {}
        for ident, path in self.local.items():
            item = self.item(ident)
            inner = item["inner"]
            kind = next(iter(inner))
            if kind not in ("struct", "enum", "union"):
                continue
            for impl_id in inner[kind].get("impls", []):
                impl = self.item(impl_id)
                if impl is None or impl["inner"]["impl"]["trait"] is not None:
                    continue
                if impl["inner"]["impl"]["is_synthetic"] or impl["inner"]["impl"]["blanket_impl"]:
                    continue
                for member in impl["inner"]["impl"]["items"]:
                    m = self.item(member)
                    if m and m["name"] and next(iter(m["inner"])) in ("assoc_const", "function"):
                        self.members[path + "::" + m["name"]] = member
        self.by_path = {path: ident for ident, path in self.local.items()}

    def item(self, ident):
        return self.index.get(str(ident))

    def kind(self, ident):
        entry = self.paths.get(ident)
        return entry["kind"] if entry else None

    def path_of(self, ident):
        entry = self.paths.get(ident)
        return "::".join(entry["path"]) if entry else None

    def source(self, span):
        name = span["filename"]
        if name not in self.files:
            path = name if os.path.isabs(name) else os.path.join(self.src_root, name)
            try:
                with open(path, encoding="utf-8") as fh:
                    self.files[name] = fh.read().split("\n")
            except OSError as err:
                raise Unreadable(f"cannot read {path} for an item's source: {err}")
        lines = self.files[name]
        (bl, bc), (el, ec) = span["begin"], span["end"]
        if bl == el:
            text = lines[bl - 1][bc - 1:ec - 1]
        else:
            text = "\n".join([lines[bl - 1][bc - 1:]] + lines[bl:el - 1] + [lines[el - 1][:ec - 1]])
        return " ".join(rust_tokens(text))


# ---------------------------------------------------------------------------
# Canonical rendering. `refs` collects the local items a rendering names.
# ---------------------------------------------------------------------------

def strip_ids(value):
    if isinstance(value, dict):
        return {k: strip_ids(v) for k, v in value.items() if k != "id"}
    if isinstance(value, list):
        return [strip_ids(v) for v in value]
    return value


def render_type(crate, ty, refs):
    if ty is None:
        return "()"
    (kind, body), = ty.items()
    if kind in ("primitive", "generic"):
        return body
    if kind == "resolved_path":
        ident = body["id"]
        path = crate.path_of(ident) or body["path"]
        if ident in crate.local:
            refs.add(ident)
        return path + render_args(crate, body.get("args"), refs)
    if kind == "array":
        return "[%s; %s]" % (render_type(crate, body["type"], refs), body["len"])
    if kind == "slice":
        return "[%s]" % render_type(crate, body, refs)
    if kind == "raw_pointer":
        return "*%s %s" % ("mut" if body["is_mutable"] else "const",
                           render_type(crate, body["type"], refs))
    if kind == "borrowed_ref":
        lt = body["lifetime"] + " " if body["lifetime"] else ""
        mut = "mut " if body["is_mutable"] else ""
        return "&%s%s%s" % (lt, mut, render_type(crate, body["type"], refs))
    if kind == "tuple":
        return "(%s)" % ", ".join(render_type(crate, t, refs) for t in body)
    return json.dumps(strip_ids(ty), sort_keys=True, separators=(",", ":"))


def render_args(crate, args, refs):
    if not args:
        return ""
    if "angle_bracketed" not in args:
        return json.dumps(strip_ids(args), sort_keys=True, separators=(",", ":"))
    parts = []
    for arg in args["angle_bracketed"]["args"]:
        (kind, body), = arg.items()
        if kind == "type":
            parts.append(render_type(crate, body, refs))
        elif kind == "lifetime":
            parts.append(body)
        else:
            parts.append(json.dumps(strip_ids(body), sort_keys=True, separators=(",", ":")))
    for c in args["angle_bracketed"]["constraints"]:
        parts.append(json.dumps(strip_ids(c), sort_keys=True, separators=(",", ":")))
    return "<%s>" % ", ".join(parts) if parts else ""


def render_repr(item):
    for attr in item["attrs"]:
        if isinstance(attr, dict) and "repr" in attr:
            r = attr["repr"]
            parts = [r["kind"]]
            for key in ("int", "align", "packed"):
                if r.get(key) is not None:
                    parts.append("%s=%s" % (key, r[key]))
            return " repr(%s)" % ",".join(parts)
    return " repr()"


def render_generics(generics):
    names = [p["name"] for p in generics["params"]]
    return "<%s>" % ", ".join(names) if names else ""


def render_fields(crate, field_ids, refs):
    return ["%s: %s" % (crate.item(f)["name"], render_type(crate, crate.item(f)["inner"]["struct_field"], refs))
            for f in field_ids]


def render_struct_kind(crate, kind, refs):
    if kind == "unit":
        return ";"
    if "tuple" in kind:
        return "(%s)" % ", ".join(
            render_type(crate, crate.item(f)["inner"]["struct_field"], refs) if f is not None else "_"
            for f in kind["tuple"])
    return "{%s}" % ", ".join(render_fields(crate, kind["plain"]["fields"], refs))


def render_const_value(crate, item, const):
    if const.get("value") is not None:
        return const["value"]
    if const.get("expr") not in (None, "_"):
        return const["expr"]
    return "{%s}" % crate.source(item["span"])


def render(crate, path, item, refs):
    """One canonical line for `item`, or None for kinds the surface skips."""
    (kind, body), = item["inner"].items()
    if kind == "struct":
        return "struct %s%s%s %s" % (path, render_generics(body["generics"]), render_repr(item),
                                     render_struct_kind(crate, body["kind"], refs))
    if kind == "union":
        return "union %s%s%s {%s}" % (path, render_generics(body["generics"]), render_repr(item),
                                      ", ".join(render_fields(crate, body["fields"], refs)))
    if kind == "enum":
        variants, prev = [], -1
        for v in body["variants"]:
            var = crate.item(v)
            vb = var["inner"]["variant"]
            disc = vb["discriminant"]
            if disc is not None:
                value = disc["value"]
            elif isinstance(prev, int):
                value = str(prev + 1)
            else:
                value = "%s+1" % prev
            iv = int_value(value)
            prev = iv if iv is not None else value
            vk = vb["kind"]
            shape = ""
            if vk != "plain":
                if "tuple" in vk:
                    shape = render_struct_kind(crate, {"tuple": vk["tuple"]}, refs)
                else:
                    shape = "{%s}" % ", ".join(render_fields(crate, vk["struct"]["fields"], refs))
            variants.append("%s%s = %s" % (var["name"], shape, value))
        return "enum %s%s%s {%s}" % (path, render_generics(body["generics"]), render_repr(item),
                                     ", ".join(variants))
    if kind == "type_alias":
        return "type %s%s = %s" % (path, render_generics(body["generics"]),
                                   render_type(crate, body["type"], refs))
    if kind == "constant":
        return "const %s: %s = %s" % (path, render_type(crate, body["type"], refs),
                                      render_const_value(crate, item, body["const"]))
    if kind == "assoc_const":
        value = body.get("value")
        if value in (None, "_"):
            value = "{%s}" % crate.source(item["span"])
        return "const %s: %s = %s" % (path, render_type(crate, body["type"], refs), value)
    if kind == "static":
        return "static %s: %s = {%s}" % (path, render_type(crate, body["type"], refs),
                                         crate.source(item["span"]))
    if kind == "function":
        return "fn %s {%s}" % (path, crate.source(item["span"]))
    return None


# ---------------------------------------------------------------------------
# Signatures.
# ---------------------------------------------------------------------------

def code_spans(docs):
    i, n = 0, len(docs)
    while i < n:
        if docs[i] != "`":
            i += 1
            continue
        j = i
        while j < n and docs[j] == "`":
            j += 1
        fence = docs[i:j]
        k = docs.find(fence, j)
        while k >= 0 and k + len(fence) < n and docs[k + len(fence)] == "`":
            k = docs.find(fence, k + len(fence) + 1)
        if k < 0:
            return
        yield docs[j:k]
        i = k + len(fence)


def find_signature(docs, name):
    for span in code_spans(docs or ""):
        text = " ".join(span.split())
        if text.startswith(name + "("):
            return text
    return None


def split_args(text):
    parts, depth, cur = [], 0, ""
    for c in text:
        if c in "([":
            depth += 1
        elif c in ")]":
            depth -= 1
        if c == "," and depth == 0:
            parts.append(cur)
            cur = ""
        else:
            cur += c
    parts.append(cur)
    return parts


def type_idents(sig, name):
    """The capitalised identifiers in a signature's argument types and return:
    the type and constant names it can mean. A lowercase one is a primitive or
    a field of a packing, `hotspot: (hot_x << 16) | hot_y`; a return is often
    prose, `-> entries written`."""
    body = sig[len(name) + 1:]
    depth, close = 1, None
    for i, c in enumerate(body):
        if c in "([":
            depth += 1
        elif c in ")]":
            depth -= 1
            if depth == 0:
                close = i
                break
    if close is None:
        return None
    found = []
    for arg in split_args(body[:close]):
        m = re.match(r"\s*[A-Za-z_]\w*\s*:(?!:)(.*)$", arg, re.S)
        if m:
            found.extend(IDENT_PATH.findall(m.group(1)))
    rest = body[close + 1:].strip()
    if rest.startswith("->"):
        found.extend(IDENT_PATH.findall(rest[2:]))
    out = []
    for p in found:
        segs = [s for s in p.split("::") if s not in ("crate", "super", "self", CRATE)]
        if segs and segs[-1][0].isupper() and segs[-1] != "Self":
            out.append(p)
    return out


def resolve(crate, ident):
    segs = [s for s in ident.split("::") if s not in ("crate", "super", "self", CRATE)]
    hits = []
    for local, path in crate.local.items():
        if crate.kind(local) not in TYPE_KINDS:
            continue
        parts = path.split("::")
        if parts[-len(segs):] == segs:
            hits.append(local)
    return hits


# ---------------------------------------------------------------------------
# The gate file.
# ---------------------------------------------------------------------------

def read_gate(path):
    roots, rows, findings = [], [], []
    try:
        with open(path, encoding="utf-8") as fh:
            lines = fh.read().split("\n")
    except OSError as err:
        raise Unreadable(f"cannot read {path}: {err}")
    for lineno, line in enumerate(lines, 1):
        text = line.strip()
        if not text or text.startswith("#"):
            continue
        if text.startswith("root "):
            glob = text[5:].strip()
            if not glob or " " in glob:
                findings.append(("ledger", f"{path}:{lineno}: malformed root line: {text}"))
            else:
                roots.append((lineno, glob))
            continue
        m = ROW.match(text)
        if not m:
            findings.append(("ledger", f"{path}:{lineno}: not a `root <glob>` or `interface <n> <sha256>` line: {text}"))
            continue
        rows.append((lineno, int(m.group(1)), m.group(2)))
    return roots, rows, findings


# ---------------------------------------------------------------------------
# The surface.
# ---------------------------------------------------------------------------

def under(path, prefix):
    return path == prefix or path.startswith(prefix + "::")


class Surface:
    def __init__(self, crate, roots):
        self.crate = crate
        self.findings = []
        self.lines = set()
        self.calls = 0
        self.done = set()

        consts = {}
        for ident, path in crate.local.items():
            entry = crate.paths[ident]
            if entry["kind"] == "constant" and tuple(entry["path"][:-1]) == NUMBERS:
                consts[entry["path"][-1]] = crate.item(ident)
        base = int_value((consts.get("SYSCALL_PRIVATE_BASE") or {}).get("inner", {}).get("constant", {}).get("const", {}).get("value"))
        end = int_value((consts.get("SYSCALL_PRIVATE_END") or {}).get("inner", {}).get("constant", {}).get("const", {}).get("value"))
        if base is None or end is None:
            self.findings.append(("meta", "SYSCALL_PRIVATE_BASE / SYSCALL_PRIVATE_END missing from %s or not evaluated" % "::".join(NUMBERS)))
        self.base_interface = self.single_constant("BASE_INTERFACE")
        self.base_interface_min = self.single_constant("BASE_INTERFACE_MIN")

        pending_items = []
        pending_prefixes = []
        if base is not None and end is not None:
            for name in sorted(consts):
                if not name.startswith("SYSCALL_") or name in RANGE_BOUNDS:
                    continue
                item = consts[name]
                number = int_value(item["inner"]["constant"]["const"]["value"])
                if number is None or not (base <= number < end):
                    continue
                self.calls += 1
                call = name[len("SYSCALL_"):].lower()
                sig = find_signature(item["docs"], call)
                if sig is None:
                    self.findings.append(("signature", f"{name}: its docs carry no `{call}(...)` code span — the call's arguments are part of the interface and must be written down"))
                    continue
                self.lines.add(f"call {number} {name} {sig}")
                idents = type_idents(sig, call)
                if idents is None:
                    self.findings.append(("signature", f"{name}: unbalanced parentheses in `{sig}`"))
                    continue
                for ident in idents:
                    hits = resolve(crate, ident)
                    if len(hits) != 1:
                        why = "names nothing in %s" % CRATE if not hits else "is ambiguous: " + ", ".join(sorted(crate.local[h] for h in hits))
                        self.findings.append(("unresolved", f"{name}: `{ident}` in `{sig}` {why}"))
                        continue
                    pending_items.append(hits[0])
                for target in item["links"].values():
                    if target not in crate.local or crate.local[target].startswith(CALL_PREFIX):
                        continue
                    if crate.kind(target) == "module":
                        pending_prefixes.append(crate.local[target])
                    else:
                        pending_items.append(target)

        universe = sorted(set(crate.by_path) | set(crate.members))

        def renders(p):
            if under(p, EXCLUDED_PREFIX):
                return False
            return p in crate.members or crate.kind(crate.by_path[p]) in ITEM_KINDS

        for lineno, glob in roots:
            matched = [p for p in universe if fnmatch.fnmatchcase(p, glob)]
            reached = [q for p in matched for q in universe if under(q, p)]
            if not any(renders(q) for q in reached):
                self.findings.append(("dead", f"line {lineno}: `root {glob}` roots nothing the surface renders"))
            for p in matched:
                if p in crate.members:
                    self.add_member(p)
                elif crate.kind(crate.by_path[p]) == "module":
                    pending_prefixes.append(p)
                else:
                    pending_items.append(crate.by_path[p])

        for prefix in pending_prefixes:
            for p in universe:
                if under(p, prefix):
                    if p in crate.members:
                        self.add_member(p)
                    else:
                        pending_items.append(crate.by_path[p])
        for ident in pending_items:
            self.add_item(ident)

    def single_constant(self, name):
        hits = [i for i, p in self.crate.local.items()
                if self.crate.kind(i) == "constant" and p.split("::")[-1] == name]
        value = None
        if len(hits) == 1:
            value = int_value(self.crate.item(hits[0])["inner"]["constant"]["const"]["value"])
        if value is None:
            self.findings.append(("meta", f"{CRATE} defines no single evaluated constant {name}"))
        return value

    def add_member(self, path):
        if path in self.done or under(path, EXCLUDED_PREFIX):
            return
        self.done.add(path)
        refs = set()
        line = render(self.crate, path, self.crate.item(self.crate.members[path]), refs)
        if line:
            self.lines.add(line)
        for r in refs:
            self.add_item(r)

    def add_item(self, ident):
        work = [ident]
        while work:
            ident = work.pop()
            path = self.crate.local.get(ident)
            if path is None or path in self.done or under(path, EXCLUDED_PREFIX):
                continue
            if self.crate.kind(ident) not in ITEM_KINDS:
                continue
            self.done.add(path)
            refs = set()
            line = render(self.crate, path, self.crate.item(ident), refs)
            if line:
                self.lines.add(line)
            # A type's inherent constants are part of its shape (`SERIALIZED_LEN`).
            for member, mid in self.crate.members.items():
                if member.rsplit("::", 1)[0] == path and "assoc_const" in self.crate.item(mid)["inner"]:
                    if member not in self.done:
                        self.done.add(member)
                        self.lines.add(render(self.crate, member, self.crate.item(mid), refs))
            work.extend(refs)

    def sorted_lines(self):
        return sorted(self.lines)

    def digest(self):
        return hashlib.sha256("\n".join(self.sorted_lines()).encode()).hexdigest()


def ledger_findings(rows, base_interface, base_interface_min):
    out = []
    for expect, (lineno, n, _) in enumerate(rows, 1):
        if n != expect:
            out.append(("ledger", f"line {lineno}: `interface {n}` where the ledger's next row is {expect} — rows run 1, 2, 3 … oldest first"))
        elif base_interface is not None and n > base_interface:
            out.append(("ledger", f"line {lineno}: `interface {n}` is past BASE_INTERFACE = {base_interface}"))
    if base_interface is not None and base_interface_min is not None:
        if not (1 <= base_interface_min <= base_interface):
            out.append(("ledger", f"BASE_INTERFACE_MIN = {base_interface_min} is outside 1..={base_interface}"))
    if base_interface is not None:
        last = len(rows)
        if base_interface > last + 1:
            out.append(("ledger", f"BASE_INTERFACE = {base_interface} skips past the ledger's next number {last + 1}"))
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--json", required=True)
    ap.add_argument("--src-root", required=True)
    ap.add_argument("--gate-file", required=True)
    ap.add_argument("--min-calls", type=int, required=True)
    ap.add_argument("--min-lines", type=int, required=True)
    ap.add_argument("mode", choices=("check", "print", "record"))
    args = ap.parse_args()

    try:
        try:
            with open(args.json, encoding="utf-8") as fh:
                doc = json.load(fh)
        except (OSError, ValueError) as err:
            raise Unreadable(f"cannot read {args.json}: {err}")
        if doc.get("format_version") != FORMAT_VERSION:
            raise Unreadable(
                f"rustdoc JSON format_version is {doc.get('format_version')}, the parser was written "
                f"against {FORMAT_VERSION}. A toolchain bump must re-validate scripts/lib/base_interface.py "
                f"(run --self-test, then --print before and after) and update FORMAT_VERSION; "
                f"otherwise the digest could change, or stop seeing changes, silently")
        root = doc["index"].get(str(doc["root"]))
        if root is None or root["name"] != CRATE:
            raise Unreadable(f"{args.json} documents {root and root['name']!r}, not {CRATE}")
        crate = Crate(doc, args.src_root)
        roots, rows, gate_findings = read_gate(args.gate_file)
        surface = Surface(crate, roots)
    except Unreadable as err:
        print(f"base_interface.py: {err}", file=sys.stderr)
        return 2

    findings = gate_findings + surface.findings
    if surface.calls < args.min_calls:
        findings.append(("floor", f"{surface.calls} private calls, fewer than {args.min_calls}: the parse missed numbers.rs"))
    if len(surface.lines) < args.min_lines:
        findings.append(("floor", f"{len(surface.lines)} surface lines, fewer than {args.min_lines}: the closure reached almost nothing"))
    findings += ledger_findings(rows, surface.base_interface, surface.base_interface_min)
    digest = surface.digest()
    recorded = {n: d for _, n, d in rows}
    current = surface.base_interface

    if args.mode == "print":
        for line in surface.sorted_lines():
            print(line)
        for tag, msg in findings:
            print(f"base_interface.py: warning: {tag}: {msg}", file=sys.stderr)
        return 0

    if args.mode == "record":
        if findings:
            for tag, msg in findings:
                print(f"{tag}\t{msg}")
            print("base_interface.py: refusing to record a surface the gate does not accept", file=sys.stderr)
            return 1
        last = len(rows)
        if current in recorded:
            if recorded[current] == digest:
                print(f"base_interface.py: interface {current} is already recorded with this surface; nothing to do", file=sys.stderr)
                return 0
            print(f"base_interface.py: interface {current} is already recorded with another surface "
                  f"({recorded[current][:12]}…, now {digest[:12]}…). A recorded row is never rewritten: "
                  f"this surface needs BASE_INTERFACE = {last + 1}", file=sys.stderr)
            return 1
        with open(args.gate_file, "rb") as fh:
            data = fh.read()
        with open(args.gate_file, "a", encoding="utf-8") as fh:
            if data and not data.endswith(b"\n"):
                fh.write("\n")
            fh.write(f"interface {current} {digest}\n")
        print(f"base_interface.py: recorded interface {current} {digest}", file=sys.stderr)
        return 0

    # A surface that could not be read in full has no meaningful digest.
    if not findings:
        if current in recorded:
            if recorded[current] != digest:
                findings.append(("bump", f"interface {current} is recorded as {recorded[current]}, but the surface now digests to {digest}"))
        else:
            findings.append(("unrecorded", f"BASE_INTERFACE = {current} has no ledger row; the surface digests to {digest}"))
    for tag, msg in findings:
        print(f"{tag}\t{msg}")
    print(f"info\t{surface.calls}\t{len(surface.lines)}\t{digest}")
    return 1 if findings else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        traceback.print_exc()
        print("base_interface.py: the parser failed; refusing to report on the surface", file=sys.stderr)
        sys.exit(2)

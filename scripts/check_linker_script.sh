#!/usr/bin/env bash
# Hold a linker to the linker-script constructs link.ld depends on, and fail
# when reality and scripts/gates/linker/<linker>.txt disagree in either
# direction.
#
# The kernel's image is not a default layout with a base address applied: it
# is eleven linker registries whose spans check_registry_sections.sh holds to
# whole entries, a .limine_requests section the bootloader only finds in the
# first LOAD segment, three PHDRS with declared flags, and four early page
# tables carved out of the location counter past _bss_end. A linker that
# accepts `-T link.ld` and lays the image out its own way produces a kernel
# that links, boots nothing, and says nothing about why.
#
# That is the failure this gate is built around, and it is the one observed.
# wild 0.10.0 refuses link.ld on `. = KERNEL_VIRT_BASE`, and given the shape
# it does accept it keeps the script's section order and still starts the
# image 0x13e8 past the base. A refusal is a bad day; that is a wrong image.
# What each verdict conflates is recorded per verdict in
# scripts/gates/linker/wild.txt, because they do not age alike: the refusal is
# already fixed upstream and unreleased, the section ordering is a stated
# design difference wild's own test suite excuses, and the base offset is a
# disagreement about whether `. = X` places the headers or the first section.
#
# Two rules follow. Each probe's script carries the construct under test and
# nothing else a probe grades, or the linker's support for the scaffolding
# gets reported under the probe's name. And one probe, composed-layout, breaks
# that rule on purpose, because a linker can take every construct alone and
# compose them differently — wild reorders output sections when no PHDRS is
# declared and keeps the order when one is, so the minimal ordering verdict
# describes a script shape link.ld does not have.
#
# A coverage check runs beside the probes. Every construct probed here must
# actually appear in link.ld (a dead probe measures nothing), every uppercase
# keyword link.ld uses must have a probe (a construct added to the script
# without one would be graded by nothing), and every keyword's probe must have
# reported. All three fail.
#
# A linker not on PATH is reported as `skipped`, not as failing: `wild` is an
# optional `cargo install`, and a CI job without it still has a meaningful
# run of the linker the tree ships on.
#
# Usage:
#     scripts/check_linker_script.sh --linker lld
#     scripts/check_linker_script.sh --linker wild
#     scripts/check_linker_script.sh --linker wild --emit-allowlist
#     scripts/check_linker_script.sh --self-test
#
# --require turns a "skipped" into a failure, for the CI job whose only reason
# to exist is re-asking the question: an uninstalled candidate there is a
# question that stopped being asked, not a clean run.
#
# --gate-data-dir and --link-script point the gate at another expectations
# directory and another script, for reading a fresh measurement before
# promoting it. Coverage is checked on the --emit-allowlist path too, so a
# construct added to link.ld must get a probe before its verdicts can be
# recorded.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

LINKER=""
EMIT_ALLOWLIST=0
SELF_TEST=0
REQUIRE=0
GATE_DATA_DIR="$SCRIPT_DIR/gates/linker"
LINK_SCRIPT="$REPO_ROOT/link.ld"

while [ $# -gt 0 ]; do
    case "$1" in
        --linker)         LINKER="${2:?--linker needs a value}"; shift 2 ;;
        --emit-allowlist) EMIT_ALLOWLIST=1; shift ;;
        --self-test)      SELF_TEST=1; shift ;;
        --require)        REQUIRE=1; shift ;;
        --gate-data-dir)  GATE_DATA_DIR="${2:?--gate-data-dir needs a value}"; shift 2 ;;
        --link-script)    LINK_SCRIPT="${2:?--link-script needs a value}"; shift 2 ;;
        *) echo "check_linker_script: unknown option $1" >&2; exit 2 ;;
    esac
done

# The higher half link.ld places the kernel at, as the linker prints it.
# Compared as text, because bash's signed 64-bit arithmetic wraps it.
PROBE_BASE_HEX=ffffffff80000000
PROBE_BASE="0x$PROBE_BASE_HEX"
PROBE_GAP=0x1000

WORK=""
LINK_CMD=()
LINKER_VERSION=""
READOBJ=""

# ---------------------------------------------------------------------------
# The fixture object
# ---------------------------------------------------------------------------
#
# Synthesised with llc rather than taken from a build: the probes need one
# input section per output section link.ld names a class of, including a
# registry section nothing references (so KEEP is the only thing that can
# save it) and a note section that /DISCARD/ has to drop. llc's own
# .eh_frame, .comment and .note.GNU-stack are stripped, so every allocatable
# section in the object is one a probe script names — an unnamed section is an
# orphan, and where a linker places an orphan is its own policy rather than
# something the script said.

write_fixture_object() {
    local llc objcopy ir="$WORK/fixture.ll"
    llc="$("$SCRIPT_DIR/llvm_tool.sh" llc)"
    objcopy="$("$SCRIPT_DIR/llvm_tool.sh" llvm-objcopy)"
    cat > "$ir" <<'EOF'
target triple = "x86_64-unknown-none-elf"

@probe_rodata = constant i64 1, section ".rodata"
@probe_registry_entry = constant i64 2, section ".probe_registry"
@probe_data = global i64 3, section ".data"
@probe_bss = global i64 0, section ".bss"
@probe_note = constant i64 4, section ".note.probe"
@probe_common = common global i64 0

define void @_start() {
  ret void
}

define void @probe_entry_target() {
  ret void
}
EOF
    "$llc" -mtriple=x86_64-unknown-none-elf -filetype=obj -o "$WORK/fixture.o" "$ir"
    "$objcopy" --remove-section=.eh_frame --remove-section=.comment \
        --remove-section=.note.GNU-stack "$WORK/fixture.o"

    # The floor under the probes that read an absence: "the section is gone" is
    # also what a section the fixture never carried produces.
    local missing="" name
    for name in .text .rodata .probe_registry .note.probe .data .bss; do
        if [ -z "$(section_field "$WORK/fixture.o" "$name" type)" ]; then
            missing="$missing $name"
        fi
    done
    if [ -n "$missing" ]; then
        echo "check_linker_script: the fixture object carries no${missing}." >&2
        echo "  Probes read absences, so the run would be meaningless." >&2
        exit 2
    fi
}

fixture_tail() {
    printf '  .rodata : { *(.rodata) }\n'
    printf '  .probe_registry : { *(.probe_registry) }\n'
    printf '  .note.probe : { *(.note.probe) }\n'
    printf '  .data : { *(.data) }\n'
    printf '  .bss : { *(.bss) }\n'
}

# ---------------------------------------------------------------------------
# Reading the output
# ---------------------------------------------------------------------------

section_field() {
    local elf="$1" name="$2" field="$3"
    "$READOBJ" --elf-output-style=GNU --section-headers "$elf" 2>/dev/null \
        | awk -v want="$name" -v field="$field" '
            /^ *\[ *[0-9]+\]/ {
                sub(/^ *\[ *[0-9]+\] */, "");
                if ($1 != want || found) next;
                print (field == "type" ? $2 : (field == "size" ? $5 : $3));
                found = 1;
            }'
}

section_order() {
    local elf="$1"
    shift
    "$READOBJ" --elf-output-style=GNU --section-headers "$elf" 2>/dev/null \
        | awk -v keep=" $* " '
            /^ *\[ *[0-9]+\]/ {
                sub(/^ *\[ *[0-9]+\] */, "");
                if (index(keep, " " $1 " ") && $3 ~ /^[0-9a-f]+$/ && $3 !~ /^0+$/) print $3, $1;
            }' \
        | LC_ALL=C sort -s -k1,1 | awk '{ printf "%s ", $2 }'
}

symbol_value() {
    "$READOBJ" --elf-output-style=GNU --symbols "$1" 2>/dev/null \
        | awk -v want="$2" '$NF == want && !found { print $2; found = 1 }'
}

# llvm-readelf prints e_entry unpadded and a symbol value padded to sixteen
# digits, so one of the two has to be normalised before they can be compared.
elf_class_and_machine() {
    "$READOBJ" --elf-output-style=GNU --file-headers "$1" 2>/dev/null \
        | awk '/^ *Class:/ { c = $2 } /^ *Machine:/ { m = $NF } END { print c, m }'
}

entry_point() {
    "$READOBJ" --elf-output-style=GNU --file-headers "$1" 2>/dev/null \
        | awk '/Entry point address:/ {
                   v = $NF; sub(/^0x/, "", v);
                   while (length(v) < 16) v = "0" v;
                   print v;
               }'
}

# One line per PT_LOAD: its permission letters, in header order. llvm-readelf
# writes them as one field when adjacent (`RW`) and as two when not (`R E`).
load_segment_flags() {
    "$READOBJ" --elf-output-style=GNU --program-headers "$1" 2>/dev/null \
        | awk '$1 == "LOAD" { flags = ""; for (i = 7; i <= NF; i++) if ($i ~ /^[RWE]+$/) flags = flags $i; print flags }'
}

# The first PT_LOAD's virtual address, unpadded-to-padded like entry_point:
# .limine_requests has to be inside the first LOAD segment or the bootloader
# never finds the requests, and that is a segment property, not a section one.
first_load_vaddr() {
    "$READOBJ" --elf-output-style=GNU --program-headers "$1" 2>/dev/null \
        | awk '$1 == "LOAD" && !found {
                   v = $3; sub(/^0x/, "", v);
                   while (length(v) < 16) v = "0" v;
                   print v; found = 1;
               }'
}

# ---------------------------------------------------------------------------
# The probes
# ---------------------------------------------------------------------------
#
# Each appends one `<construct> <TAB> has|lacks|unknown <TAB> <detail>` line.
# `unknown` matches no recorded verdict, so a probe that could not run fails
# the run rather than reading as a measured absence.

FINDINGS=""

# `lacks` when the linker said why, `unknown` when it did not: a failure with
# no message is a broken invocation, and it must not read as a measurement.
record_link_failure() {
    local construct="$1" reason
    reason="$(link_error)"
    if [ -n "$reason" ]; then
        record "$construct" lacks "$reason"
    else
        record "$construct" unknown "the link failed with no diagnostic"
    fi
}

record() {
    FINDINGS="${FINDINGS}$(printf '%s\t%s\t%s' "$1" "$2" "$3")
"
}

# Section GC is off unless a probe asks for it, so that every verdict but
# `keep`'s is about the construct under test. wild defaults it on and lld
# defaults it off, and a probe that inherited either default would be
# measuring the default.
try_link() {
    local out="$WORK/out.elf"
    [ $# -eq 0 ] && set -- --no-gc-sections
    cat > "$WORK/probe.ld"
    rm -f "$out"
    if "${LINK_CMD[@]}" -T "$WORK/probe.ld" "$WORK/fixture.o" -o "$out" "$@" \
        > "$WORK/link.err" 2>&1
    then
        printf '%s\n' "$out"
        return 0
    fi
    return 1
}

link_error() {
    sed -n 's/^[^:]*: *error: *//p; s/^error: *//p' "$WORK/link.err" | sed -n '1p'
}

probe_output_format() {
    local elf
    elf="$(try_link <<EOF
OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
$(fixture_tail)
}
EOF
    )" || { record_link_failure output-format; return; }
    local header
    header="$(elf_class_and_machine "$elf")"
    if [ "$header" != "ELF64 X86-64" ]; then
        record output-format lacks "the output is ${header:-unreadable}"
        return
    fi
    # The control: the class and machine come from the input object, so a
    # linker that parses the directives and ignores them would pass on them
    # alone. Naming a format the input contradicts has to change the answer.
    elf="$(try_link <<EOF
OUTPUT_FORMAT(elf64-littleaarch64)
OUTPUT_ARCH(aarch64)
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
$(fixture_tail)
}
EOF
    )" || { record output-format has "a contradicting OUTPUT_FORMAT is refused"; return; }
    header="$(elf_class_and_machine "$elf")"
    if [ "$header" = "ELF64 X86-64" ]; then
        record output-format lacks "a contradicting OUTPUT_FORMAT still produced $header"
    else
        record output-format has "the named format and architecture are what came out"
    fi
}

probe_entry() {
    local elf
    elf="$(try_link <<EOF
ENTRY(probe_entry_target)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
$(fixture_tail)
}
EOF
    )" || { record_link_failure entry; return; }
    local entry target default
    entry="$(entry_point "$elf")"
    target="$(symbol_value "$elf" probe_entry_target)"
    default="$(symbol_value "$elf" _start)"
    if [ -z "$entry" ] || [ -z "$target" ] || [ "$target" = "$default" ]; then
        record entry unknown "e_entry 0x${entry:-?}, target 0x${target:-?}, _start 0x${default:-?}"
    elif [ "$entry" = "$target" ]; then
        record entry has "e_entry is the named symbol, not the default"
    else
        record entry lacks "e_entry 0x$entry is not probe_entry_target at 0x$target"
    fi
}

# Read back through a second symbol rather than through a section's address:
# what is under test is whether the definition is visible, and using it to
# place a section would fold in explicit-base and section-order as well.
probe_top_level_assign() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
PROBE_TOP_LEVEL = $PROBE_BASE;
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
$(fixture_tail)
}
EOF
    )" || { record_link_failure top-level-assign; return; }
    local mark
    mark="$(symbol_value "$elf" PROBE_TOP_LEVEL)"
    if [ "$mark" = "$PROBE_BASE_HEX" ]; then
        record top-level-assign has "a symbol defined before SECTIONS reaches the symbol table"
    else
        record top-level-assign lacks "PROBE_TOP_LEVEL is ${mark:+0x}${mark:-absent}$mark, not $PROBE_BASE"
    fi
}

# link.ld's own form, and the one it is refused on: the location counter set
# from a symbol rather than from a literal.
probe_dot_from_symbol() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
PROBE_BASE = $PROBE_BASE;
SECTIONS {
  . = PROBE_BASE;
  .text : { *(.text) }
$(fixture_tail)
}
EOF
    )" || { record_link_failure dot-from-symbol; return; }
    local lowest
    lowest="$(lowest_section_addr "$elf")"
    if [ -z "$lowest" ]; then
        record dot-from-symbol unknown "the link produced no allocatable section"
    elif [ "$lowest" = "$PROBE_BASE_HEX" ]; then
        record dot-from-symbol has "the location counter takes a symbol"
    else
        record dot-from-symbol lacks "the image starts at 0x$lowest, not $PROBE_BASE"
    fi
}

lowest_section_addr() {
    local name
    name="$(section_order "$1" .text .rodata .probe_registry .note.probe .data .bss \
        | awk '{ print $1 }')"
    if [ -z "$name" ]; then
        return 0
    fi
    section_field "$1" "$name" addr
}

# Where the image *starts*, which is a different question from which section
# comes first — that is section-order's. Reporting a reorder twice would make
# one finding look like two.
probe_explicit_base() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
$(fixture_tail)
}
EOF
    )" || { record_link_failure explicit-base; return; }
    local lowest
    lowest="$(lowest_section_addr "$elf")"
    if [ -z "$lowest" ]; then
        record explicit-base unknown "the link produced no allocatable section"
    elif [ "$lowest" = "$PROBE_BASE_HEX" ]; then
        record explicit-base has "the image starts on the location counter"
    else
        record explicit-base lacks "the lowest section is at 0x$lowest, not $PROBE_BASE"
    fi
}

probe_section_order() {
    local elf want=".text .rodata .probe_registry .note.probe .data .bss"
    elf="$(try_link <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
$(fixture_tail)
}
EOF
    )" || { record_link_failure section-order; return; }
    local got
    got="$(section_order "$elf" $want)"
    if [ "$got" = "$want " ]; then
        record section-order has "output sections follow the script"
    else
        record section-order lacks "laid out as ${got% }"
    fi
}

probe_output_align_expr() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text ALIGN(4096) : { *(.text) }
  .rodata ALIGN(4096) : { *(.rodata) }
  .probe_registry : { *(.probe_registry) }
  .note.probe : { *(.note.probe) }
  .data ALIGN(4096) : { *(.data) }
  .bss : { *(.bss) }
}
EOF
    )" || { record_link_failure output-align-expr; return; }
    local addr
    addr="$(section_field "$elf" .data addr)"
    if [ -z "$addr" ]; then
        record output-align-expr unknown ".data is absent from the output"
    elif [ "${addr%000}" != "$addr" ]; then
        record output-align-expr has "an ALIGN() address expression is honoured"
    else
        record output-align-expr lacks ".data at 0x$addr is not page-aligned"
    fi
}

probe_in_section_assign() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { probe_text_start = .; *(.text) probe_text_end = .; }
$(fixture_tail)
}
EOF
    )" || { record_link_failure in-section-assign; return; }
    # Against .text's own address rather than the base: where the section
    # landed is explicit-base's and section-order's question, and a bracket
    # that follows a misplaced section is still a working bracket.
    local start end addr size
    addr="$(section_field "$elf" .text addr)"
    size="$(section_field "$elf" .text size)"
    start="$(symbol_value "$elf" probe_text_start)"
    end="$(symbol_value "$elf" probe_text_end)"
    if [ -n "$addr" ] && [ -n "$size" ] && [ "$start" = "$addr" ] \
        && [ -n "$end" ] && [ $(( 0x$end - 0x$start )) -eq $(( 0x$size )) ]
    then
        record in-section-assign has "symbols bracket exactly the section they are written in"
    else
        record in-section-assign lacks "0x$start..0x$end brackets a 0x$size .text at 0x$addr"
    fi
}

probe_symbol_alias() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) probe_text_end = .; }
  probe_alias = probe_text_end;
$(fixture_tail)
}
EOF
    )" || { record_link_failure symbol-alias; return; }
    local alias target
    alias="$(symbol_value "$elf" probe_alias)"
    target="$(symbol_value "$elf" probe_text_end)"
    if [ -n "$alias" ] && [ "$alias" = "$target" ]; then
        record symbol-alias has "one script symbol can be defined from another"
    else
        record symbol-alias lacks "alias 0x$alias, target 0x$target"
    fi
}

# Two links: a KEEP that survives is only evidence if the same section
# without KEEP does not. A linker that ignores --gc-sections passes the first
# and fails the second.
probe_keep() {
    local elf
    elf="$(try_link --gc-sections <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
  .probe_registry : { KEEP(*(.probe_registry)) }
  .rodata : { *(.rodata) }
  .note.probe : { *(.note.probe) }
  .data : { *(.data) }
  .bss : { *(.bss) }
}
EOF
    )" || { record_link_failure keep; return; }
    if [ -z "$(section_field "$elf" .probe_registry addr)" ]; then
        record keep lacks "--gc-sections dropped a KEEP section"
        return
    fi
    elf="$(try_link --gc-sections <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
  .probe_registry : { *(.probe_registry) }
  .rodata : { *(.rodata) }
  .note.probe : { *(.note.probe) }
  .data : { *(.data) }
  .bss : { *(.bss) }
}
EOF
    )" || { record keep unknown "the control link failed: $(link_error)"; return; }
    if [ -n "$(section_field "$elf" .probe_registry addr)" ]; then
        record keep lacks "an unreferenced section survives without KEEP too"
    else
        record keep has "KEEP is what saves an unreferenced section from --gc-sections"
    fi
}

probe_assert_sizeof() {
    if ! try_link >/dev/null <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
$(fixture_tail)
  ASSERT(SIZEOF(.text) >= 1, "probe: .text is empty")
}
EOF
    then
        record assert-sizeof lacks "a satisfied ASSERT failed the link: $(link_error)"
        return
    fi
    # An ASSERT that cannot fail is not support for ASSERT.
    if try_link >/dev/null <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
$(fixture_tail)
  ASSERT(SIZEOF(.text) >= 4096, "probe: .text is too small")
}
EOF
    then
        record assert-sizeof lacks "a violated ASSERT still linked"
    else
        record assert-sizeof has "ASSERT(SIZEOF()) fires, and only when violated"
    fi
}

probe_noload() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
  .rodata : { *(.rodata) }
  .probe_registry (NOLOAD) : { *(.probe_registry) }
  .note.probe : { *(.note.probe) }
  .data : { *(.data) }
  .bss : { *(.bss) *(COMMON) }
}
EOF
    )" || { record_link_failure noload; return; }
    # On .probe_registry, not .bss: the input .bss is already NOBITS, so the
    # output would be NOBITS with or without the specifier.
    local type common bss_addr
    type="$(section_field "$elf" .probe_registry type)"
    # `*(COMMON)` is in this script's .bss and the fixture carries a common
    # symbol, so the keyword the coverage table maps here has a verdict
    # depending on it rather than only being parsed.
    common="$(symbol_value "$elf" probe_common)"
    bss_addr="$(section_field "$elf" .bss addr)"
    if [ -z "$type" ] || [ -z "$common" ] || [ -z "$bss_addr" ]; then
        record noload unknown ".probe_registry is ${type:-absent}, probe_common ${common:-absent}"
    elif [ "$type" != "NOBITS" ]; then
        record noload lacks ".probe_registry is still $type"
    elif [ $(( 0x$common - 0x$bss_addr )) -lt 0 ]; then
        record noload lacks "probe_common at 0x$common is before .bss at 0x$bss_addr"
    else
        record noload has "a PROGBITS section occupies no file space; COMMON lands in .bss"
    fi
}

# link.ld spells .bss's alignment after the colon, which is a different
# production from the address expression output-align-expr measures.
probe_section_align_attr() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
  .rodata : { *(.rodata) }
  .probe_registry : { *(.probe_registry) }
  .note.probe : { *(.note.probe) }
  .data : ALIGN(4096) { *(.data) }
  .bss : { *(.bss) *(COMMON) }
}
EOF
    )" || { record_link_failure section-align-attr; return; }
    local addr
    addr="$(section_field "$elf" .data addr)"
    if [ -z "$addr" ]; then
        record section-align-attr unknown ".data is absent from the output"
    elif [ "${addr%000}" != "$addr" ]; then
        record section-align-attr has "a post-colon ALIGN() is honoured"
    else
        record section-align-attr lacks ".data at 0x$addr is not page-aligned"
    fi
}

probe_phdrs() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
PHDRS { text PT_LOAD FLAGS(7); rodata PT_LOAD FLAGS(4); data PT_LOAD FLAGS(6); }
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) } :text
  .rodata : { *(.rodata) } :rodata
  .probe_registry : { *(.probe_registry) } :rodata
  .note.probe : { *(.note.probe) } :rodata
  .data : { *(.data) } :data
  .bss : { *(.bss) } :data
}
EOF
    )" || { record_link_failure phdrs; return; }
    # FLAGS(7) on the text segment, which section flags alone cannot produce:
    # asserting RE R RW would pass on a linker that ignored PHDRS entirely and
    # grouped by section flags.
    local got
    got="$(load_segment_flags "$elf" | tr '\n' ' ')"
    if [ "$got" = "RWE R RW " ]; then
        record phdrs has "three PT_LOADs carry the declared flags"
    else
        record phdrs lacks "PT_LOAD flags are ${got% }"
    fi
}

probe_discard() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
  .rodata : { *(.rodata) }
  .probe_registry : { *(.probe_registry) }
  .data : { *(.data) }
  .bss : { *(.bss) }
  /DISCARD/ : { *(.note*) }
}
EOF
    )" || { record_link_failure discard; return; }
    if [ -z "$(section_field "$elf" .note.probe addr)" ]; then
        record discard has "/DISCARD/ drops a matched section"
    else
        record discard lacks ".note.probe survived /DISCARD/"
    fi
}

probe_location_gap() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE;
  .text : { *(.text) }
$(fixture_tail)
  probe_gap_start = .;
  . = . + $PROBE_GAP;
  probe_gap_end = .;
}
EOF
    )" || { record_link_failure location-gap; return; }
    local start end
    start="$(symbol_value "$elf" probe_gap_start)"
    end="$(symbol_value "$elf" probe_gap_end)"
    if [ -z "$start" ] || [ -z "$end" ]; then
        record location-gap unknown "the bracket symbols are 0x${start:-?}..0x${end:-?}"
    elif [ $(( 0x$end - 0x$start )) -eq $(( PROBE_GAP )) ]; then
        record location-gap has "a reservation past the last section"
    else
        record location-gap lacks "the gap is 0x$(( 0x$end - 0x$start )) bytes, not $PROBE_GAP"
    fi
}

# The third ALIGN production link.ld uses: on the location counter, which is
# how the four early page tables reach a page boundary.
probe_dot_align() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
SECTIONS {
  . = $PROBE_BASE + 8;
  .text : { *(.text) }
$(fixture_tail)
  . = ALIGN(0x1000);
  probe_aligned = .;
}
EOF
    )" || { record_link_failure dot-align; return; }
    local mark
    mark="$(symbol_value "$elf" probe_aligned)"
    if [ -z "$mark" ]; then
        record dot-align unknown "probe_aligned is absent from the symbol table"
    elif [ "${mark%000}" != "$mark" ]; then
        record dot-align has "ALIGN() on the location counter is honoured"
    else
        record dot-align lacks "probe_aligned is 0x$mark"
    fi
}

# Deliberately *not* minimal, and the one carve-out from the rule beside
# run_probes: a linker can take every construct alone and still compose them
# differently. wild is that case — it reorders output sections when no PHDRS is
# declared and keeps script order when one is — so the minimal section-order
# verdict describes a shape link.ld does not have. Built only from constructs
# no linker here refuses, so the verdict is about composition, not support.
probe_composed_layout() {
    local elf
    elf="$(try_link <<EOF
ENTRY(_start)
PHDRS { text PT_LOAD FLAGS(5); rodata PT_LOAD FLAGS(4); data PT_LOAD FLAGS(6); }
SECTIONS {
  . = $PROBE_BASE;
  .probe_registry : { __start_probe = .; KEEP(*(.probe_registry)) __stop_probe = .; } :text
  .text : { *(.text) } :text
  .rodata : { *(.rodata) } :rodata
  .note.probe : { *(.note.probe) } :rodata
  .data : { *(.data) } :data
  .bss : { *(.bss) *(COMMON) } :data
}
EOF
    )" || { record_link_failure composed-layout; return; }
    local first lowest order load
    first="$(section_field "$elf" .probe_registry addr)"
    lowest="$(lowest_section_addr "$elf")"
    order="$(section_order "$elf" .probe_registry .text .rodata .note.probe .data .bss)"
    load="$(first_load_vaddr "$elf")"
    if [ -z "$first" ] || [ -z "$lowest" ] || [ -z "$load" ]; then
        record composed-layout unknown "the link produced no .probe_registry or no PT_LOAD"
    elif [ "$lowest" != "$PROBE_BASE_HEX" ]; then
        record composed-layout lacks "the image starts at 0x$lowest, not $PROBE_BASE"
    elif [ "$order" != ".probe_registry .text .rodata .note.probe .data .bss " ]; then
        record composed-layout lacks "laid out as ${order% }"
    elif [ "$first" != "$PROBE_BASE_HEX" ]; then
        record composed-layout lacks "the first declared section is at 0x$first"
    elif [ "$load" != "$first" ]; then
        record composed-layout lacks "the first PT_LOAD starts at 0x$load, not at 0x$first"
    else
        record composed-layout has "a kernel-shaped script lays out as written"
    fi
}

# Each probe's script carries the construct under test and nothing else a probe
# grades, or the linker's support for the scaffolding gets reported under the
# probe's name. The floor that is not counted is `ENTRY(_start)` plus a base
# assignment — a literal one, except in dot-align, whose ALIGN needs a base
# that is not already aligned. symbol-alias and dot-from-symbol need an
# in-section assignment and a top-level definition to have anything to work on,
# and composed-layout above is the deliberate exception.
run_probes() {
    probe_output_format
    probe_entry
    probe_top_level_assign
    probe_dot_from_symbol
    probe_explicit_base
    probe_section_order
    probe_output_align_expr
    probe_section_align_attr
    probe_in_section_assign
    probe_symbol_alias
    probe_keep
    probe_assert_sizeof
    probe_noload
    probe_phdrs
    probe_discard
    probe_location_gap
    probe_dot_align
    probe_composed_layout
}

# ---------------------------------------------------------------------------
# Coverage against link.ld
# ---------------------------------------------------------------------------
#
# Keyword <TAB> the probe that covers it. A keyword link.ld uses that is
# absent here is a construct nothing grades; a keyword here that link.ld has
# stopped using is a dead probe; a probe named here that emitted no verdict is
# a rename this table did not follow. All three fail.
#
# Nine probed constructs have no keyword of their own and are absent here by
# construction: top-level-assign, dot-from-symbol, explicit-base,
# section-order, in-section-assign, symbol-alias, location-gap and
# composed-layout are syntactic or compositional, and ALIGN spells three
# separate productions of which this row names one — section-align-attr and
# dot-align are the other two.
PROBED_KEYWORDS='
OUTPUT_FORMAT	output-format
OUTPUT_ARCH	output-format
ENTRY	entry
SECTIONS	explicit-base
PHDRS	phdrs
PT_LOAD	phdrs
FLAGS	phdrs
ALIGN	output-align-expr
KEEP	keep
ASSERT	assert-sizeof
SIZEOF	assert-sizeof
NOLOAD	noload
COMMON	noload
DISCARD	discard
'

# Comments and string literals stripped (an ASSERT's message is prose), symbols
# the script defines itself subtracted, and a token must start with a letter —
# which is what keeps the `__start_<registry>` symbols out.
link_script_keywords() {
    local body defined
    body="$(awk '{
        rest = $0; out = "";
        while (rest != "") {
            if (in_comment) {
                p = index(rest, "*/");
                if (p == 0) break;
                rest = substr(rest, p + 2); in_comment = 0;
            } else {
                p = index(rest, "/*");
                if (p == 0) { out = out rest; break }
                out = out substr(rest, 1, p - 1);
                rest = substr(rest, p + 2); in_comment = 1;
            }
        }
        gsub(/"[^"]*"/, "", out);
        print out;
    }' "$LINK_SCRIPT")"
    defined="$(printf '%s\n' "$body" | sed -n 's/^[[:space:]]*\([A-Za-z_][A-Za-z_0-9]*\)[[:space:]]*=.*/\1/p' | LC_ALL=C sort -u)"
    printf '%s\n' "$body" \
        | awk '{ n = split($0, t, /[^A-Za-z0-9_]+/); for (i = 1; i <= n; i++) if (t[i] ~ /^[A-Z][A-Z_0-9]*$/) print t[i] }' \
        | LC_ALL=C sort -u \
        | LC_ALL=C comm -23 - <(printf '%s\n' "$defined")
}

check_coverage() {
    local probed used missing dead unrun fail=0
    probed="$(printf '%s' "$PROBED_KEYWORDS" | awk -F'\t' 'NF == 2 { print $1 }' | LC_ALL=C sort -u)"
    used="$(link_script_keywords)"

    missing="$(printf '%s\n' "$used" | LC_ALL=C comm -23 - <(printf '%s\n' "$probed"))"
    if [ -n "$missing" ]; then
        echo "check_linker_script: $(basename "$LINK_SCRIPT") uses construct(s) this gate does not probe:" >&2
        printf '%s\n' "$missing" | sed 's/^/      /' >&2
        fail=1
    fi

    dead="$(printf '%s\n' "$probed" | LC_ALL=C comm -23 - <(printf '%s\n' "$used"))"
    if [ -n "$dead" ]; then
        echo "check_linker_script: probe(s) for construct(s) $(basename "$LINK_SCRIPT") no longer uses:" >&2
        printf '%s\n' "$dead" | sed 's/^/      /' >&2
        fail=1
    fi

    unrun="$(printf '%s' "$PROBED_KEYWORDS" | awk -F'\t' 'NF == 2 { print $2 }' | LC_ALL=C sort -u \
        | LC_ALL=C comm -23 - <(printf '%s' "$FINDINGS" | awk -F'\t' 'NF >= 2 { print $1 }' | LC_ALL=C sort -u))"
    if [ -n "$unrun" ]; then
        echo "check_linker_script: keyword(s) mapped to a probe that emitted no verdict:" >&2
        printf '%s\n' "$unrun" | sed 's/^/      /' >&2
        fail=1
    fi

    if [ "$fail" -ne 0 ]; then
        return 1
    fi
    printf 'check_linker_script: coverage OK — %d keyword(s) in %s, all probed\n' \
        "$(printf '%s\n' "$used" | grep -c .)" "$(basename "$LINK_SCRIPT")"
}

# ---------------------------------------------------------------------------
# The gate
# ---------------------------------------------------------------------------

emit_allowlist() {
    # An `unknown` recorded as an expectation would make every later run
    # pass on a measurement that never happened.
    if printf '%s' "$FINDINGS" | awk -F'\t' '$2 == "unknown" { found = 1 } END { exit !found }'; then
        echo "check_linker_script: refusing to emit — some verdicts are unknown:" >&2
        printf '%s' "$FINDINGS" | awk -F'\t' '$2 == "unknown" { printf "      %s: %s\n", $1, $3 }' >&2
        exit 2
    fi
    printf '# check_linker_script expectations — linker: %s\n#\n' "$LINKER"
    printf '# Measured by: scripts/check_linker_script.sh --linker %s --emit-allowlist\n#\n' "$LINKER"
    printf '# <construct> <TAB> has|lacks\n#\n'
    printf '# A `lacks` the probe finds present fails the run just as a `has` that\n'
    printf '# regressed does: this file is what makes "not yet" a measurement.\n#\n'
    printf '# Re-emitting prints the rows, not the reasons — the tracked file says\n'
    printf '# what each verdict costs, and that prose is written by hand.\n\n'
    printf '%s' "$FINDINGS" | awk -F'\t' 'NF >= 2 { printf "%s\t%s\n", $1, $2 }'
}

# An empty side would otherwise reach diff as one blank line and come back as
# a finding with nothing in it.
verdict_lines() {
    printf '%s\n' "$1" | sed '/^$/d'
}

compare_against_allowlist() {
    local allowlist="$GATE_DATA_DIR/$LINKER.txt"
    if [ ! -f "$allowlist" ]; then
        echo "check_linker_script: no expectations for linker '$LINKER' at $allowlist" >&2
        echo "  Record them with --emit-allowlist." >&2
        exit 2
    fi

    # A space where a tab belongs reads as a set difference on every row, which
    # points at the measurement instead of at the typo.
    local malformed
    # awk, not grep: an ERE has no \t, so a grep pattern would read every row
    # as malformed.
    malformed="$(awk -F'\t' '
        /^[[:space:]]*(#|$)/ { next }
        NF == 2 && $1 ~ /^[a-z0-9-]+$/ && ($2 == "has" || $2 == "lacks") { next }
        { bad = bad "      " FNR ": " $0 "\n"; n++ }
        END { printf "%d\n%s", n + 0, bad }' "$allowlist")"
    if [ "$(printf '%s' "$malformed" | sed -n '1p')" != "0" ]; then
        echo "check_linker_script: $allowlist has row(s) that are not '<name><TAB>has|lacks'" >&2
        printf '%s' "$malformed" | sed -n '2,$p' >&2
        return 1
    fi

    local expected observed
    expected="$(awk -F'\t' '!/^[[:space:]]*(#|$)/ && NF >= 2 { printf "%s\t%s\n", $1, $2 }' \
        "$allowlist" | LC_ALL=C sort)"
    observed="$(printf '%s' "$FINDINGS" | awk -F'\t' 'NF >= 2 { printf "%s\t%s\n", $1, $2 }' | LC_ALL=C sort)"

    if [ "$expected" = "$observed" ]; then
        local has_count lacks_count
        has_count="$(printf '%s\n' "$observed" | grep -c '	has$' || true)"
        lacks_count="$(printf '%s\n' "$observed" | grep -c '	lacks$' || true)"
        printf 'check_linker_script: OK — linker=%s, %d construct(s) supported, %d not\n' \
            "$LINKER" "$has_count" "$lacks_count"
        if [ -n "$LINKER_VERSION" ]; then
            printf '  measured with: %s\n' "$LINKER_VERSION"
        fi
        printf '%s' "$FINDINGS" | awk -F'\t' 'NF >= 3 { printf "  %-18s %-6s %s\n", $1, $2, $3 }'
        return 0
    fi

    echo "check_linker_script: FAIL — linker=$LINKER disagrees with $allowlist" >&2
    # `diff` exits 1 exactly when this branch is reached, and pipefail would
    # make that the pipeline's status and end the run before the remedy line.
    { diff <(verdict_lines "$expected") <(verdict_lines "$observed") || true; } \
        | sed -n 's/^< /  recorded but not observed: /p; s/^> /  observed but not recorded: /p' >&2
    printf '%s' "$FINDINGS" | awk -F'\t' 'NF >= 3 { printf "  %-18s %-6s %s\n", $1, $2, $3 }' >&2
    echo "  Re-measure with --emit-allowlist, restore the per-row prose it does not" >&2
    echo "  print, and say in the commit message what moved." >&2
    return 1
}

resolve_linker() {
    case "$LINKER" in
        lld)
            local rust_lld
            rust_lld="$("$SCRIPT_DIR/llvm_tool.sh" rust-lld)"
            LINK_CMD=("$rust_lld" -flavor gnu --no-pie)
            LINKER_VERSION="$("$rust_lld" -flavor gnu --version 2>/dev/null | sed -n '1p' || true)"
            ;;
        wild)
            local wild
            wild="$(command -v wild || true)"
            if [ -z "$wild" ]; then
                echo "check_linker_script: skipped — 'wild' is not on PATH" >&2
                echo "  Install it with: cargo install wild-linker" >&2
                # A shell redirection has already emptied the tracked file by
                # the time this runs; exit 2 says so rather than leaving the
                # empty file looking like a measurement. Restore it from git.
                if [ "$EMIT_ALLOWLIST" = "1" ] || [ "$REQUIRE" = "1" ]; then
                    exit 2
                fi
                exit 0
            fi
            LINK_CMD=("$wild" --no-pie)
            LINKER_VERSION="$("$wild" --version 2>/dev/null | sed -n '1p' || true)"
            ;;
        *)
            echo "check_linker_script: unknown linker '$LINKER' (known: lld, wild)" >&2
            exit 2
            ;;
    esac
}

main() {
    if [ -z "$LINKER" ]; then
        echo "check_linker_script: --linker is required" >&2
        exit 2
    fi
    resolve_linker

    READOBJ="$("$SCRIPT_DIR/llvm_tool.sh" llvm-readobj)"
    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT INT TERM
    write_fixture_object
    run_probes

    # stdout is the gate file when emitting, so the coverage note goes to stderr.
    if [ "$EMIT_ALLOWLIST" = "1" ]; then
        check_coverage >&2
        emit_allowlist
        exit 0
    fi
    check_coverage
    compare_against_allowlist
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------

self_test() {
    local root fail=0
    root="$(mktemp -d)"
    trap 'rm -rf "$root"' EXIT INT TERM
    echo "check_linker_script: self-test against built-in fixtures"

    GATE_DATA_DIR="$root"
    LINKER="fixture"
    printf 'phdrs\thas\nsection-order\tlacks\nkeep\thas\n' > "$root/fixture.txt"

    # `set +e` around the subshell rather than `|| got=$?`: the errexit
    # suppression an `&&`/`||` list applies reaches inside it, and `set -e`
    # there does not undo it — so the failure path would not be under errexit.
    expect() {
        local name="$1" want="$2" got
        set +e
        ( set -e; compare_against_allowlist ) > "$root/out" 2>&1
        got=$?
        set -e
        if [ "$got" -ne "$want" ]; then
            echo "check_linker_script --self-test: $name expected exit $want, got $got" >&2
            sed 's/^/      /' "$root/out" >&2
            fail=1
            return
        fi
        # Only the disagreement path (1) names that remedy; the operator-error
        # path (2) tells you to record a file instead.
        if [ "$want" -eq 1 ] && ! grep -q 'Re-measure with --emit-allowlist' "$root/out"; then
            echo "check_linker_script --self-test: $name rejected without reaching the remedy line" >&2
            sed 's/^/      /' "$root/out" >&2
            fail=1
            return
        fi
        echo "  $name: exits $got as expected"
    }

    FINDINGS="$(printf 'phdrs\thas\td\nsection-order\tlacks\td\nkeep\thas\td\n')
"
    expect "an exact match passes" 0

    FINDINGS="$(printf 'phdrs\thas\td\nsection-order\thas\td\nkeep\thas\td\n')
"
    expect "a construct gained since the last measurement fails" 1

    FINDINGS="$(printf 'phdrs\tlacks\td\nsection-order\tlacks\td\nkeep\thas\td\n')
"
    expect "a construct lost since the last measurement fails" 1

    FINDINGS="$(printf 'phdrs\tunknown\td\nsection-order\tunknown\td\nkeep\tunknown\td\n')
"
    expect "a probe that could not run fails rather than reading as absence" 1

    # The remedy the failure message names has to work, or it is not a remedy.
    FINDINGS="$(printf 'alpha\thas\td\nbeta\tlacks\td\ngamma\thas\td\n')
"
    emit_allowlist > "$root/fixture.txt"
    expect "a re-emitted allowlist compares equal to what produced it" 0

    rm -f "$root/fixture.txt"
    expect "a missing expectations file is an operator error, not a finding" 2

    expect_coverage() {
        local name="$1" want="$2" got
        set +e
        ( set -e; check_coverage ) > "$root/out" 2>&1
        got=$?
        set -e
        if [ "$got" -ne "$want" ]; then
            echo "check_linker_script --self-test: $name expected exit $want, got $got" >&2
            sed 's/^/      /' "$root/out" >&2
            fail=1
            return
        fi
        echo "  $name: exits $got as expected"
    }

    # Every probe reported, so the coverage cases below are about the script
    # and the keyword table rather than about a missing verdict.
    FINDINGS="$(printf '%s\thas\td\n' output-format entry top-level-assign \
        explicit-base section-order output-align-expr in-section-assign \
        symbol-alias keep assert-sizeof noload phdrs discard location-gap)
"
    LINK_SCRIPT="$root/probed.ld"
    # PROVIDE and SUBALIGN appear only inside a comment and inside a string,
    # and nowhere as real constructs: either stripper regressing fails here.
    cat > "$LINK_SCRIPT" <<'EOF'
/* PROVIDE and SUBALIGN appear in this comment and must not count. */
OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)
BASE = 0xffffffff80000000;
PHDRS { text PT_LOAD FLAGS(5); }
SECTIONS {
  . = BASE;
  .text ALIGN(4096) : { KEEP(*(.text)) } :text
  .bss (NOLOAD) : { *(.bss) *(COMMON) }
  ASSERT(SIZEOF(.text) >= 1, "PROVIDE SUBALIGN in a string")
  /DISCARD/ : { *(.note*) }
}
EOF
    expect_coverage "a script using exactly the probed constructs passes" 0

    printf '  PROVIDE(probe = 0);\n' >> "$LINK_SCRIPT"
    expect_coverage "a construct with no probe fails" 1

    cat > "$LINK_SCRIPT" <<'EOF'
ENTRY(_start)
SECTIONS { . = 0xffffffff80000000; .text : { *(.text) } }
EOF
    expect_coverage "a probe for a construct the script dropped fails" 1

    cat > "$LINK_SCRIPT" <<'EOF'
OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)
PHDRS { text PT_LOAD FLAGS(5); }
SECTIONS {
  . = 0xffffffff80000000;
  .text ALIGN(4096) : { KEEP(*(.text)) } :text
  .bss (NOLOAD) : { *(.bss) *(COMMON) }
  ASSERT(SIZEOF(.text) >= 1, "empty")
  /DISCARD/ : { *(.note*) }
}
EOF
    FINDINGS="$(printf '%s\thas\td\n' output-format entry)
"
    expect_coverage "a keyword mapped to a probe that reported nothing fails" 1

    # The parsers, against captured llvm-readelf and linker text. Everything
    # above tests the comparison; a parser that silently stops answering turns
    # every verdict into `lacks` and only the all-`has` tracked files would
    # notice.
    #
    # The fixture is deliberately awkward: the header rows are NOT in address
    # order, .comment sits at address 0, .note.probe is outside the requested
    # set, and the entry address is short — so the sort, the two filters and
    # the pad loop each have something to get wrong.
    cat > "$root/readobj.txt" <<'EOF'
There are 7 section headers, starting at offset 0x1118:

Section Headers:
  [Nr] Name              Type            Address          Off    Size   ES Flg Lk Inf Al
  [ 0]                   NULL            0000000000000000 000000 000000 00      0   0  0
  [ 1] .bss              NOBITS          ffffffff80002000 003000 000008 00  WA  0   0  1
  [ 2] .rodata           PROGBITS        ffffffff80001000 002000 000008 00   A  0   0  1
  [ 3] .text             PROGBITS        ffffffff80000000 001000 000006 00  AX  0   0  4
  [ 4] .note.probe       NOTE            ffffffff80001800 002800 000008 00   A  0   0  8
  [ 5] .comment          PROGBITS        0000000000000000 003008 00005e 01  MS  0   0  1

Elf file type is EXEC (Executable file)
Entry point address:               0x401000
Program Headers:
  Type           Offset   VirtAddr           PhysAddr           FileSiz  MemSiz   Flg Align
  LOAD           0x001000 0xffffffff80000000 0xffffffff80000000 0x000006 0x000006 R E 0x1000
  LOAD           0x002000 0xffffffff80001000 0xffffffff80001000 0x000008 0x000008 R   0x1000
  LOAD           0x003000 0xffffffff80002000 0xffffffff80002000 0x000000 0x000008 RW  0x1000

Symbol table '.symtab' contains 4 entries:
   Num:    Value          Size Type    Bind   Vis       Ndx Name
     0: 0000000000000000     0 NOTYPE  LOCAL  DEFAULT   UND
     1: ffffffff80000000     0 NOTYPE  GLOBAL DEFAULT     1 _start
     2: ffffffff80001008     0 NOTYPE  GLOBAL DEFAULT     2 probe_gap_end
     3: ffffffff80002000     0 NOTYPE  GLOBAL DEFAULT     1 probe_common
EOF
    printf '#!/bin/sh\ncat "%s"\n' "$root/readobj.txt" > "$root/readobj"
    chmod +x "$root/readobj"
    READOBJ="$root/readobj"

    expect_parse() {
        local name="$1" want="$2" got="$3"
        if [ "$got" != "$want" ]; then
            echo "check_linker_script --self-test: $name gave [$got], wanted [$want]" >&2
            fail=1
            return
        fi
        echo "  parser $name: [$got]"
    }
    expect_parse "section_field addr" "ffffffff80001000" "$(section_field x .rodata addr)"
    expect_parse "section_field type" "NOBITS" "$(section_field x .bss type)"
    expect_parse "section_field size" "000008" "$(section_field x .rodata size)"
    expect_parse "section_field absent" "" "$(section_field x .nope addr)"
    expect_parse "section_order sorts by address" ".text .rodata .bss " \
        "$(section_order x .text .rodata .bss)"
    expect_parse "section_order drops the unasked" ".text .bss " \
        "$(section_order x .text .bss)"
    expect_parse "section_order drops address zero" ".text " \
        "$(section_order x .text .comment)"
    expect_parse "symbol_value" "ffffffff80001008" "$(symbol_value x probe_gap_end)"
    expect_parse "symbol_value absent" "" "$(symbol_value x nope)"
    expect_parse "entry_point pads" "0000000000401000" "$(entry_point x)"
    expect_parse "load_segment_flags" "RE R RW" "$(load_segment_flags x | tr '\n' ' ' | sed 's/ $//')"
    expect_parse "first_load_vaddr" "ffffffff80000000" "$(first_load_vaddr x)"
    cat > "$root/header.txt" <<'EOF'
ELF Header:
  Class:                             ELF64
  Data:                              2's complement, little endian
  Machine:                           Advanced Micro Devices X86-64
EOF
    printf '#!/bin/sh\ncat "%s"\n' "$root/header.txt" > "$root/readobj-header"
    chmod +x "$root/readobj-header"
    READOBJ="$root/readobj-header"
    expect_parse "elf_class_and_machine" "ELF64 X86-64" "$(elf_class_and_machine x)"
    READOBJ="$root/readobj"

    WORK="$root"
    printf 'wild: error: Symbols with the set location operation are not yet supported.\nwild: error: a second line the parser must not reach\n' > "$root/link.err"
    expect_parse "link_error takes the first" \
        "Symbols with the set location operation are not yet supported." "$(link_error)"
    printf 'error: an unprefixed diagnostic\n' > "$root/link.err"
    expect_parse "link_error unprefixed" "an unprefixed diagnostic" "$(link_error)"
    : > "$root/link.err"
    expect_parse "link_error silent" "" "$(link_error)"

    rm -rf "$root"
    trap - EXIT INT TERM
    if [ "$fail" -ne 0 ]; then
        echo "check_linker_script: SELF-TEST FAILED — the gate's comparisons are wrong" >&2
        exit 1
    fi
    echo "check_linker_script: self-test OK"
    exit 0
}

[ "$SELF_TEST" = "1" ] && self_test
main

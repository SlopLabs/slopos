#!/usr/bin/env bash
set -euo pipefail

# Hold the SlopOS clang driver to the link line the build writes by hand.
#
# Usage: check_clang_driver.sh [--require] [--self-test]
#
# `scripts/build_userland.sh` spells every SlopOS link out longhand — `crt0.o`
# first, `--image-base=0x400000`, `--dynamic-linker=/lib/ld-slopos.so.1`,
# `--eh-frame-hdr`, `-z now`, `-L<sysroot>/lib -lc`, and `libbuiltins.a` last
# because a 128-bit helper has no libgcc to come from. The driver puts
# `-lc++` ahead of `-lc` where that script writes them the other way round;
# the driver's order is the one the C++ runtime's own link line uses, and it
# is the order a libc++ that calls back into libc needs.
# `toolchains::SlopOS` in the port is the second copy of that
# knowledge and the one a self-hosted `cc` will use, and nothing compares the
# two: a port whose `Driver::getToolChain` hunk went missing still builds a
# clang, still links on the host, and emits an executable this kernel cannot
# run — no interpreter, no `crt0.o`, no `PT_GNU_EH_FRAME`, at a load address
# the loader does not map.
#
# The subject is `clangDriver` and `clangBasic`, because a `Driver` builds a
# compilation and hands out its jobs with no cc1 behind it: the argv graded
# here costs those two libraries and their LLVM dependencies rather than a
# clang binary and a backend.
#
# `skipped` without a materialised source tree, since a checkout that has
# extracted nothing still has a consistent pin; the CI step that materialises
# one passes `--require`. `--self-test` grades the skip and `--require` paths
# on every host and the rejection wherever there is a tree to plant one in.

SELF="check_clang_driver"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

die() {
    echo "$SELF: $*" >&2
    exit 1
}

REQUIRE=0
SELF_TEST=0
for arg in "$@"; do
    case "$arg" in
    --require) REQUIRE=1 ;;
    --self-test) SELF_TEST=1 ;;
    *) die "unknown argument: $arg" ;;
    esac
done

skip() {
    [ "$REQUIRE" -eq 0 ] || die "$1"
    echo "$SELF: skipped — $1"
    exit 0
}

PIN="$REPO_ROOT/toolchain/cxx/PIN"
LLVM_VERSION="$(sed -n 's/^llvm_version=\(.*\)$/\1/p' "$PIN" | head -n 1)"
SOURCE="${LLVM_SRC_DIR:-$REPO_ROOT/third_party/llvm-project-${LLVM_VERSION}.src}"
STAGE_DIR="${BUILD_DIR:-$REPO_ROOT/builddir}"
BUILD="$STAGE_DIR/gates/clang-driver"
SYSROOT="$BUILD/sysroot"
TARGET="x86_64-unknown-slopos"
TARGETS="clangDriver clangBasic"

CLANG=""
CLANGXX=""
SKIP_REASON=""

# Unlike `check_llvm_port.sh` this build is a *host* build, so it needs no
# cross runtime and no host `llvm-tblgen`: the tree's own tablegen is built
# first.
inputs_ready() {
    local tool tools
    if [ ! -d "$SOURCE/clang/lib/Driver" ]; then
        SKIP_REASON="no llvm sources — run scripts/make_slopos_llvm_src.sh"
        return 1
    fi

    tools="$("$SCRIPT_DIR/cxx_host_tools.sh")" ||
        die "no host LLVM toolchain — see scripts/cxx_host_tools.sh"
    eval "$tools"
    for tool in cmake ninja; do
        command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
    done
    return 0
}

# The probe: a `Driver` over the port's own toolchain, asked for the link jobs
# the four shapes of SlopOS binary produce, grading each one's argv against
# what `build_userland.sh` writes by hand. It is the gate — the shell around
# it only arranges for it to compile.
write_probe() {
    cat >"$BUILD/probe.cpp" <<'EOF'
#include "clang/Basic/Diagnostic.h"
#include "clang/Basic/DiagnosticIDs.h"
#include "clang/Basic/DiagnosticOptions.h"
#include "clang/Driver/Compilation.h"
#include "clang/Driver/Driver.h"
#include "clang/Driver/Job.h"
#include "clang/Driver/ToolChain.h"
#include "llvm/ADT/IntrusiveRefCntPtr.h"
#include "llvm/ADT/SmallString.h"
#include "llvm/ADT/StringRef.h"
#include "llvm/Option/ArgList.h"
#include "llvm/Support/Path.h"
#include "llvm/Support/VirtualFileSystem.h"
#include "llvm/Support/raw_ostream.h"

#include <iterator>
#include <memory>
#include <string>
#include <vector>

using namespace clang;
using namespace clang::driver;

namespace {

int Checks = 0;
int Failures = 0;

void check(bool Cond, const llvm::Twine &What) {
  llvm::outs() << (Cond ? "      ok   " : "      FAIL ") << What << "\n";
  ++Checks;
  if (!Cond)
    ++Failures;
}

class Reporter : public DiagnosticConsumer {
  void HandleDiagnostic(DiagnosticsEngine::Level Level,
                        const Diagnostic &Info) override {
    DiagnosticConsumer::HandleDiagnostic(Level, Info);
    llvm::SmallString<256> Message;
    Info.FormatDiagnostic(Message);
    llvm::outs() << "      driver: " << Message << "\n";
  }
};

int indexOf(const llvm::opt::ArgStringList &Argv, llvm::StringRef Needle) {
  for (int I = 0, E = (int)Argv.size(); I != E; ++I)
    if (Needle == Argv[I])
      return I;
  return -1;
}

int firstObject(const llvm::opt::ArgStringList &Argv) {
  for (int I = 0, E = (int)Argv.size(); I != E; ++I)
    if (llvm::StringRef(Argv[I]).ends_with(".o"))
      return I;
  return -1;
}

struct Shape {
  const char *Name;
  const char *Mode;   // "" | "-static" | "-shared"
  bool CXX;
  bool Crt0;
  bool DynamicLinker;
};

void grade(const Shape &S, const std::string &Triple,
           const std::string &SysRoot, const std::string &Object,
           const std::string &Output, bool Properties) {
  llvm::outs() << "    " << S.Name << ":\n";

  std::vector<std::string> Args = {"clang"};
  if (S.CXX)
    Args.push_back("--driver-mode=g++");
  Args.push_back("--target=" + Triple);
  Args.push_back("--sysroot=" + SysRoot);
  if (*S.Mode)
    Args.push_back(S.Mode);
  Args.push_back(Object);
  Args.push_back("-o");
  Args.push_back(Output);

  std::vector<const char *> Argv;
  for (const std::string &A : Args)
    Argv.push_back(A.c_str());

  llvm::IntrusiveRefCntPtr<DiagnosticOptions> DiagOpts(new DiagnosticOptions());
  Reporter Consumer;
  DiagnosticsEngine Diags(new DiagnosticIDs(), DiagOpts, &Consumer, false);
  Driver TheDriver("/usr/bin/clang", Triple, Diags);
  std::unique_ptr<Compilation> C(TheDriver.BuildCompilation(Argv));

  if (!C || C->getJobs().size() != 1) {
    check(false, "one link job");
    return;
  }

  const Command &Cmd = *C->getJobs().begin();
  const llvm::opt::ArgStringList &A = Cmd.getArguments();

  check(llvm::sys::path::filename(Cmd.getExecutable()) == "ld.lld",
        "linker is ld.lld (" + llvm::Twine(Cmd.getExecutable()) + ")");
  check(indexOf(A, "--eh-frame-hdr") >= 0, "--eh-frame-hdr");
  check(indexOf(A, "-L" + SysRoot + "/lib") >= 0, "-L<sysroot>/lib");
  check(indexOf(A, "-lc") >= 0, "-lc");
  int Builtins = indexOf(A, SysRoot + "/lib/libbuiltins.a");
  check(Builtins >= 0 && Builtins > indexOf(A, "-lc"),
        "<sysroot>/lib/libbuiltins.a after -lc");

  int Object0 = firstObject(A);
  bool IsCrt0 =
      Object0 >= 0 && llvm::sys::path::filename(A[Object0]) == "crt0.o";
  if (S.Crt0) {
    check(IsCrt0, "crt0.o is the first input");
    check(Object0 >= 0 && A[Object0] == SysRoot + "/lib/crt0.o",
          "crt0.o comes from <sysroot>/lib");
  } else {
    check(!IsCrt0, "no crt0.o");
  }

  bool HasDyld = indexOf(A, "--dynamic-linker=/lib/ld-slopos.so.1") >= 0;
  check(HasDyld == S.DynamicLinker,
        S.DynamicLinker ? "--dynamic-linker=/lib/ld-slopos.so.1"
                        : "no --dynamic-linker");

  if (*S.Mode)
    check(indexOf(A, S.Mode) >= 0, llvm::Twine(S.Mode));
  check((indexOf(A, "--image-base=0x400000") >= 0) == S.Crt0,
        S.Crt0 ? "--image-base=0x400000" : "no --image-base");

  int Now = indexOf(A, "now");
  check((Now > 0 && llvm::StringRef(A[Now - 1]) == "-z") == S.DynamicLinker,
        S.DynamicLinker ? "-z now" : "no -z now");

  if (S.CXX) {
    int Cxx = indexOf(A, "-lc++");
    check(Cxx >= 0 && Cxx < indexOf(A, "-lc"), "-lc++ before -lc");
  }

  if (!Properties)
    return;

  const ToolChain &TC = C->getDefaultToolChain();
  const llvm::opt::ArgList &DArgs = C->getArgs();
  check(llvm::StringRef(TC.getDefaultLinker()) == "ld.lld",
        "getDefaultLinker() is ld.lld");
  check(llvm::sys::path::filename(TC.GetLinkerPath()) == "ld.lld",
        "GetLinkerPath() is ld.lld");
  check(TC.HasNativeLLVMSupport(), "HasNativeLLVMSupport()");
  check(TC.IsIntegratedAssemblerDefault(), "IsIntegratedAssemblerDefault()");
  check(!TC.isPICDefault(), "!isPICDefault()");
  check(!TC.isPIEDefault(DArgs), "!isPIEDefault()");
  check(!TC.isPICDefaultForced(), "!isPICDefaultForced()");
  check(TC.getDefaultUnwindTableLevel(DArgs) ==
            ToolChain::UnwindTableLevel::Asynchronous,
        "unwind tables are asynchronous");
  check(TC.GetCXXStdlibType(DArgs) == ToolChain::CST_Libcxx, "libc++");
  check(TC.GetDefaultRuntimeLibType() == ToolChain::RLT_CompilerRT,
        "compiler-rt");
  check(TC.GetFilePath("crt0.o") == SysRoot + "/lib/crt0.o",
        "GetFilePath(\"crt0.o\") is <sysroot>/lib/crt0.o");

  llvm::opt::ArgStringList CC1Args;
  TC.AddClangCXXStdlibIncludeArgs(DArgs, CC1Args);
  TC.AddClangSystemIncludeArgs(DArgs, CC1Args);
  int Cxx1 = indexOf(CC1Args, SysRoot + "/include/c++/v1");
  int C1 = indexOf(CC1Args, SysRoot + "/include");
  check(Cxx1 >= 0, "<sysroot>/include/c++/v1 is on the C++ include path");
  check(C1 >= 0, "<sysroot>/include is on the C include path");
  // libc++'s `<cstdlib>` reaches the C header through `#include_next`, so the
  // C directory ahead of it stops the search one header early.
  check(Cxx1 >= 0 && C1 > Cxx1, "the C++ include path comes first");
}

} // namespace

int main(int argc, char **argv) {
  std::string Triple = "x86_64-unknown-slopos";
  std::string SysRoot, Object, Output;

  for (int I = 1; I < argc; ++I) {
    llvm::StringRef Arg = argv[I];
    if (Arg.consume_front("--triple="))
      Triple = Arg.str();
    else if (Arg.consume_front("--sysroot="))
      SysRoot = Arg.str();
    else if (Arg.consume_front("--object="))
      Object = Arg.str();
    else if (Arg.consume_front("--output="))
      Output = Arg.str();
    else {
      llvm::errs() << "probe: unknown argument: " << Arg << "\n";
      return 2;
    }
  }
  if (SysRoot.empty() || Object.empty() || Output.empty()) {
    llvm::errs() << "probe: --sysroot=, --object= and --output= are required\n";
    return 2;
  }

  static const Shape Shapes[] = {
      {"static executable", "-static", false, true, false},
      {"dynamic executable", "", false, true, true},
      {"shared object", "-shared", false, false, false},
      {"C++ executable", "", true, true, true},
  };
  for (const Shape &S : Shapes)
    grade(S, Triple, SysRoot, Object, Output, &S == &Shapes[1]);

  llvm::outs() << (Failures ? "rejected " : "accepted ") << Triple << ": "
               << Checks << " assertions over " << std::size(Shapes)
               << " link shapes, " << Failures << " failed\n";
  return Failures ? 1 : 0;
}
EOF
}

# The tree fresh, cmake run, the two libraries built and the probe compiled
# against them. Split out because the self-test drives the same probe twice,
# once for the port's own triple and once for one it does not name.
prepare() {
    local want_src

    want_src="$("$SCRIPT_DIR/make_slopos_llvm_src.sh" --print-stamp)"
    [ -n "$want_src" ] || die "make_slopos_llvm_src.sh --print-stamp printed nothing"
    [ "$(cat "$SOURCE/.slopos-llvm-stamp" 2>/dev/null)" = "$want_src" ] ||
        die "the llvm source tree is stale — run scripts/make_slopos_llvm_src.sh"

    # cmake refuses an existing binary directory whose source directory moved,
    # so the version is the key rather than something to diagnose later.
    if [ "$(cat "$BUILD/.slopos-source" 2>/dev/null)" != "$SOURCE" ]; then
        rm -rf "$BUILD"
    fi
    mkdir -p "$BUILD"
    printf '%s\n' "$SOURCE" >"$BUILD/.slopos-source"

    # `-include cstdint`: LLVM 18's `SmallVector.h` names `uint64_t` without
    # including it and got away with it on the standard libraries of 2024. It
    # is a host-toolchain workaround and deliberately not a patch — the pin
    # describes an upstream release, and a hunk here would be one more thing
    # `git apply` has to keep matching for a problem that lives on this
    # machine rather than in the port.
    cmake -G Ninja -S "$SOURCE/llvm" -B "$BUILD" -Wno-dev \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_C_COMPILER="$CLANG" -DCMAKE_CXX_COMPILER="$CLANGXX" \
        -DCMAKE_CXX_FLAGS="-include cstdint" \
        -DLLVM_ENABLE_PROJECTS=clang \
        -DLLVM_TARGETS_TO_BUILD=X86 \
        -DLLVM_INCLUDE_TESTS=OFF -DLLVM_INCLUDE_BENCHMARKS=OFF \
        -DLLVM_INCLUDE_EXAMPLES=OFF -DLLVM_INCLUDE_DOCS=OFF \
        -DCLANG_INCLUDE_TESTS=OFF -DCLANG_INCLUDE_DOCS=OFF \
        -DLLVM_ENABLE_ZLIB=OFF -DLLVM_ENABLE_ZSTD=OFF \
        -DLLVM_ENABLE_TERMINFO=OFF -DLLVM_ENABLE_LIBXML2=OFF \
        -DLLVM_ENABLE_LIBEDIT=OFF -DLLVM_ENABLE_LIBPFM=OFF \
        >"$BUILD/configure.log" 2>&1 || {
        tail -n 30 "$BUILD/configure.log" >&2
        die "cmake configure failed; see $BUILD/configure.log"
    }

    ninja -C "$BUILD" $TARGETS >"$BUILD/build.log" 2>&1 || {
        grep -h 'error:' "$BUILD/build.log" | sed 's/.*error: /  /' |
            sort -u | head -n 20 >&2
        die "$TARGETS does not build; see $BUILD/build.log"
    }

    # The sysroot the probe points at. `crt0.o` and `libbuiltins.a` have to
    # exist for `GetFilePath` to resolve them to a path rather than hand back
    # the bare name, and the input object has to exist or the driver
    # diagnoses it and builds no job at all; none is ever opened.
    mkdir -p "$SYSROOT/lib" "$SYSROOT/include/c++/v1"
    : >"$SYSROOT/lib/crt0.o"
    : >"$SYSROOT/lib/libbuiltins.a"
    : >"$BUILD/probe-input.o"

    write_probe
    # `-fno-rtti` because LLVM is: a probe with RTTI on needs a `typeinfo` for
    # `DiagnosticConsumer` that no LLVM archive defines. The archives go in a
    # group rather than in dependency order — they are cyclic, and naming the
    # order here would be a third copy of LLVM's own link graph.
    "$CLANGXX" -std=c++17 -fno-rtti -fno-exceptions -include cstdint -O1 \
        -I "$BUILD/tools/clang/include" -I "$SOURCE/clang/include" \
        -I "$BUILD/include" -I "$SOURCE/llvm/include" \
        "$BUILD/probe.cpp" -o "$BUILD/probe" \
        -Wl,--start-group "$BUILD"/lib/*.a -Wl,--end-group \
        -lpthread -ldl -lm >"$BUILD/probe.log" 2>&1 || {
        tail -n 20 "$BUILD/probe.log" >&2
        die "the driver probe does not build; see $BUILD/probe.log"
    }
}

# Quiet unless it rejects: the probe's per-assertion lines are the evidence a
# failure needs and noise a green run does not.
probe() {
    "$BUILD/probe" "--triple=$1" "--sysroot=$SYSROOT" \
        "--object=$BUILD/probe-input.o" "--output=$BUILD/probe-output" \
        >"$2" 2>&1
}

run_gate() {
    inputs_ready || skip "$SKIP_REASON"
    prepare
    probe "$TARGET" "$BUILD/probe-run.log" || {
        sed -n 's/^ *FAIL /  /p' "$BUILD/probe-run.log" >&2
        die "the $TARGET driver does not produce the link line
       scripts/build_userland.sh writes by hand; see $BUILD/probe-run.log"
    }
    echo "$SELF: $(sed -n 's/^accepted //p' "$BUILD/probe-run.log") — ld.lld, crt0.o, /lib/ld-slopos.so.1, -lc"
}

self_test() {
    # `scratch` is not `local`: a `die` in `prepare` pops it before the EXIT
    # trap runs, and `set -u` then reports that instead of the real message.
    local failed=0
    scratch="$(mktemp -d)"
    trap 'rm -rf "$scratch"' EXIT INT TERM

    if LLVM_SRC_DIR="$scratch/absent" "$SCRIPT_DIR/$SELF.sh" >/dev/null 2>&1; then
        echo "  case no-source-tree: skipped rather than failed"
    else
        echo "$SELF --self-test: a checkout with no source tree was failed" >&2
        failed=1
    fi

    if LLVM_SRC_DIR="$scratch/absent" "$SCRIPT_DIR/$SELF.sh" --require \
        >/dev/null 2>&1; then
        echo "$SELF --self-test: --require passed with no source tree" >&2
        failed=1
    else
        echo "  case require-no-source-tree: failed rather than skipped"
    fi

    if inputs_ready; then
        prepare
        # The positive control first: without it a probe that fails for an
        # unrelated reason reads as a working check.
        if probe "$TARGET" "$scratch/ported.log"; then
            echo "  case ported-driver: $TARGET links the way build_userland.sh does"
        else
            sed 's/^/    /' "$scratch/ported.log" >&2
            echo "$SELF --self-test: the ported driver does not produce the expected link line" >&2
            failed=1
        fi

        # The planted violation, with the same assertions over it:
        # `x86_64-unknown-none` is a triple this tree does not name, so
        # `Driver::getToolChain` falls through to its `default:` arm and
        # builds a `Generic_ELF` — the same toolchain a `Driver.cpp` without
        # the port's `case` builds for SlopOS itself.
        if probe "x86_64-unknown-none" "$scratch/unported.log"; then
            echo "$SELF --self-test: a toolchain the port does not name produced an accepted link line" >&2
            failed=1
        else
            echo "  case unported-triple: rejected the link line of a toolchain the port does not name"
        fi
    else
        echo "  cases ported-driver, unported-triple: skipped — $SKIP_REASON"
    fi

    rm -rf "$scratch"
    trap - EXIT INT TERM
    if [ "$failed" -ne 0 ]; then
        echo "$SELF: SELF-TEST FAILED — the gate does not catch what it claims to" >&2
        return 1
    fi
    echo "$SELF: self-test OK"
}

if [ "$SELF_TEST" -eq 1 ]; then
    self_test
else
    run_gate
fi

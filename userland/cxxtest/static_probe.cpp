// `/bin/cxx_static_probe` — the same C++ runtime, linked as archives.
//
// `cxx_probe` proves the dynamic case; this one is the static one, and it
// exists because the two take different roads to the same DWARF. A dynamic
// program's unwinder finds an object's `PT_GNU_EH_FRAME` through the loader's
// table, which is populated because something was loaded; a static program is
// not in that table at all and is found through `AT_PHDR` instead. Linking
// `libc++.a` beside `libc.a` is also the only thing that exercises either
// archive, and `libc.a` carries the unwinder whose own frames every unwind
// starts in.
//
// `cxx_test` grades the exit status — 0 for every check, otherwise the number
// of the one that failed — and looks for the static destructor's mark on
// stdout, which is written after `main` has returned and so cannot be one.

#include <stdio.h>

#include <exception>
#include <memory>
#include <string>
#include <vector>

namespace {

int check = 0;

bool fail(const char *what) {
    fprintf(stderr, "cxx_static_probe: check %d: %s\n", check, what);
    return false;
}

struct StaticProbeError {
    int code;
};

struct UnknownToTheHandler {
    int filler;
};

struct WitnessOnUnwind {
    int *witness;
    int code;
    ~WitnessOnUnwind() { *witness = code; }
};

int static_ctor_witness = 0;

const char *const kDestructorMark = "static-dtor-ran";

struct StaticObject {
    StaticObject() { static_ctor_witness = 0x5105; }
    ~StaticObject() { printf("%s\n", kDestructorMark); }
};

StaticObject the_static_object;

bool run() {
    check = 1;
    if (static_ctor_witness != 0x5105) {
        return fail("the static constructor did not run before main");
    }

    check = 2;
    int witness = 0;
    try {
        WitnessOnUnwind unwind_witness{&witness, 0x5678};
        throw StaticProbeError{0x1234};
    } catch (const StaticProbeError &error) {
        if (error.code != 0x1234) {
            return fail("the exception arrived with the wrong code");
        }
    } catch (...) {
        return fail("the exception was caught by the wrong handler");
    }
    if (witness != 0x5678) {
        return fail("a destructor in the unwound frame did not run");
    }

    check = 3;
    try {
        throw UnknownToTheHandler{7};
    } catch (const StaticProbeError &) {
        return fail("a foreign type matched StaticProbeError");
    } catch (...) {
    }

    check = 4;
    try {
        auto held = std::make_unique<std::vector<std::string>>(4, "held");
        throw StaticProbeError{static_cast<int>(held->size())};
    } catch (const StaticProbeError &error) {
        if (error.code != 4) {
            return fail("the exception arrived with the wrong code");
        }
    }

    check = 5;
    std::exception_ptr captured;
    try {
        throw StaticProbeError{0x99};
    } catch (...) {
        captured = std::current_exception();
    }
    try {
        std::rethrow_exception(captured);
        return fail("a rethrown exception did not propagate");
    } catch (const StaticProbeError &error) {
        if (error.code != 0x99) {
            return fail("a rethrown exception lost its payload");
        }
    }

    return true;
}

} // namespace

int main() {
    return run() ? 0 : check;
}

// `/bin/cxx_probe` — the dynamically linked one of the tree's two cross-built
// C++ programs; `cxx_static_probe` is the other.
//
// It reaches the C library through `libc.so` and the C++ library through
// `libc++.so`, which is the shape every cross-built C++ program has. `cxx_test`
// spawns it and grades its exit status: 0 means every check below passed, and
// any other value is the number of the one that did not.
//
// Run as `cxx_probe terminate` it throws with no handler anywhere, which must
// reach `std::terminate` and die by `SIGABRT`.

#include "cxxtest.h"

#include <dlfcn.h>
#include <stdio.h>
#include <string.h>

#include <exception>
#include <fstream>
#include <iomanip>
#include <locale>
#include <memory>
#include <sstream>
#include <string>
#include <vector>

CxxTestError::~CxxTestError() = default;

const char *CxxTestError::what() const noexcept {
    return "CxxTestError";
}

namespace {

int destruction_note = 0;
int unwind_on_close_note = 0;

const char *const kLibrary = "/lib/libcxxtest.so";

int check = 0;

bool fail(const char *what) {
    fprintf(stderr, "cxx_probe: check %d: %s\n", check, what);
    return false;
}

// The half of the runtime that needs a locale. LLVM reaches all of it:
// `raw_os_ostream.cpp` writes through `std::ostream`, `ARMBuildAttrs.cpp`
// formats through `<iomanip>` and `<sstream>`, fourteen clang translation
// units read through `<fstream>`, and `ConvertUTF.cpp` converts to
// `std::wstring`.
bool localized_runtime_works() {
    check = 13;
    std::ostringstream out;
    out << std::hex << std::setw(6) << std::setfill('0') << 0xbeef;
    if (out.str() != "00beef") {
        return fail("std::ostringstream formatted an integer wrongly");
    }

    check = 14;
    int parsed = 0;
    std::istringstream in("1234 rest");
    in >> parsed;
    if (parsed != 1234) {
        return fail("std::istringstream parsed an integer wrongly");
    }

    check = 15;
    const std::locale classic("C");
    const auto &ctype = std::use_facet<std::ctype<char>>(classic);
    if (!ctype.is(std::ctype_base::digit, '7') || ctype.is(std::ctype_base::digit, 'q')) {
        return fail("std::ctype<char> classifies wrongly");
    }

    check = 16;
    if (std::to_wstring(-4210) != L"-4210") {
        return fail("std::to_wstring formatted wrongly");
    }

    check = 17;
    std::ifstream self("/bin/cxx_probe", std::ios::binary);
    char magic[4] = {};
    if (!self.read(magic, sizeof magic) || magic[0] != 0x7f || magic[1] != 'E') {
        return fail("std::ifstream did not read this binary's own header");
    }

    return true;
}

bool exception_is_caught_in_its_own_object() {
    try {
        throw CxxTestError(11);
    } catch (const CxxTestError &error) {
        return error.code() == 11 || fail("wrong code from a local throw");
    } catch (...) {
        return fail("a local throw was caught by the wrong handler");
    }
}

using ThrowFn = void (*)(int);
using ThrowRaiiFn = void (*)(int, int *);
using VoidFn = void (*)(void);
using AddFn = int (*)(int, int);
using StringLengthFn = int (*)(const char *);

struct Library {
    void *handle = nullptr;

    ~Library() { close(); }

    void close() {
        if (handle != nullptr) {
            dlclose(handle);
            handle = nullptr;
        }
    }

    template <typename Fn> Fn symbol(const char *name) {
        return reinterpret_cast<Fn>(dlsym(handle, name));
    }
};

bool run(Library &library) {
    check = 2;
    library.handle = dlopen(kLibrary, RTLD_NOW);
    if (library.handle == nullptr) {
        const char *reason = dlerror();
        return fail(reason != nullptr ? reason : "dlopen failed");
    }

    check = 3;
    auto add = library.symbol<AddFn>("cxxtest_add");
    if (add == nullptr || add(40, 2) != 42) {
        return fail("the loaded object's std::vector does not work");
    }

    check = 4;
    auto ctor_ran = library.symbol<int (*)(void)>("cxxtest_ctor_ran");
    if (ctor_ran == nullptr || ctor_ran() != 0x5105) {
        return fail("the loaded object's static constructor did not run");
    }

    check = 5;
    auto string_length = library.symbol<StringLengthFn>("cxxtest_string_length");
    if (string_length == nullptr || string_length("workbench") != 9) {
        return fail("the loaded object's std::string does not work");
    }

    // The property this whole binary exists for.
    check = 6;
    auto throw_error = library.symbol<ThrowFn>("cxxtest_throw");
    if (throw_error == nullptr) {
        return fail("cxxtest_throw is missing");
    }
    try {
        throw_error(0x1234);
        return fail("cxxtest_throw returned instead of throwing");
    } catch (const CxxTestError &error) {
        if (error.code() != 0x1234) {
            return fail("the exception arrived with the wrong code");
        }
        if (strcmp(error.what(), "CxxTestError") != 0) {
            return fail("the exception arrived with the wrong vtable");
        }
    } catch (...) {
        return fail("the exception was caught by the wrong handler");
    }

    check = 7;
    auto throw_through_raii = library.symbol<ThrowRaiiFn>("cxxtest_throw_through_raii");
    if (throw_through_raii == nullptr) {
        return fail("cxxtest_throw_through_raii is missing");
    }
    int witness = 0;
    try {
        throw_through_raii(0x5678, &witness);
        return fail("cxxtest_throw_through_raii returned instead of throwing");
    } catch (const CxxTestError &error) {
        if (error.code() != 0x5678) {
            return fail("the exception arrived with the wrong code");
        }
    }
    if (witness != 0x5678) {
        return fail("a destructor in the unwound frame did not run");
    }

    check = 8;
    auto throw_unknown = library.symbol<VoidFn>("cxxtest_throw_unknown");
    if (throw_unknown == nullptr) {
        return fail("cxxtest_throw_unknown is missing");
    }
    try {
        throw_unknown();
        return fail("cxxtest_throw_unknown returned instead of throwing");
    } catch (const CxxTestError &) {
        return fail("a foreign type matched CxxTestError");
    } catch (...) {
    }

    // The frame being unwound holds a heap object, which is what a runtime
    // does and a `longjmp` does not.
    check = 9;
    try {
        auto held = std::make_unique<std::vector<std::string>>(4, "held");
        throw_error(static_cast<int>(held->size()));
        return fail("cxxtest_throw returned instead of throwing");
    } catch (const CxxTestError &error) {
        if (error.code() != 4) {
            return fail("the exception arrived with the wrong code");
        }
    }

    check = 10;
    std::exception_ptr captured;
    try {
        throw_error(0x99);
    } catch (...) {
        captured = std::current_exception();
    }
    try {
        std::rethrow_exception(captured);
        return fail("a rethrown exception did not propagate");
    } catch (const CxxTestError &error) {
        if (error.code() != 0x99) {
            return fail("a rethrown exception lost its payload");
        }
    }

    // `__cxa_atexit`/`__cxa_finalize` are the half of the Itanium ABI the C
    // library owns rather than the C++ one.
    check = 11;
    library.close();
    if (destruction_note != 0x600d) {
        return fail("the loaded object's static destructor did not run on dlclose");
    }

    check = 12;
    if (unwind_on_close_note != 0x7717) {
        return fail("a throw from a static destructor did not unwind during dlclose");
    }

    return localized_runtime_works();
}

} // namespace

extern "C" void cxx_probe_note_destruction(int code) {
    destruction_note = code;
}

extern "C" void cxx_probe_note_unwind_on_close(int code) {
    unwind_on_close_note = code;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "terminate") == 0) {
        Library library;
        library.handle = dlopen(kLibrary, RTLD_NOW);
        if (library.handle == nullptr) {
            return 1;
        }
        library.symbol<ThrowFn>("cxxtest_throw")(1);
        return 1;
    }

    check = 1;
    if (!exception_is_caught_in_its_own_object()) {
        return check;
    }

    Library library;
    return run(library) ? 0 : check;
}

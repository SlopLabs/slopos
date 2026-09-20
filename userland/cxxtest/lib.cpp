// `libcxxtest.so` — the shared object `cxx_probe` loads and throws out of.

#include "cxxtest.h"

#include <string>
#include <vector>

namespace {

int ctor_witness = 0;

struct StaticObject {
    StaticObject() { ctor_witness = 0x5105; }
    ~StaticObject() { cxx_probe_note_destruction(0x600d); }
};

StaticObject the_static_object;

struct ThrowsWhileUnloading {
    // A destructor that lets an exception escape calls `std::terminate`, so
    // this one catches its own.
    ~ThrowsWhileUnloading() {
        try {
            throw CxxTestError(0x7717);
        } catch (const CxxTestError &error) {
            cxx_probe_note_unwind_on_close(error.code());
        }
    }
};

ThrowsWhileUnloading the_throwing_object;

struct WitnessOnUnwind {
    int *witness;
    int code;
    ~WitnessOnUnwind() { *witness = code; }
};

struct UnknownToTheProbe {
    int filler;
};

} // namespace

extern "C" int cxxtest_add(int a, int b) {
    std::vector<int> values{a, b};
    return values[0] + values[1];
}

extern "C" int cxxtest_ctor_ran(void) {
    return ctor_witness;
}

extern "C" int cxxtest_string_length(const char *text) {
    return static_cast<int>(std::string(text).size());
}

extern "C" void cxxtest_throw(int code) {
    throw CxxTestError(code);
}

extern "C" void cxxtest_throw_through_raii(int code, int *witness) {
    WitnessOnUnwind unwind_witness{witness, code};
    throw CxxTestError(code);
}

extern "C" void cxxtest_throw_unknown(void) {
    throw UnknownToTheProbe{7};
}

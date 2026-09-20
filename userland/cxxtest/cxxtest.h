// The type an exception crosses the `dlopen` boundary as, and the entry
// points `libcxxtest.so` exports.
//
// `CxxTestError`'s key function is its destructor, which `cxx_probe` defines
// and `--export-dynamic` publishes: that puts the class's `type_info` and
// vtable in the executable, which is the only object both sides can name. The
// shared object is loaded by `dlopen` rather than by `DT_NEEDED`, so a
// `type_info` living there would be one the executable could not have linked
// against.

#ifndef SLOPOS_CXXTEST_H
#define SLOPOS_CXXTEST_H

#include <exception>

class CxxTestError : public std::exception {
public:
    explicit CxxTestError(int code) noexcept : code_(code) {}
    ~CxxTestError() override;
    const char *what() const noexcept override;
    int code() const noexcept { return code_; }

private:
    int code_;
};

extern "C" {

int cxxtest_add(int a, int b);
int cxxtest_ctor_ran(void);

// Defined by `cxx_probe`, which links `--export-dynamic` for it, and called
// from a static object's destructor in the loaded object: after `dlclose` the
// object is unmapped, so the only witness that can outlive it is one the
// executable owns.
void cxx_probe_note_destruction(int code);

// The same, from a static destructor that throws and catches within itself
// while the object is being unloaded. A throw needs the unwinder to find the
// FDEs of an object `dlclose` has already marked dying, so this is the witness
// that it still can.
void cxx_probe_note_unwind_on_close(int code);
int cxxtest_string_length(const char *text);

// Throws `CxxTestError(code)` from a frame inside this object.
void cxxtest_throw(int code);

// Throws through a frame holding an object whose destructor writes `code` to
// `witness`, so a caller that catches can tell phase-2 cleanup ran.
void cxxtest_throw_through_raii(int code, int *witness);

// Throws a type the executable has never seen, for `catch (...)`.
void cxxtest_throw_unknown(void);
}

#endif

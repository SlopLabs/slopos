// `/bin/libc_probe` — the C library's own surface, exercised from C.
//
// Reachable only from C: a `long double` has no Rust spelling, `setjmp`
// returns twice, and nothing else compiles the generated headers as C. A
// header that disagrees with its export fails here at compile time.
// `libc_abi_test` grades the exit status: 0, or the number of the check
// that failed.

#include <errno.h>
#include <langinfo.h>
#include <limits.h>
#include <locale.h>
#include <math.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <wchar.h>

static int check = 0;

static int fail(const char *what) {
    fprintf(stderr, "libc_probe: check %d: %s\n", check, what);
    return 0;
}

// The x87 significand and biased exponent, which is the only way to say
// "wider than a double" about a value.
static void ld_bits(long double x, uint64_t *sig, uint16_t *exp) {
    unsigned char raw[sizeof(long double)];
    memcpy(raw, &x, sizeof raw);
    memcpy(sig, raw, 8);
    memcpy(exp, raw + 8, 2);
}

// A `long double` whose value clang cannot see: handed a literal it answers
// most of the calls below from its own builtin table.
static long double opaque(long double x) {
    volatile long double held = x;
    return held;
}

// `fabsl` clears a bit, so clang does it inline whatever the argument is.
// Only an indirect call it cannot fold reaches the library's own.
static long double (*volatile library_fabsl)(long double) = fabsl;

static jmp_buf jump;
static sigjmp_buf sig_jump;

// Read once before the jump and never after, so a compiler that thought the
// `setjmp` below returned only once could not recover the value by reloading.
static volatile int jump_seed = 11;

// 2024-02-29T12:34:56Z, a leap day so the month arithmetic has to be real.
static const time_t leap_day = 1709210096;

static int compare_int(const void *a, const void *b) {
    int x = *(const int *)a;
    int y = *(const int *)b;
    return (x > y) - (x < y);
}

static int compare_triple(const void *a, const void *b) {
    return memcmp(a, b, 3);
}

typedef struct {
    uint64_t key;
    uint64_t tag;
} tagged;

static int compare_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a;
    uint64_t y = *(const uint64_t *)b;
    return (x > y) - (x < y);
}

static int compare_tagged(const void *a, const void *b) {
    uint64_t x = ((const tagged *)a)->key;
    uint64_t y = ((const tagged *)b)->key;
    return (x > y) - (x < y);
}

// "Less" whichever way round it is asked, which no ordering allows: every
// partition sheds its pivot alone until the depth counter falls to heapsort.
static int compare_lying(const void *a, const void *b) {
    (void)a;
    (void)b;
    return -1;
}

static uint32_t noise(uint32_t *state) {
    *state = *state * 1664525u + 1013904223u;
    return *state >> 8;
}

static void *strerror_in_thread(void *arg) {
    return strerror(*(int *)arg);
}

static int jumps(void) {
    volatile int landed = setjmp(jump);
    if (landed == 0) {
        longjmp(jump, 7);
    }
    if (landed != 7) {
        return fail("longjmp did not carry its value");
    }

    landed = setjmp(jump);
    if (landed == 0) {
        longjmp(jump, 0);
    }
    if (landed != 1) {
        return fail("longjmp(env, 0) must arrive as 1");
    }

    // C11 7.13.2.1 p3: an automatic object unchanged between the two arrivals
    // keeps its value, `volatile` or not. At -O2 only the `returns_twice` the
    // compiler attaches to the call site makes that hold.
    int carried = jump_seed * 3;
    int again = setjmp(jump);
    if (again == 0) {
        jump_seed = 0;
        longjmp(jump, 5);
    }
    if (again != 5 || carried != 33) {
        return fail("a non-volatile local did not survive the second arrival");
    }
    return 1;
}

static int mask_jumps(void) {
    sigset_t blocked;
    sigset_t after;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGUSR1);
    sigprocmask(SIG_SETMASK, &blocked, NULL);

    if (sigsetjmp(sig_jump, 1) == 0) {
        sigset_t none;
        sigemptyset(&none);
        sigprocmask(SIG_SETMASK, &none, NULL);
        siglongjmp(sig_jump, 3);
        return fail("siglongjmp returned");
    }

    sigprocmask(SIG_SETMASK, NULL, &after);
    if (!sigismember(&after, SIGUSR1)) {
        return fail("siglongjmp did not restore the saved mask");
    }

    // `setjmp` saves no mask, so `siglongjmp` must be told there is none: a
    // 0xff-filled automatic buffer is residue claiming one was saved.
    jmp_buf dirty;
    memset(dirty, 0xff, sizeof dirty);
    volatile int landed = setjmp(dirty);
    if (landed == 0) {
        sigset_t other;
        sigemptyset(&other);
        sigaddset(&other, SIGUSR2);
        sigprocmask(SIG_SETMASK, &other, NULL);
        siglongjmp(dirty, 4);
        return fail("siglongjmp returned");
    }
    if (landed != 4) {
        return fail("siglongjmp did not carry its value");
    }
    sigprocmask(SIG_SETMASK, NULL, &after);
    if (!sigismember(&after, SIGUSR2) || sigismember(&after, SIGUSR1)) {
        return fail("siglongjmp installed a mask setjmp never saved");
    }

    sigemptyset(&blocked);
    sigprocmask(SIG_SETMASK, &blocked, NULL);
    return 1;
}

static int calendar(void) {
    struct tm parts;
    if (gmtime_r(&leap_day, &parts) != &parts) {
        return fail("gmtime_r did not answer its own argument");
    }
    if (parts.tm_year != 124 || parts.tm_mon != 1 || parts.tm_mday != 29 ||
        parts.tm_hour != 12 || parts.tm_min != 34 || parts.tm_sec != 56 ||
        parts.tm_wday != 4 || parts.tm_yday != 59) {
        return fail("gmtime_r decoded the wrong civil date");
    }
    if (parts.tm_isdst != 0 || parts.tm_gmtoff != 0 ||
        strcmp(parts.tm_zone, "UTC") != 0) {
        return fail("gmtime_r did not report UTC");
    }
    if (mktime(&parts) != leap_day || timegm(&parts) != leap_day) {
        return fail("mktime did not invert gmtime_r");
    }

    // Normalisation, not validation: hour 25 is tomorrow 01:00.
    struct tm rolls = parts;
    rolls.tm_hour = 25;
    rolls.tm_min = 0;
    rolls.tm_sec = 0;
    if (mktime(&rolls) != 1709254800 || rolls.tm_mday != 1 ||
        rolls.tm_mon != 2 || rolls.tm_hour != 1) {
        return fail("mktime did not normalise an out-of-range hour");
    }
    return 1;
}

static int formatting(void) {
    struct tm parts;
    gmtime_r(&leap_day, &parts);

    // 29 characters, so 30 bytes is the exact fit and 29 is one short.
    char out[64];
    const char *fmt = "%Y-%m-%d %H:%M:%S %z %Z";
    if (strftime(out, sizeof out, fmt, &parts) != 29 ||
        strcmp(out, "2024-02-29 12:34:56 +0000 UTC") != 0) {
        return fail("strftime rendered the wrong bytes");
    }
    char tight[30];
    if (strftime(tight, sizeof tight, fmt, &parts) != 29) {
        return fail("strftime refused a buffer that exactly fits");
    }
    char shy[29];
    if (strftime(shy, sizeof shy, fmt, &parts) != 0) {
        return fail("strftime accepted a buffer one byte short");
    }

    // `tm_zone` is a BSD extension, not one of C17 7.27.1's nine members, so
    // a format with no `%Z` must never dereference it.
    struct tm unzoned = parts;
    unzoned.tm_zone = (const char *)1;
    char plain[16];
    if (strftime(plain, sizeof plain, "%Y-%m-%d", &unzoned) != 10 ||
        strcmp(plain, "2024-02-29") != 0) {
        return fail("strftime did not render a zone-free format");
    }
    if (strcmp(ctime(&leap_day), "Thu Feb 29 12:34:56 2024\n") != 0) {
        return fail("ctime wrote the wrong bytes into its shared buffer");
    }
    return 1;
}

// The header spells `CLOCKS_PER_SEC` as a `raw` macro, which the generator's
// ABI check never reads.
static int cpu_clock(void) {
    if (CLOCKS_PER_SEC != 1000000) {
        return fail("CLOCKS_PER_SEC is not the POSIX million");
    }
    clock_t start = clock();
    if (start < 0) {
        return fail("clock could not read the process CPU clock");
    }
    volatile unsigned long spun = 0;
    for (unsigned long i = 0; i < 50000000UL && clock() == start; i++) {
        spun = spun + 1;
    }
    if (clock() <= start) {
        return fail("clock did not advance across a spin");
    }
    return 1;
}

// `printf`'s float conversions. `%Lf` is the only one whose operand is a
// 16-byte stack slot rather than an SSE register, so it is the one that can
// leave the rest of a format string reading the wrong place.
static int float_formatting(void) {
    char out[64];

    if (snprintf(out, sizeof out, "%f|%e|%g|%a", 1.5, 1.5, 1.5, 1.5) != 34 ||
        strcmp(out, "1.500000|1.500000e+00|1.5|0x1.8p+0") != 0) {
        return fail("snprintf rendered the wrong float bytes");
    }
    if (snprintf(out, sizeof out, "%.3f|%.2e|%.4g|%.2a", 0.1, 0.1, 0.1, 0.1) != 28 ||
        strcmp(out, "0.100|1.00e-01|0.1|0x1.9ap-4") != 0) {
        return fail("a requested precision did not round the way C asks");
    }
    if (snprintf(out, sizeof out, "%08.2f|%-10.3e|%+g|%#.0f", -1.5, 1.0 / 3.0, 2.0, 7.0) != 25 ||
        strcmp(out, "-0001.50|3.333e-01 |+2|7.") != 0) {
        return fail("a float width or flag was not applied");
    }
    if (snprintf(out, sizeof out, "%08f %08.2E %5.1g", INFINITY, -NAN, INFINITY) != 23 ||
        strcmp(out, "     inf     -NAN   inf") != 0) {
        return fail("the zero flag padded an infinity or a NaN");
    }

    // `volatile`, so clang cannot fold the size and diagnose the truncation
    // that is the point of the check.
    char shy[5];
    static volatile size_t room = sizeof shy;
    if (snprintf(shy, room, "%.3f", 3.14159) != 5 || strcmp(shy, "3.14") != 0) {
        return fail("snprintf did not report the length it would have written");
    }

    if (snprintf(out, sizeof out, "%f %d %s", 2.5, 42, "tail") != 16 ||
        strcmp(out, "2.500000 42 tail") != 0) {
        return fail("a conversion after a float read the wrong argument");
    }
    if (snprintf(out, sizeof out, "%d %Lf %d", 1, opaque(2.5L), 3) != 12 ||
        strcmp(out, "1 2.500000 3") != 0) {
        return fail("a long double conversion missed its stack operand");
    }
    if (snprintf(out, sizeof out, "%Lf %Lf %d %f", opaque(1.5L), opaque(-2.25L), 7, 0.5) != 29 ||
        strcmp(out, "1.500000 -2.250000 7 0.500000") != 0) {
        return fail("a second long double did not step the overflow area");
    }
    return 1;
}

// `scanf`'s float family, one directive in C: `a e f g` all read a `strtod`
// subject sequence. `sscanf` hands the input straight to the parser while
// `fscanf` finds the sequence a byte at a time with one byte of push-back.
static int float_scanning(void) {
    const char *subjects = "2.5 0x1.8p1 -3.25e2";
    double wide = 0.0;
    float narrow = 0.0f;
    long double widest = 0.0L;
    if (sscanf(subjects, "%lf %f %Lf", &wide, &narrow, &widest) != 3 || wide != 2.5 ||
        narrow != 3.0f || widest != -325.0L) {
        return fail("sscanf did not read the float family back");
    }

    FILE *scratch = fopen("/tmp/libc_probe_floats", "w+");
    if (scratch == NULL) {
        return fail("could not open a scratch stream");
    }
    wide = 0.0;
    narrow = 0.0f;
    widest = 0.0L;
    double unbounded = 0.0;
    if (fprintf(scratch, "%s inf 7x", subjects) != 26 || fseek(scratch, 0, SEEK_SET) != 0 ||
        fscanf(scratch, "%lf %f %Lf %lf", &wide, &narrow, &widest, &unbounded) != 4 ||
        wide != 2.5 || narrow != 3.0f || widest != -325.0L || !isinf(unbounded)) {
        fclose(scratch);
        return fail("fscanf did not read the float family back");
    }

    // `7x` is a number and then a byte no number can use, so the byte the
    // scan read past the subject sequence has to be back on the stream.
    char past = 0;
    if (fscanf(scratch, "%lf%c", &wide, &past) != 2 || wide != 7.0 || past != 'x') {
        fclose(scratch);
        return fail("fscanf swallowed the byte past a subject sequence");
    }
    fclose(scratch);

    // C17 7.21.6.2 p9: a directive fails unless its input item completes a
    // matching sequence. `1ex`, `0x` and `infi` each have a shorter prefix
    // that converts, which is `strtod`'s answer and not `scanf`'s.
    double partial = 42.0;
    if (sscanf("1ex", "%lf", &partial) != 0 || sscanf("0x", "%lf", &partial) != 0 ||
        sscanf("infi", "%lf", &partial) != 0 || partial != 42.0) {
        return fail("sscanf converted a prefix of a non-matching subject");
    }

    FILE *partials = fopen("/tmp/libc_probe_partial", "w+");
    if (partials == NULL) {
        return fail("could not open a scratch stream");
    }
    if (fputs("1ex", partials) < 0 || fseek(partials, 0, SEEK_SET) != 0 ||
        fscanf(partials, "%lf", &partial) != 0 || partial != 42.0) {
        fclose(partials);
        return fail("fscanf converted a prefix of a non-matching subject");
    }
    fclose(partials);

    // What one half writes the other reads back: 1207 characters is well
    // inside C's 4095-character floor, and the exponent is at the far end.
    char wide_text[1300];
    double back = 0.0;
    if (snprintf(wide_text, sizeof wide_text, "%.1200e", 1e-300) != 1207) {
        return fail("printf did not render a 1200-digit conversion");
    }
    if (sscanf(wide_text, "%lf", &back) != 1 || back != 1e-300) {
        return fail("sscanf dropped the exponent of a long subject sequence");
    }
    FILE *wides = fopen("/tmp/libc_probe_wide", "w+");
    if (wides == NULL) {
        return fail("could not open a scratch stream");
    }
    back = 0.0;
    if (fputs(wide_text, wides) < 0 || fseek(wides, 0, SEEK_SET) != 0 ||
        fscanf(wides, "%lf", &back) != 1 || back != 1e-300) {
        fclose(wides);
        return fail("fscanf dropped the exponent of a long subject sequence");
    }
    fclose(wides);
    return 1;
}

static int locale(void) {
    if (strcmp(setlocale(LC_ALL, NULL), "C") != 0) {
        return fail("setlocale query did not answer C");
    }
    if (setlocale(LC_ALL, "en_US.UTF-8") != NULL) {
        return fail("setlocale accepted a locale that does not exist");
    }
    struct lconv *conv = localeconv();
    if (strcmp(conv->decimal_point, ".") != 0 ||
        strcmp(conv->thousands_sep, "") != 0 ||
        conv->int_frac_digits != CHAR_MAX) {
        return fail("localeconv is not the C locale");
    }
    if (strcmp(nl_langinfo(CODESET), "UTF-8") != 0 ||
        strcmp(nl_langinfo(DAY_1), "Sunday") != 0 ||
        strcmp(nl_langinfo(ABMON_12), "Dec") != 0 ||
        strcmp(nl_langinfo(D_T_FMT), "%a %b %e %T %Y") != 0 ||
        strcmp(nl_langinfo(THOUSEP), "") != 0 ||
        strcmp(nl_langinfo(0x70000), "") != 0) {
        return fail("nl_langinfo answered the wrong item");
    }
    return 1;
}

static int sorting(void) {
    int values[] = {5, -3, 9, 0, 9, 1, -100, 42, 7, 7};
    size_t count = sizeof values / sizeof values[0];
    qsort(values, count, sizeof values[0], compare_int);
    for (size_t i = 1; i < count; i++) {
        if (values[i - 1] > values[i]) {
            return fail("qsort left the array out of order");
        }
    }
    int key = 42;
    int *found = bsearch(&key, values, count, sizeof key, compare_int);
    if (found == NULL || *found != 42) {
        return fail("bsearch missed a present key");
    }
    key = 43;
    if (bsearch(&key, values, count, sizeof key, compare_int) != NULL) {
        return fail("bsearch found an absent key");
    }

    // Three-byte elements on an odd base: the byte-wise swap path.
    char raw[1 + 3 * 5] = {0, 'd', 'd', 'd', 'b', 'b', 'b', 'e', 'e',
                           'e', 'a', 'a', 'a', 'c', 'c', 'c'};
    qsort(raw + 1, 5, 3, compare_triple);
    if (memcmp(raw + 1, "aaabbbcccdddeee", 15) != 0 || raw[0] != 0) {
        return fail("qsort mishandled an unaligned three-byte element");
    }
    return 1;
}

// Past 16 elements `qsort` partitions instead of insertion-sorting, and past
// 128 it takes its pivot from a ninther. 300 reaches both, several levels
// deep.
enum { DEEP = 300 };

static int deep_sorts(void) {
    int shuffled[DEEP];
    for (size_t i = 0; i < DEEP; i++) {
        shuffled[i] = (int)i;
    }
    uint32_t seed = 0x5eed1234u;
    for (size_t i = DEEP - 1; i > 0; i--) {
        size_t j = noise(&seed) % (i + 1);
        int held = shuffled[i];
        shuffled[i] = shuffled[j];
        shuffled[j] = held;
    }
    qsort(shuffled, DEEP, sizeof shuffled[0], compare_int);
    for (size_t i = 0; i < DEEP; i++) {
        if (shuffled[i] != (int)i) {
            return fail("qsort did not sort a shuffled array");
        }
    }

    // Descending input degenerates when the pivot is swapped into place
    // rather than sorted into it; eight aligned bytes are the word exchange.
    uint64_t falling[DEEP];
    for (size_t i = 0; i < DEEP; i++) {
        falling[i] = DEEP - i;
    }
    qsort(falling, DEEP, sizeof falling[0], compare_u64);
    for (size_t i = 0; i < DEEP; i++) {
        if (falling[i] != i + 1) {
            return fail("qsort did not sort a descending array");
        }
    }

    // One key for the whole array, so every comparison is a tie. The tags
    // are what says whether the sixteen-byte exchange moved both words.
    tagged equal[DEEP];
    for (size_t i = 0; i < DEEP; i++) {
        equal[i].key = 7;
        equal[i].tag = i;
    }
    qsort(equal, DEEP, sizeof equal[0], compare_tagged);
    unsigned char seen[DEEP];
    memset(seen, 0, sizeof seen);
    for (size_t i = 0; i < DEEP; i++) {
        if (equal[i].key != 7 || equal[i].tag >= DEEP || seen[equal[i].tag]) {
            return fail("qsort lost an element of an all-equal array");
        }
        seen[equal[i].tag] = 1;
    }

    // A comparator disagreeing with itself may produce a bad split and
    // nothing else, so order is not asserted — only the guards and elements.
    struct {
        unsigned char head[16];
        int data[DEEP];
        unsigned char tail[16];
    } arena;
    memset(arena.head, 0xa5, sizeof arena.head);
    memset(arena.tail, 0x5a, sizeof arena.tail);
    for (size_t i = 0; i < DEEP; i++) {
        arena.data[i] = (int)i;
    }
    qsort(arena.data, DEEP, sizeof arena.data[0], compare_lying);
    memset(seen, 0, sizeof seen);
    for (size_t i = 0; i < DEEP; i++) {
        int value = arena.data[i];
        if (value < 0 || value >= DEEP || seen[value]) {
            return fail("a lying comparator lost an element");
        }
        seen[value] = 1;
    }
    for (size_t i = 0; i < sizeof arena.head; i++) {
        if (arena.head[i] != 0xa5 || arena.tail[i] != 0x5a) {
            return fail("qsort wrote outside its array");
        }
    }
    return 1;
}

static int errors(void) {
    char buf[64];
    if (strerror_r(EINVAL, buf, sizeof buf) != 0) {
        return fail("strerror_r refused a buffer big enough for its message");
    }
    if (strcmp(strerror(EINVAL), buf) != 0) {
        return fail("strerror and strerror_r disagree");
    }

    int other = EAGAIN;
    pthread_t thread;
    if (pthread_create(&thread, NULL, strerror_in_thread, &other) != 0) {
        return fail("pthread_create failed");
    }
    char *mine = strerror(EINVAL);
    void *theirs = NULL;
    pthread_join(thread, &theirs);
    if (theirs == mine) {
        return fail("two threads shared one strerror buffer");
    }
    if (strcmp(mine, buf) != 0) {
        return fail("another thread's strerror overwrote this one's");
    }
    return 1;
}

static int hex_floats(void) {
    char *end = NULL;
    if (strtod("0x10", &end) != 16.0 || *end != '\0') {
        return fail("strtod did not read a hexadecimal significand");
    }
    if (strtod("0x1.8p1", &end) != 3.0 || *end != '\0') {
        return fail("strtod mis-scaled a binary exponent");
    }
    if (strtod("0x", &end) != 0.0 || *end != 'x') {
        return fail("a digitless 0x must convert the 0 alone");
    }

    errno = 0;
    if (strtod("0x1p1024", &end) != HUGE_VAL || errno != ERANGE) {
        return fail("hexadecimal overflow did not report ERANGE");
    }
    errno = 0;
    if (strtod("0x1p-1074", &end) == 0.0 || errno != 0) {
        return fail("a subnormal is not a range error");
    }

    // The one input where rounding through double gives a different float.
    const char *tie = "0x1.00000100000008p0";
    float once = strtof(tie, &end);
    float twice = (float)strtod(tie, &end);
    if (once == twice) {
        return fail("strtof rounded through double");
    }
    if (strtold("0x1.8p1", &end) != 3.0L) {
        return fail("strtold did not read hexadecimal");
    }
    return 1;
}

static int wide(void) {
    // U+20AC one byte per call: the conversion state has to survive in the
    // eight bytes of the C `mbstate_t`, not in anything Rust-side.
    mbstate_t state;
    memset(&state, 0, sizeof state);
    if (!mbsinit(&state)) {
        return fail("a zeroed mbstate_t is not initial");
    }
    const char euro[] = "\xe2\x82\xac";
    wchar_t wc = 0;
    if (mbrtowc(&wc, euro, 1, &state) != (size_t)-2 || mbsinit(&state)) {
        return fail("a lead byte did not leave a partial state");
    }
    if (mbrtowc(&wc, euro + 1, 1, &state) != (size_t)-2) {
        return fail("a continuation byte did not extend the state");
    }
    if (mbrtowc(&wc, euro + 2, 1, &state) != 1 || wc != 0x20AC ||
        !mbsinit(&state)) {
        return fail("the final byte did not complete the character");
    }

    if (mbrtowc(&wc, "\xc0\x80", 2, &state) != (size_t)-1 || errno != EILSEQ) {
        return fail("an overlong encoding was accepted");
    }

    const wchar_t *text = L"caf\u00e9";
    if (wcslen(text) != 4 || wcscmp(text, L"caf\u00e9") != 0) {
        return fail("the wide string functions disagree with the compiler");
    }

    char narrow[3];
    const wchar_t *cursor = text;
    memset(&state, 0, sizeof state);
    size_t wrote = wcsrtombs(narrow, &cursor, sizeof narrow, &state);
    if (wrote != 3 || cursor != text + 3) {
        return fail("wcsrtombs did not write back the unconverted position");
    }
    cursor = text;
    if (wcsrtombs(NULL, &cursor, 0, &state) != 5 || cursor != text) {
        return fail("a counting wcsrtombs moved the source");
    }

    // C11 7.22 p2 makes `MB_CUR_MAX` the only portable way to size the
    // buffer `wctomb` writes into, so it has to work as an array bound.
    char mb[MB_CUR_MAX];
    int mb_len = wctomb(mb, 0x20AC);
    if (mb_len != 3 || memcmp(mb, euro, 3) != 0) {
        return fail("wctomb into a MB_CUR_MAX buffer did not encode U+20AC");
    }
    wchar_t back = 0;
    if (mbtowc(&back, mb, (size_t)mb_len) != 3 || back != 0x20AC) {
        return fail("mbtowc did not read back what wctomb wrote");
    }
    return 1;
}

static int bulk_conversion(void) {
    // 0x28 cannot continue a two-byte prefix. `*src` is left on the character
    // that failed rather than on the byte that did.
    const char broken[] = "ab\xe2\x82" "\x28" "cd";
    const char *src = broken;
    wchar_t out[8];
    mbstate_t state;
    memset(&state, 0, sizeof state);
    errno = 0;
    if (mbsrtowcs(out, &src, 8, &state) != (size_t)-1 || errno != EILSEQ) {
        return fail("mbsrtowcs accepted an invalid sequence");
    }
    if (src != broken + 2) {
        return fail("mbsrtowcs blamed the byte rather than the character");
    }

    // `nmc` counts bytes, so this one stops mid-character and the state has
    // to carry the half-read character into the next call.
    const char euro[] = "\xe2\x82\xac!";
    src = euro;
    memset(&state, 0, sizeof state);
    if (mbsnrtowcs(out, &src, 2, 8, &state) != 0 || src != euro + 2 ||
        mbsinit(&state)) {
        return fail("mbsnrtowcs did not stop inside a character");
    }
    if (mbsnrtowcs(out, &src, 3, 8, &state) != 2 || src != NULL ||
        out[0] != 0x20AC || out[1] != L'!') {
        return fail("mbsnrtowcs did not resume a partial character");
    }

    // C11 7.22.8.1 p2: `len` characters at most, so a source that fills them
    // leaves no room for a terminator and gets none.
    wchar_t room[8];
    for (size_t i = 0; i < sizeof room / sizeof room[0]; i++) {
        room[i] = 0x7f;
    }
    if (mbstowcs(room, "abcd", 2) != 2 || room[0] != L'a' || room[1] != L'b') {
        return fail("mbstowcs did not store the characters it counted");
    }
    if (room[2] != 0x7f) {
        return fail("mbstowcs terminated a buffer it had filled");
    }

    char narrow[8];
    if (wcstombs(narrow, L"caf\u00e9", sizeof narrow) != 5 ||
        memcmp(narrow, "caf\xc3\xa9", 6) != 0) {
        return fail("wcstombs did not encode the whole string");
    }
    return 1;
}

static int wide_numbers(void) {
    wchar_t *end = NULL;
    const wchar_t *hex = L"  -0x1.8p1xyz";
    if (wcstod(hex, &end) != -3.0 || end != hex + 10) {
        return fail("wcstod did not read a wide hexadecimal float");
    }
    if (wcstof(hex, &end) != -3.0f || end != hex + 10) {
        return fail("wcstof did not read a wide hexadecimal float");
    }
    if (wcstold(hex, &end) != -3.0L || end != hex + 10) {
        return fail("wcstold did not read a wide hexadecimal float");
    }
    const wchar_t *digitless = L"0x";
    if (wcstol(digitless, &end, 16) != 0 || end != digitless + 1) {
        return fail("a digitless 0x must convert the 0 alone");
    }
    const wchar_t *negative = L"-1";
    if (wcstoul(negative, &end, 10) != ULONG_MAX || end != negative + 2) {
        return fail("wcstoul did not negate into the unsigned range");
    }
    if (wcstoll(L"-9223372036854775808", &end, 10) != LLONG_MIN) {
        return fail("wcstoll did not reach its own minimum");
    }

    // 600 characters is past the array the transcoder keeps on the stack, and
    // a subject sequence truncated to fit it would be a different number.
    wchar_t padded[601];
    for (size_t i = 0; i < 599; i++) {
        padded[i] = L'0';
    }
    padded[599] = L'1';
    padded[600] = L'\0';
    errno = 0;
    if (wcstod(padded, &end) != 1.0 || errno != 0 || end != padded + 600) {
        return fail("a 600-character subject sequence was truncated");
    }

    wchar_t huge[601];
    for (size_t i = 0; i < 600; i++) {
        huge[i] = (wchar_t)(L'1' + i % 9);
    }
    huge[600] = L'\0';
    errno = 0;
    if (wcstoull(huge, &end, 10) != ULLONG_MAX || errno != ERANGE ||
        end != huge + 600) {
        return fail("a 600-digit integer did not saturate");
    }
    return 1;
}

static int long_doubles(void) {
    long double root = sqrtl(opaque(2.0L));
    uint64_t sig = 0;
    uint16_t exp = 0;
    ld_bits(root, &sig, &exp);
    if (exp != 0x3fff || sig != 0xb504f333f9de6484ULL) {
        return fail("sqrtl is not the x87's own square root");
    }
    if (root == (long double)sqrt(2.0)) {
        return fail("sqrtl carries only double precision");
    }

    if (nextafterl(opaque(1.0L), opaque(2.0L)) - 1.0L != 0x1p-63L) {
        return fail("nextafterl stepped by a double's ulp");
    }
    if (fmodl(opaque(0x1p16000L), opaque(3.0L)) != 1.0L) {
        return fail("fmodl returned a partial remainder");
    }
    if (library_fabsl(opaque(-0.0L)) != 0.0L ||
        signbit(library_fabsl(opaque(-0.0L)))) {
        return fail("fabsl did not clear the sign of a negative zero");
    }
    if (roundl(opaque(2.5L)) != 3.0L || rintl(opaque(2.5L)) != 2.0L ||
        truncl(opaque(-2.9L)) != -2.0L) {
        return fail("the long double rounding family disagrees with C");
    }

    int power = 0;
    long double fraction = frexpl(opaque(0x1p-16445L), &power);
    if (ldexpl(fraction, power) != 0x1p-16445L) {
        return fail("frexpl and ldexpl do not invert on a subnormal");
    }

    if (sinl(opaque(1.0L)) != (long double)sin(1.0)) {
        return fail("a narrowed entry point disagrees with its double twin");
    }

    // Past ±32767 one FSCALE cannot carry the exponent, so these three are
    // the ones that need the second.
    if (ldexpl(opaque(0x1p-16445L), 32768) != 0x1p16323L) {
        return fail("ldexpl saturated at a single FSCALE");
    }
    if (scalbnl(opaque(1.0L), 100000) != INFINITY) {
        return fail("scalbnl did not overflow to infinity");
    }
    if (scalblnl(opaque(1.0L), -100000L) != 0.0L) {
        return fail("scalblnl did not underflow to zero");
    }

    // Which of two equal zeros comes back is a decision, not a result of the
    // comparison. Both orders: answering the second argument is half right.
    if (signbit(fmaxl(opaque(-0.0L), opaque(0.0L))) ||
        signbit(fmaxl(opaque(0.0L), opaque(-0.0L))) ||
        !signbit(fmaxl(opaque(-0.0L), opaque(-0.0L))) ||
        !signbit(fminl(opaque(0.0L), opaque(-0.0L))) ||
        !signbit(fminl(opaque(-0.0L), opaque(0.0L))) ||
        signbit(fminl(opaque(0.0L), opaque(0.0L)))) {
        return fail("fmaxl or fminl answered the wrong zero");
    }

    // An x87 stack leak only shows after eight pushes, and this is past
    // every call above as well as its own.
    for (int i = 0; i < 64; i++) {
        volatile long double acc =
            library_fabsl(opaque(-1.5L)) + ceill(opaque(0.25L)) +
            floorl(opaque(2.75L)) + truncl(opaque(3.5L)) +
            sqrtl(opaque(4.0L)) + logbl(opaque(8.0L)) +
            fmaxl(opaque(1.0L), opaque(2.0L)) +
            fminl(opaque(1.0L), opaque(2.0L));
        (void)acc;
    }
    ld_bits(sqrtl(opaque(2.0L)), &sig, &exp);
    if (exp != 0x3fff || sig != 0xb504f333f9de6484ULL) {
        return fail("the x87 stack did not survive a run of calls");
    }
    return 1;
}

typedef void (*entry)(void);

// Every `long double` entry point, named so that `ld.lld -static` resolves
// it here. Neither the generator's ABI check nor its export scan sees these
// symbols, so a dropped one links everywhere and fails at its first caller.
static int entry_points(void) {
    // `volatile`, so each load really happens: told to read it, clang cannot
    // answer the test below from the addresses themselves and drop the table.
    static const volatile entry all[] = {
        (entry)acosl,      (entry)acoshl,     (entry)asinl,
        (entry)asinhl,     (entry)atanl,      (entry)atanhl,
        (entry)atan2l,     (entry)cbrtl,      (entry)ceill,
        (entry)copysignl,  (entry)cosl,       (entry)coshl,
        (entry)erfl,       (entry)erfcl,      (entry)expl,
        (entry)exp2l,      (entry)expm1l,     (entry)fabsl,
        (entry)fdiml,      (entry)floorl,     (entry)fmal,
        (entry)fmaxl,      (entry)fminl,      (entry)fmodl,
        (entry)frexpl,     (entry)hypotl,     (entry)ilogbl,
        (entry)ldexpl,     (entry)lgammal,    (entry)llrintl,
        (entry)llroundl,   (entry)logl,       (entry)log10l,
        (entry)log1pl,     (entry)log2l,      (entry)logbl,
        (entry)lrintl,     (entry)lroundl,    (entry)modfl,
        (entry)nanl,       (entry)nearbyintl, (entry)nextafterl,
        (entry)powl,       (entry)remainderl, (entry)remquol,
        (entry)rintl,      (entry)roundl,     (entry)scalblnl,
        (entry)scalbnl,    (entry)sinl,       (entry)sinhl,
        (entry)sqrtl,      (entry)tanl,       (entry)tanhl,
        (entry)tgammal,    (entry)truncl,
    };
    _Static_assert(sizeof all / sizeof all[0] == 56,
                   "every long double entry point is named here");

    for (size_t i = 0; i < sizeof all / sizeof all[0]; i++) {
        if (all[i] == NULL) {
            return fail("a long double entry point resolved to nothing");
        }
    }
    return 1;
}

static int run(void) {
    static int (*const checks[])(void) = {
        jumps,           mask_jumps,      calendar,
        formatting,      cpu_clock,       float_formatting,
        float_scanning,  locale,          sorting,
        deep_sorts,      errors,          hex_floats,
        wide,            bulk_conversion, wide_numbers,
        long_doubles,    entry_points,
    };
    for (size_t i = 0; i < sizeof checks / sizeof checks[0]; i++) {
        check = (int)i + 1;
        if (!checks[i]()) {
            return 0;
        }
    }
    return 1;
}

int main(void) {
    return run() ? 0 : check;
}

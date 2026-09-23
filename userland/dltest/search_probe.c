/* The loader's library search, seen from a dynamically linked C program.
 *
 * One source, linked four times with different DT_RPATH/DT_RUNPATH entries
 * (see build_userland.sh). dl_test lays copies out under /tmp, runs them and
 * grades the exit status: 0 passes, any other value names the failed check.
 *
 *   open NAME EXPECT   dlopen(NAME); EXPECT is the path the object providing
 *                      dlsearch_tag must have come from, or "-" for "must
 *                      not be found".
 *   self PATH SECURE   AT_EXECFN and dladdr(main) both name PATH, and
 *                      AT_SECURE reads SECURE.
 */

#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>

#ifndef AT_SECURE
#define AT_SECURE 23
#endif
#ifndef AT_EXECFN
#define AT_EXECFN 31
#endif

int main(int argc, char **argv);

static int loaded_from(void *handle, const char *expect) {
    void *tag = dlsym(handle, "dlsearch_tag");
    if (!tag) {
        return 3;
    }
    Dl_info info;
    if (!dladdr(tag, &info) || !info.dli_fname) {
        return 4;
    }
    if (strcmp(info.dli_fname, expect) != 0) {
        fprintf(stderr, "dl_search: loaded %s, expected %s\n", info.dli_fname, expect);
        return 5;
    }
    return 0;
}

static int check_self(const char *path, const char *secure) {
    const char *execfn = (const char *)getauxval(AT_EXECFN);
    if (!execfn || strcmp(execfn, path) != 0) {
        fprintf(stderr, "dl_search: AT_EXECFN is %s, expected %s\n",
                execfn ? execfn : "(none)", path);
        return 10;
    }
    Dl_info info;
    if (!dladdr((const void *)&main, &info) || !info.dli_fname ||
        strcmp(info.dli_fname, path) != 0) {
        return 11;
    }
    if (getauxval(AT_SECURE) != strtoul(secure, NULL, 10)) {
        return 12;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 4 && strcmp(argv[1], "open") == 0) {
        void *handle = dlopen(argv[2], RTLD_NOW);
        if (strcmp(argv[3], "-") == 0) {
            return handle ? 2 : 0;
        }
        if (!handle) {
            fprintf(stderr, "dl_search: dlopen(%s): %s\n", argv[2], dlerror());
            return 1;
        }
        return loaded_from(handle, argv[3]);
    }
    if (argc == 4 && strcmp(argv[1], "self") == 0) {
        return check_self(argv[2], argv[3]);
    }
    return 100;
}

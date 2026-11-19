/* libpkgtest.so: the library the test package's service carries in its
 * image's lib/, which nothing in the base's search path holds. */

const char *pkgtest_greeting(void) {
    return "its-own-lib";
}

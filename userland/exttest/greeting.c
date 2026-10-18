/* libexttest.so: the library the test extension's service carries in its
 * image's lib/, which nothing in the base's search path holds. */

const char *exttest_greeting(void) {
    return "its-own-lib";
}

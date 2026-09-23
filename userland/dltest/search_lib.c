/* libdlsearch.so: what search_probe looks for. Which copy was loaded is read
 * back from its path, so the body is immaterial. */

int dlsearch_tag(void) {
    return 7;
}

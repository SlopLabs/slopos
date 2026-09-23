/* libdlrunpath.so and libdlplain.so: a DT_NEEDED on libdlsearch.so, which
 * the loader resolves with this object's own search paths — its DT_RUNPATH,
 * or with none, the DT_RPATH of whoever loaded it. */

int dlsearch_tag(void);

int dlsearch_dep_tag(void) {
    return dlsearch_tag();
}

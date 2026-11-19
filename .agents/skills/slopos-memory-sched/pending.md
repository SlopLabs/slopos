# Not yet in the docs

- `docs/architecture/memory` (kernel heap): the large tier (`mm/src/slab/large.rs`,
  allocations over 2048 bytes) serves a request only a region of its own page
  count. A freed region of up to 16 pages waits on the list of its size, the
  lists holding 4 MiB together (`CACHE_PAGES`); any other goes back to the
  buddy and off the root's `KernelMeta` charge. A request never takes a
  larger cached region, and nothing freed stays out of the buddy past the
  cache.

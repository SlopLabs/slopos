# Not yet in the docs

- `docs/architecture/filesystem`: `FileSystem::file_runs` answers where a
  file's bytes lie on its filesystem's device. ext4 answers only for a
  sealed, extent-mapped regular file with no hole or unwritten extent, after
  writing its dirty blocks home and waiting out the flusher's batch in flight,
  and only when every block's home holds its newest contents
  (`BlockCache::home_holds`: no dirty or in-flight copy, none in the log).
- A fault on an executable mapping reads the aligned window of
  `READAHEAD_PAGES` around its page (`FileFaultPlan::read_around`), the pages
  before it too; any other fault reads the pages after it.

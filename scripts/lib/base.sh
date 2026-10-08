# What a base image carries, sourced by the justfile, scripts/selfhost.sh and
# scripts/build_userland.sh so the host and the guest build one system. POSIX
# sh.

# The shipped programs: `/bin/<name>`, and `init` at `/sbin/init`.
BASE_PROGRAMS="init shell coreutils terminal compositor roulette halt bootctl installer editor file_manager image_viewer sysmon nmap ip keymap ss nc curl ping oops_smoke cpufreq remoted kconsole"

# The multicall utility binary's installed names. `/bin/<name>` is a symlink to
# `/bin/coreutils`, which dispatches on `argv[0]` — one binary rather than
# fifty-odd copies of std. This list is the *installed* set; the binary's own
# table is the implemented set, and `coreutils_test` fails if they disagree.
COREUTILS_TOOLS="ls cat cp mv rm mkdir rmdir ln touch stat install mktemp basename dirname which grep sed find xargs sort uniq tr cut head tail wc tee cmp diff patch printf echo test [ true false yes seq sleep env nproc uname whoami pwd date hexdump ps tar gzip gunzip zcat sha256sum stty less prof"

# The programs a recipe builds that the base carries, by their path below the
# recipe prefix, which they keep below `/`: e2fsprogs's, which format, check and
# repair the volumes SlopOS mounts. A host build takes them from the recipes'
# prefix, a guest build from /usr/local, where the toolchain carries them.
BASE_RECIPE_PROGRAMS="sbin/mke2fs sbin/e2fsck sbin/resize2fs sbin/tune2fs sbin/debugfs sbin/dumpe2fs"
# The recipes they come from, whose licence texts the base carries beside them.
BASE_RECIPES="e2fsprogs"

# The tests base: the shipped programs and the suite's. `bigprog_test` costs
# ~25 MB of guest RAM on every test boot.
BASE_TEST_PROGRAMS="$BASE_PROGRAMS dl_probe dl_test dl_search_origin dl_search_rpath dl_search_runpath dl_secure_probe cxx_probe cxx_static_probe cxx_test libc_probe libc_probe_static fork_test io_capture_test heap_allocator_test image_test curl_recv_repro_test curl_e2e_test cd_test script_exec_test buildctl_test coreutils_test ring_test pidfd_e2e_test signalfd_test slopfut_test multishot_test tls_independence_test percore_reactor_test signal_handler_test sigwinch_default_test ctrlc_flood_test pty_flow_test mm_stress_test bigprog_test spin_signal_test terminal_grid_test sysmon_selection_test clipboard_test keymap_test appkit_test editor_test spawn_privilege_test seat_test mount_test install_test installer_test stdio_stream_test shell_script_test ip_e2e_test rlimit_test fifo_test session_smoke_test spawn_output_test dns_resolve_test dns_concurrent_test transfer_test persist_test reboot_clone_test libc_abi_test toolchain_test selfhost_test buildloop_test exit_stress_test cpufreq_test usb_shell_test"

# Shared objects the suite dlopens: libraries, not programs, and never on the
# shipped base.
BASE_TEST_SHARED_OBJECTS="libdltest.so libc++.so libcxxtest.so libdlsearch-fixture.so libdlrunpath.so libdlplain.so"

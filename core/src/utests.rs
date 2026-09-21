//! Userland test-binary registrations.
//!
//! Each [`utest!`](crate::utest) here corresponds to a binary in
//! `userland/src/bin/tests/`, which must also be listed in the justfile's
//! `test_userland_bins` to be packaged into `ext2-tests.img`.

crate::utest!(
    name = utest_heap_allocator,
    bin = "/bin/heap_allocator_test"
);
crate::utest!(name = utest_image, bin = "/bin/image_test");
crate::utest!(name = utest_fork, bin = "/bin/fork_test");
crate::utest!(name = utest_io_capture, bin = "/bin/io_capture_test");
crate::utest!(
    name = utest_curl_recv_repro,
    bin = "/bin/curl_recv_repro_test"
);
crate::utest!(name = utest_curl_e2e, bin = "/bin/curl_e2e_test");
crate::utest!(name = utest_dns_resolve, bin = "/bin/dns_resolve_test");
crate::utest!(name = utest_cd, bin = "/bin/cd_test");
crate::utest!(name = utest_buildctl, bin = "/bin/buildctl_test");
crate::utest!(name = utest_coreutils, bin = "/bin/coreutils_test");
crate::utest!(name = utest_dl, bin = "/bin/dl_test");
crate::utest!(name = utest_cxx, bin = "/bin/cxx_test");
crate::utest!(name = utest_spawn_output, bin = "/bin/spawn_output_test");
crate::utest!(name = utest_ring, bin = "/bin/ring_test");
crate::utest!(name = utest_pidfd, bin = "/bin/pidfd_e2e_test");
crate::utest!(name = utest_signalfd, bin = "/bin/signalfd_test");
crate::utest!(name = utest_slopfut, bin = "/bin/slopfut_test");
crate::utest!(name = utest_multishot, bin = "/bin/multishot_test");
crate::utest!(
    name = utest_tls_independence,
    bin = "/bin/tls_independence_test"
);
crate::utest!(
    name = utest_percore_reactor,
    bin = "/bin/percore_reactor_test"
);
crate::utest!(
    name = utest_signal_handler,
    bin = "/bin/signal_handler_test"
);
crate::utest!(name = utest_ctrlc_flood, bin = "/bin/ctrlc_flood_test");
crate::utest!(name = utest_pty_flow, bin = "/bin/pty_flow_test");
crate::utest!(name = utest_shell_script, bin = "/bin/shell_script_test");
crate::utest!(name = utest_mm_stress, bin = "/bin/mm_stress_test");
crate::utest!(name = utest_bigprog, bin = "/bin/bigprog_test");
crate::utest!(
    name = utest_sigwinch_default,
    bin = "/bin/sigwinch_default_test"
);
crate::utest!(name = utest_spin_signal, bin = "/bin/spin_signal_test");
crate::utest!(name = utest_terminal_grid, bin = "/bin/terminal_grid_test");
crate::utest!(
    name = utest_sysmon_selection,
    bin = "/bin/sysmon_selection_test"
);
crate::utest!(name = utest_clipboard, bin = "/bin/clipboard_test");
crate::utest!(name = utest_keymap, bin = "/bin/keymap_test");
crate::utest!(name = utest_appkit, bin = "/bin/appkit_test");
crate::utest!(name = utest_editor, bin = "/bin/editor_test");
crate::utest!(
    name = utest_spawn_privilege,
    bin = "/bin/spawn_privilege_test"
);
crate::utest!(name = utest_seat, bin = "/bin/seat_test");
crate::utest!(name = utest_mount, bin = "/bin/mount_test");
crate::utest!(name = utest_devdisk, bin = "/bin/devdisk_test");
crate::utest!(name = utest_stdio_stream, bin = "/bin/stdio_stream_test");
crate::utest!(name = utest_ip_e2e, bin = "/bin/ip_e2e_test");
crate::utest!(name = utest_rlimit, bin = "/bin/rlimit_test");
crate::utest!(name = utest_persist, bin = "/bin/persist_test");

// Nothing below is ordered by its position in this file. The userland phase
// walks `ktesting::registry::registry_sorted()`, which orders by
// `(module, name)`, and every utest here shares one module — so the sorted
// `name` is the running order, and this file's order is presentation only.
//
// `libc_abi` opens AF_UNIX endpoints, and the kernel phase has a known bug
// where exercising unix sockets leaves UDP dead for the rest of the run (see
// `dns_resolve_test`'s header), so it has to follow every network case. It
// does, by name: `utest_curl_e2e`, `utest_curl_recv_repro`,
// `utest_dns_resolve` and `utest_ip_e2e` all sort before `utest_libc_abi`. A
// network case named past `l` breaks that and has to be renamed.
crate::utest!(name = utest_libc_abi, bin = "/bin/libc_abi_test");

// Not last, and deliberately not renamed to be: `utest_s…` sorts ahead of
// `utest_terminal_grid` and `utest_tls_independence`. This one spawns eight
// concurrent children, so its `process` peak is those eight plus every account
// the phase has accumulated by the time it runs — moving it later moves that
// peak, and `scripts/gates/quota/tests.txt` records the number exactly. What
// it contributes to the `post-userland-tests` dump is a desktop-shaped
// population wherever it lands.
crate::utest!(name = utest_session_smoke, bin = "/bin/session_smoke_test");

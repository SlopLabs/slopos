crate::define_service! {
    dns => DnsServices {
        /// Resolve a hostname to an IPv4 address.
        ///
        /// * `hostname` — pointer to hostname bytes (not NUL-terminated)
        /// * `hostname_len` — length of hostname in bytes
        /// * `result` — pointer to `[u8; 4]` output for the resolved address
        ///
        /// Returns 0 on success, negative errno on failure.
        resolve(hostname: *const u8, hostname_len: usize, result: *mut [u8; 4]) -> i32;
    }
}

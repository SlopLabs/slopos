/// Extract a NUL-padded byte array as a `&str`.
///
/// Truncates at the first NUL; returns `"<invalid>"` if the prefix is not
/// valid UTF-8.
#[inline]
pub fn bytes_as_str(buf: &[u8]) -> &str {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..len]).unwrap_or("<invalid>")
}

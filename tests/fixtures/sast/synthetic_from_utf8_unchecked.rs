// Synthetic violation: tripped by
//   .semgrep/barista-rust.yml :: barista-rust-string-from-utf8-unchecked
//
// Bypassing UTF-8 validation with `from_utf8_unchecked` is `unsafe`
// and should never appear outside the cache hot path. The rule fires
// on every occurrence regardless of crate.

pub fn round_trip(bytes: Vec<u8>) -> String {
    // Violation: prefer String::from_utf8 (Result) or
    // String::from_utf8_lossy (Cow<str>).
    unsafe { String::from_utf8_unchecked(bytes) }
}

// Synthetic violation: tripped by
//   .semgrep/barista-rust.yml :: barista-rust-transmute
//
// `mem::transmute` outside the cache crate is a strong signal that
// something is using `unsafe` to bypass the type system. The rule
// excludes `crates/barista-cache/**` (vetted CAS code) and fires on
// every other path.

use std::mem;

#[repr(C)]
struct Wrapper([u8; 4]);

pub fn boom(w: Wrapper) -> u32 {
    // Violation: prefer `u32::from_ne_bytes(w.0)` or a `bytemuck`
    // cast with a compile-time soundness proof.
    unsafe { mem::transmute::<Wrapper, u32>(w) }
}

// SPDX-License-Identifier: MIT OR Apache-2.0

//! Windows DACL builder for owner-only named pipes.
//!
//! Builds a `SECURITY_ATTRIBUTES` whose `lpSecurityDescriptor`
//! points at a discretionary ACL (DACL) granting full read/write
//! access to exactly two SIDs:
//!
//! * The current process token's user SID (the "daemon owner").
//! * `NT AUTHORITY\SYSTEM` (S-1-5-18), so an admin / installer
//!   running as SYSTEM can still inspect or terminate the pipe
//!   if needed.
//!
//! Everyone else — including other interactive users on the same
//! machine, even other admins — gets `ERROR_ACCESS_DENIED` from
//! `CreateFileW` at open time.
//!
//! # Why a hand-rolled DACL
//!
//! tokio's [`ServerOptions`] exposes
//! `create_with_security_attributes_raw` which takes a raw
//! `*mut SECURITY_ATTRIBUTES`. We build that pointer here using
//! `windows-sys`. The alternative — passing a NULL SD — gives the
//! pipe a default DACL inherited from the process token, which on
//! a typical workstation grants `Authenticated Users` enough access
//! to *open* the pipe (admins) or to be probed for existence (any
//! local user). The explicit DACL is the security-bearing call.
//!
//! # Memory ownership
//!
//! The SD + SID buffers are owned by [`PipeDacl`] for the lifetime
//! of the pipe handle. tokio takes ownership of the pipe handle on
//! `create_with_security_attributes_raw`; we must keep our SD alive
//! at least until that call returns. We use `Box::leak` for the SD
//! buffer (released on `PipeDacl::drop` via `Box::from_raw`) and
//! `LocalFree` for the SYSTEM SID (allocated by
//! `ConvertStringSidToSidW`). The user-SID buffer is owned inline
//! in the struct.

#![cfg(windows)]
// The DACL builder relies on direct FFI into advapi32 / kernel32 — every
// helper here mirrors a Microsoft-documented API. `as` casts are
// unavoidable for converting `usize` lengths into `u32` Win32 sizes.
// `mem_forget` is a workspace-level deny; the SidGuard disarm pattern
// uses an explicit `disarmed` bool instead of `mem::forget`.
#![allow(
    clippy::as_conversions,
    reason = "Win32 FFI requires usize→u32 size casts; documented per-call"
)]

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl,
    InitializeSecurityDescriptor, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::{AuthError, Result};

/// Win32 access mask granting the standard read/write/execute set
/// the named-pipe transport needs. The pipe is opened with
/// `GENERIC_READ | GENERIC_WRITE`; `SYNCHRONIZE` is included
/// because every Win32 sync object needs it to be waitable.
const PIPE_ACCESS_MASK: u32 = 0x1F01FF; // FILE_ALL_ACCESS — matches NamedPipeServer's open mode

/// The SDDL string for `NT AUTHORITY\SYSTEM`. Pre-defined SID;
/// always valid; doesn't depend on the local SAM database.
const SYSTEM_SID_SDDL: &[u16] = &[
    'S' as u16, '-' as u16, '1' as u16, '-' as u16, '5' as u16, '-' as u16, '1' as u16, '8' as u16,
    0,
];

/// A constructed DACL ready to be passed to tokio's
/// `create_with_security_attributes_raw`.
///
/// Construction allocates:
///
/// * One `Vec<u8>` for the user SID (copied out of the token).
/// * One `LocalAlloc`'d SID for `NT AUTHORITY\SYSTEM`.
/// * One `Vec<u8>` sized for the SECURITY_DESCRIPTOR + DACL + 2
///   ACEs.
/// * One boxed `SECURITY_ATTRIBUTES` that points at the above.
///
/// All four are released on `Drop`. The pointer returned by
/// [`Self::raw_attrs`] is valid for as long as the `PipeDacl`
/// lives — the caller MUST keep the `PipeDacl` alive until the
/// `NamedPipeServer` has been created (i.e. the
/// `create_with_security_attributes_raw` call has returned).
pub(crate) struct PipeDacl {
    /// The constructed SECURITY_ATTRIBUTES, heap-allocated so the
    /// pointer we hand to tokio is stable.
    attrs: Box<SECURITY_ATTRIBUTES>,

    /// Backing storage for the SECURITY_DESCRIPTOR + DACL.
    /// `attrs.lpSecurityDescriptor` points into this buffer.
    _sd_buf: Vec<u8>,

    /// Heap-allocated copy of the current user's SID. The DACL's
    /// first ACE references this; we keep it alive for the DACL's
    /// lifetime.
    _user_sid: Vec<u8>,

    /// `LocalAlloc`'d SID for `NT AUTHORITY\SYSTEM`. Freed via
    /// `LocalFree` in `Drop`.
    system_sid: PSID,
}

impl PipeDacl {
    /// Build the DACL.
    ///
    /// Returns [`AuthError::Io`] on any underlying Win32 failure
    /// (`OpenProcessToken`, `GetTokenInformation`,
    /// `ConvertStringSidToSidW`, ACL init / append). Each failure
    /// carries the precise Win32 error code via `GetLastError`.
    pub fn new() -> Result<Self> {
        // ---- 1. Get the current user's SID from our process token.
        let user_sid = current_user_sid()?;

        // ---- 2. Allocate the SYSTEM SID via ConvertStringSidToSidW.
        let mut system_sid: PSID = ptr::null_mut();
        // SAFETY: SYSTEM_SID_SDDL is a static null-terminated UTF-16
        // string we control. `system_sid` is a stack-local out-ptr.
        let ok = unsafe { ConvertStringSidToSidW(SYSTEM_SID_SDDL.as_ptr(), &raw mut system_sid) };
        if ok == 0 {
            return Err(AuthError::Io(io::Error::last_os_error()));
        }
        // From here, on any error path we must `LocalFree(system_sid)`
        // to avoid leaking the kernel-side allocation.
        let guard = SidGuard::new(system_sid);

        // ---- 3. Compute the buffer size we need for the SD + DACL.
        //
        // SECURITY_DESCRIPTOR is a fixed-size opaque struct
        // (`SECURITY_DESCRIPTOR_MIN_LENGTH` = 20 bytes on x64, but
        // we use `size_of::<SECURITY_DESCRIPTOR>()` to track upstream).
        //
        // The DACL needs: ACL header + per-ACE { ACCESS_ALLOWED_ACE
        // header + SID body }. We have two ACEs (user + SYSTEM).
        // SAFETY: GetLengthSid reads the length of a valid SID; both
        // `user_sid` and `system_sid` are valid SIDs constructed above.
        let sid_user_len = unsafe { GetLengthSid(user_sid.as_ptr().cast::<c_void>().cast_mut()) };
        let sid_sys_len = unsafe { GetLengthSid(system_sid) };

        // ACCESS_ALLOWED_ACE = 8 bytes of header + the SID body, minus
        // 4 bytes of the in-struct SID stub that overlaps the first
        // DWORD of the SID. Microsoft's docs give the formula as
        // `sizeof(ACCESS_ALLOWED_ACE) - sizeof(DWORD) + GetLengthSid(sid)`.
        // ACCESS_ALLOWED_ACE is 12 bytes in windows-sys; sizeof(DWORD) = 4.
        let ace_header_overhead: u32 = 12 - 4; // = 8
        let dacl_size: u32 = size_of::<ACL>() as u32
            + ace_header_overhead
            + sid_user_len
            + ace_header_overhead
            + sid_sys_len;

        // Buffer: SECURITY_DESCRIPTOR followed by DACL. We allocate
        // one Vec<u8> and slice it; the SD's `lpDacl` pointer is set
        // to the DACL portion.
        let sd_align = size_of::<u32>();
        let sd_size = size_of::<windows_sys::Win32::Security::SECURITY_DESCRIPTOR>();
        // Pad the SD region so the DACL starts u32-aligned.
        let sd_padded = (sd_size + sd_align - 1) & !(sd_align - 1);
        let total = sd_padded + dacl_size as usize;
        let mut sd_buf = vec![0u8; total];

        // ---- 4. Initialize the SECURITY_DESCRIPTOR.
        let sd_ptr: PSECURITY_DESCRIPTOR = sd_buf.as_mut_ptr().cast::<c_void>();
        // SAFETY: sd_ptr points at a sufficiently-sized zero-init
        // buffer we own; revision is the documented constant.
        let ok = unsafe { InitializeSecurityDescriptor(sd_ptr, SECURITY_DESCRIPTOR_REVISION) };
        if ok == 0 {
            return Err(AuthError::Io(io::Error::last_os_error()));
        }

        // ---- 5. Initialize the DACL inside the same buffer.
        // SAFETY: dacl_ptr points sd_padded bytes into sd_buf;
        // dacl_size bytes are reserved for the DACL.
        let dacl_ptr: *mut ACL = unsafe { sd_buf.as_mut_ptr().add(sd_padded).cast::<ACL>() };
        let ok = unsafe { InitializeAcl(dacl_ptr, dacl_size, ACL_REVISION) };
        if ok == 0 {
            return Err(AuthError::Io(io::Error::last_os_error()));
        }

        // ---- 6. Add the two ACCESS_ALLOWED ACEs.
        // SAFETY: both SID pointers are valid (live for the lifetime
        // of this fn body; `_user_sid` outlives this scope and
        // `system_sid` is freed on error or stored in self below).
        let ok = unsafe {
            AddAccessAllowedAce(
                dacl_ptr,
                ACL_REVISION,
                PIPE_ACCESS_MASK,
                user_sid.as_ptr().cast::<c_void>() as PSID,
            )
        };
        if ok == 0 {
            return Err(AuthError::Io(io::Error::last_os_error()));
        }

        // SAFETY: `dacl_ptr` points to the initialized ACL sized above;
        // `system_sid` is a valid SID. AddAccessAllowedAce appends one ACE.
        let ok =
            unsafe { AddAccessAllowedAce(dacl_ptr, ACL_REVISION, PIPE_ACCESS_MASK, system_sid) };
        if ok == 0 {
            return Err(AuthError::Io(io::Error::last_os_error()));
        }

        // ---- 7. Attach the DACL to the SD.
        // SAFETY: sd_ptr is the SD we initialized above; dacl_ptr is
        // a DACL inside the same buffer. `BOOL` in windows-sys is
        // `i32` — pass `1`/`0` as `i32` literals.
        let ok = unsafe { SetSecurityDescriptorDacl(sd_ptr, 1_i32, dacl_ptr, 0_i32) };
        if ok == 0 {
            return Err(AuthError::Io(io::Error::last_os_error()));
        }

        // ---- 8. Build the SECURITY_ATTRIBUTES wrapper.
        #[allow(clippy::as_conversions)]
        let attrs = Box::new(SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd_ptr,
            bInheritHandle: 0,
        });

        // We've successfully built everything; consume the SidGuard
        // so its Drop doesn't free our SYSTEM SID.
        let sys_sid = guard.into_inner();

        Ok(Self {
            attrs,
            _sd_buf: sd_buf,
            _user_sid: user_sid,
            system_sid: sys_sid,
        })
    }

    /// Raw pointer to the `SECURITY_ATTRIBUTES`. Valid until `self`
    /// is dropped.
    pub fn raw_attrs(&self) -> *mut SECURITY_ATTRIBUTES {
        // The Box's pointer is stable; we cast away const because
        // tokio's API takes `*mut`.
        let ptr: *const SECURITY_ATTRIBUTES = &*self.attrs;
        ptr.cast_mut()
    }
}

impl Drop for PipeDacl {
    fn drop(&mut self) {
        if !self.system_sid.is_null() {
            // SAFETY: `system_sid` was allocated by
            // `ConvertStringSidToSidW`, which docs require
            // `LocalFree` to release.
            unsafe {
                LocalFree(self.system_sid as HLOCAL);
            }
            self.system_sid = ptr::null_mut();
        }
    }
}

/// RAII guard that `LocalFree`s a SID on drop unless explicitly
/// disarmed via `into_inner`. Used during `PipeDacl::new` so that
/// any `?`-returned early exit cleans up the kernel allocation.
///
/// The disarm pattern uses a `disarmed` bool rather than
/// `mem::forget` because the workspace lint policy denies
/// `clippy::mem_forget`. Functionally identical: `into_inner` flips
/// the bool, `Drop` checks the bool before freeing.
struct SidGuard {
    sid: PSID,
    disarmed: bool,
}

impl SidGuard {
    fn new(sid: PSID) -> Self {
        Self {
            sid,
            disarmed: false,
        }
    }

    fn into_inner(mut self) -> PSID {
        self.disarmed = true;
        self.sid
    }
}

impl Drop for SidGuard {
    fn drop(&mut self) {
        if !self.disarmed && !self.sid.is_null() {
            // SAFETY: same as PipeDacl::drop.
            unsafe {
                LocalFree(self.sid as HLOCAL);
            }
        }
    }
}

/// Read the current process's user SID via `OpenProcessToken` +
/// `GetTokenInformation(TokenUser)`.
///
/// Returns the SID bytes as a `Vec<u8>` (the in-token SID is
/// referenced by a transient pointer that becomes invalid when we
/// close the token handle; we copy out for stable ownership).
fn current_user_sid() -> Result<Vec<u8>> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that's
    // always valid; `&raw mut token` is a stack-local out-ptr.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
    if ok == 0 {
        return Err(AuthError::Io(io::Error::last_os_error()));
    }
    let token_guard = HandleGuard(token);

    // First call: ask for the required size (`return_length` out;
    // returns 0 + `ERROR_INSUFFICIENT_BUFFER`).
    let mut required: u32 = 0;
    // SAFETY: sizing call — `token` is a live handle; a null buffer with
    // length 0 is the documented way to ask for the required size via
    // `&raw mut required` (an owned stack-local out-ptr).
    let _ = unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &raw mut required) };
    if required == 0 {
        return Err(AuthError::Io(io::Error::last_os_error()));
    }

    let mut buf = vec![0u8; required as usize];
    // SAFETY: buf is sized to `required` bytes; kernel writes the
    // TOKEN_USER + SID body into it.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast::<c_void>(),
            required,
            &raw mut required,
        )
    };
    if ok == 0 {
        return Err(AuthError::Io(io::Error::last_os_error()));
    }
    drop(token_guard);

    // `TOKEN_USER` is `{ User: SID_AND_ATTRIBUTES }`. The first field
    // of SID_AND_ATTRIBUTES is `Sid: PSID` pointing into the same
    // buffer. We extract the SID's length and copy out its bytes.
    // SAFETY: buf holds a valid TOKEN_USER as just initialized.
    let tu: &TOKEN_USER = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    let sid_ptr = tu.User.Sid;
    if sid_ptr.is_null() {
        return Err(AuthError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "TOKEN_USER.User.Sid was NULL",
        )));
    }
    // SAFETY: `sid_ptr` was just null-checked and points into `buf`'s
    // valid TOKEN_USER; GetLengthSid only reads the SID header.
    let sid_len = unsafe { GetLengthSid(sid_ptr) };
    let mut out = vec![0u8; sid_len as usize];
    // SAFETY: src points to `sid_len` valid bytes inside `buf`; dst
    // is a freshly-allocated `Vec<u8>` of the same size.
    unsafe {
        ptr::copy_nonoverlapping(sid_ptr.cast::<u8>(), out.as_mut_ptr(), sid_len as usize);
    }
    Ok(out)
}

/// RAII close on a Win32 `HANDLE`.
struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: handle is a real Win32 HANDLE we own.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests — Windows-only.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::as_conversions,
        unsafe_code
    )]

    use super::*;

    #[test]
    fn current_user_sid_returns_nonempty() {
        let sid = current_user_sid().expect("should succeed for the running process");
        assert!(!sid.is_empty());
        // SID always starts with revision byte 0x01.
        assert_eq!(sid[0], 1, "SID revision byte should be 1");
    }

    #[test]
    fn pipe_dacl_new_succeeds() {
        let dacl = PipeDacl::new().expect("PipeDacl construction should succeed");
        assert!(!dacl.raw_attrs().is_null());
        // SAFETY: we just verified non-null and PipeDacl outlives
        // the borrow.
        let attrs = unsafe { &*dacl.raw_attrs() };
        assert_eq!(attrs.nLength, size_of::<SECURITY_ATTRIBUTES>() as u32);
        assert!(!attrs.lpSecurityDescriptor.is_null());
        assert_eq!(attrs.bInheritHandle, 0, "handles should not be inheritable");
    }
}

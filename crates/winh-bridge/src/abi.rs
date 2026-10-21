//! Values verified against the official Speech SDK 1.49.0 C headers.
use core::ffi::{c_char, c_int, c_void};

pub type Handle = *mut c_void;
pub type Status = usize;
pub type Callback = Option<unsafe extern "system" fn(Handle, Handle, *mut c_void)>;
pub const OK: Status = 0;
pub const INVALID_ARG: Status = 0x005;
pub const TIMEOUT: Status = 0x006;
pub const BUFFER_TOO_SMALL: Status = 0x019;
pub const INVALID_HANDLE: Status = 0x021;
pub const NOT_IMPL: Status = 0xfff;
pub const INVALID: Handle = usize::MAX as Handle;
pub const RECOGNIZING: c_int = 2;
pub const RECOGNIZED: c_int = 3;
pub const CANCELED: c_int = 1;
pub const CANCEL_ERROR: c_int = 1;
pub const CANCEL_USER: c_int = 3;

pub type Setter = unsafe extern "system" fn(Handle, Callback, *mut c_void) -> Status;
pub type AsyncStart = unsafe extern "system" fn(Handle, *mut Handle) -> Status;
pub type Wait = unsafe extern "system" fn(Handle, u32) -> Status;
pub type Release = unsafe extern "system" fn(Handle) -> Status;
pub type Valid = unsafe extern "system" fn(Handle) -> bool;
pub type HandleOut = unsafe extern "system" fn(Handle, *mut Handle) -> Status;
pub type U64Out = unsafe extern "system" fn(Handle, *mut u64) -> Status;
pub type U32Out = unsafe extern "system" fn(Handle, *mut u32) -> Status;
pub type IntOut = unsafe extern "system" fn(Handle, *mut c_int) -> Status;
pub type StringOut = unsafe extern "system" fn(Handle, *mut c_char, u32) -> Status;
pub type BagCreate = unsafe extern "system" fn(*mut Handle) -> Status;
pub type BagSet = unsafe extern "system" fn(Handle, c_int, *const c_char, *const c_char) -> Status;
pub type BagGet =
    unsafe extern "system" fn(Handle, c_int, *const c_char, *const c_char) -> *const c_char;
pub type FreeString = unsafe extern "system" fn(*const c_char) -> Status;

/// C SDK buffers contain UTF-8 including a trailing NUL. Never truncate a result.
/// # Safety
/// A non-null `out` must be writable for `capacity` bytes.
pub unsafe fn copy_string(value: &str, out: *mut c_char, capacity: u32) -> Status {
    if out.is_null() {
        return INVALID_ARG;
    }
    if value.as_bytes().contains(&0) {
        return INVALID_ARG;
    }
    if value
        .len()
        .checked_add(1)
        .is_none_or(|n| n > capacity as usize)
    {
        return BUFFER_TOO_SMALL;
    }
    // SAFETY: caller owns the output buffer and its size has been checked above.
    unsafe {
        core::ptr::copy_nonoverlapping(value.as_ptr(), out.cast(), value.len());
        out.add(value.len()).write(0);
    }
    OK
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utf8_buffer_requires_room_for_nul_and_does_not_truncate() {
        let mut b = [0x55u8; 4];
        assert_eq!(
            unsafe { copy_string("你", b.as_mut_ptr().cast(), 3) },
            BUFFER_TOO_SMALL
        );
        assert_eq!(b, [0x55; 4]);
        assert_eq!(unsafe { copy_string("你", b.as_mut_ptr().cast(), 4) }, OK);
        assert_eq!(&b[..3], "你".as_bytes());
        assert_eq!(b[3], 0);
    }
}

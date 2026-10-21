//! CarbonDictation's Speech SDK boundary. Loading this library does not install it.
#[cfg(all(windows, not(target_pointer_width = "64")))]
compile_error!("the verified CarbonDictation ABI requires Windows x64");
pub mod abi;
mod hooks;
pub mod ipc;
mod pe;
#[cfg(windows)]
mod recover;
mod state;
#[cfg(windows)]
mod win;

/// Handles must already have been duplicated into the calling TextInputHost process.
/// Correct magic plus nonzero, non-INVALID, distinct handles transfer ownership to
/// this DLL on every result path (including install errors). Before that validation
/// the caller retains ownership. Never close an accepted remote handle twice.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InstallArgs {
    pub magic: u64,
    pub request_write: usize,
    pub event_read: usize,
}

pub const INSTALL_MAGIC: u64 = 0x5758_4842_5249_4447;

/// Installs only the checked CarbonDictation import slots; never locates another process.
///
/// # Safety
/// `args` must point to a readable InstallArgs. On successful installation the DLL owns
/// both supplied handles. The host must keep the broker alive for this process lifetime.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn WinhBridgeInstall(args: *const InstallArgs) -> u32 {
    if args.is_null() {
        return 1;
    }
    #[cfg(windows)]
    {
        // SAFETY: the caller supplies a valid InstallArgs as required by this ABI.
        unsafe { win::install(args.read()) }
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        abi::NOT_IMPL as u32
    }
}

/// Restores CarbonDictation's IAT only with no active session, pending callback,
/// or outstanding fake handle. Does not unload this DLL or terminate any process.
/// The host must stop the broker and close event-write to let event-read reach EOF.
#[unsafe(no_mangle)]
pub extern "system" fn WinhBridgeDisable(_: *const core::ffi::c_void) -> u32 {
    #[cfg(windows)]
    {
        unsafe { win::disable() }
    }
    #[cfg(not(windows))]
    {
        abi::NOT_IMPL as u32
    }
}

/// Read-only diagnostic history bits; see this crate's README for their meanings.
#[unsafe(no_mangle)]
pub extern "system" fn WinhBridgeDiagnostics(_: *const core::ffi::c_void) -> u32 {
    hooks::diagnostics()
}

/// No worker creation, pipe access, SDK calls or installation under the loader lock.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_: usize, _: u32, _: *mut core::ffi::c_void) -> i32 {
    1
}

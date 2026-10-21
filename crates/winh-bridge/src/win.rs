//! Installation runs only inside the already selected TextInputHost. No process APIs.
use crate::{INSTALL_MAGIC, InstallArgs, hooks, ipc, pe, state};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::{self, BufReader, Write};
use std::os::windows::{ffi::OsStringExt, io::FromRawHandle};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use windows_sys::Win32::Foundation::{HANDLE, HMODULE};
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
use windows_sys::Win32::System::Com::CoGetApartmentType;
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN, GetModuleFileNameW,
    GetModuleHandleExW, GetModuleHandleW, GetProcAddress,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_IMAGE, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualProtect, VirtualQuery,
};

#[link(name = "runtimeobject")]
unsafe extern "system" {
    fn RoInitialize(init: u32) -> i32;
    fn RoUninitialize();
}
struct RuntimeApartment;
impl Drop for RuntimeApartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}

// Stable bridge-specific install errors; not Speech SDK status values.
pub(crate) const BAD_ARGS: u32 = 1;
const BAD_PIPE: u32 = 2;
const ALREADY_INSTALLED: u32 = 3;
const MODULE_MISSING: u32 = 4;
const IDENTITY_MISMATCH: u32 = 5;
const IMPORT_MISMATCH: u32 = 6;
const PATCH_FAILED: u32 = 7;
const ROLLBACK_FAILED: u32 = 8;
const WORKER_FAILED: u32 = 9;
const BUSY: u32 = 10;

#[derive(Clone)]
struct Patch {
    slot: usize,
    original: usize,
    replacement: usize,
    protection: u32,
    name: String,
}
struct Installed {
    runtime: Arc<hooks::Runtime>,
    patches: Vec<Patch>,
    reader_done: Arc<AtomicBool>,
}
#[derive(Default)]
struct Installation {
    active: Option<Installed>,
    previous_reader: Option<Arc<AtomicBool>>,
    poisoned: bool,
}
static INSTALLATION: OnceLock<Mutex<Installation>> = OnceLock::new();

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

unsafe fn checked_module(name: &str, expected_hash: &str) -> Result<(HMODULE, Vec<u8>), u32> {
    let name = wide(name);
    let module = unsafe { GetModuleHandleW(name.as_ptr()) };
    if module.is_null() {
        return Err(MODULE_MISSING);
    }
    let mut path = vec![0u16; 32768];
    let count =
        unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32) } as usize;
    if count == 0 || count >= path.len() {
        return Err(IDENTITY_MISMATCH);
    }
    let path = std::path::PathBuf::from(OsString::from_wide(&path[..count]));
    let metadata = std::fs::metadata(&path).map_err(|_| IDENTITY_MISMATCH)?;
    if metadata.len() > 32 * 1024 * 1024 {
        return Err(IDENTITY_MISMATCH);
    }
    let bytes = std::fs::read(path).map_err(|_| IDENTITY_MISMATCH)?;
    if format!("{:x}", Sha256::digest(&bytes)) != expected_hash {
        return Err(IDENTITY_MISMATCH);
    }
    Ok((module, bytes))
}

unsafe fn export(core: HMODULE, name: &str) -> Result<usize, u32> {
    let name = CString::new(name).map_err(|_| IMPORT_MISMATCH)?;
    unsafe { GetProcAddress(core, name.as_ptr().cast()) }
        .map(|f| f as *const () as usize)
        .ok_or(IMPORT_MISMATCH)
}

unsafe fn plan(
    carbon: HMODULE,
    core: HMODULE,
    bytes: &[u8],
) -> Result<(Vec<Patch>, HashMap<String, usize>), u32> {
    let image = pe::imports(bytes).map_err(|_| IMPORT_MISMATCH)?;
    let base = carbon as usize;
    // Check the loaded header as well as the file, before touching any IAT slot.
    let loaded = unsafe { core::slice::from_raw_parts(base as *const u8, 4096) };
    let nt = u32::from_le_bytes(
        loaded[0x3c..0x40]
            .try_into()
            .map_err(|_| IDENTITY_MISMATCH)?,
    ) as usize;
    if nt > 4096 - 84 || &loaded[nt..nt + 4] != b"PE\0\0" {
        return Err(IDENTITY_MISMATCH);
    }
    let loaded_size = u32::from_le_bytes(
        loaded[nt + 80..nt + 84]
            .try_into()
            .map_err(|_| IDENTITY_MISMATCH)?,
    ) as usize;
    if loaded_size != image.size {
        return Err(IDENTITY_MISMATCH);
    }
    let mut patches = Vec::new();
    let mut originals = HashMap::new();
    for import in image.imports {
        let slot = base.checked_add(import.iat_rva).ok_or(IMPORT_MISMATCH)?;
        let mut info = MEMORY_BASIC_INFORMATION::default();
        if unsafe { VirtualQuery(slot as *const _, &mut info, core::mem::size_of_val(&info)) } == 0
            || info.State != MEM_COMMIT
            || info.Type != MEM_IMAGE
            || info.AllocationBase != carbon
            || slot
                .checked_add(8)
                .is_none_or(|end| end > info.BaseAddress as usize + info.RegionSize)
        {
            return Err(IMPORT_MISMATCH);
        }
        let original = unsafe { export(core, &import.name) }?;
        // Refuse pre-existing hooks and unresolved/bound import surprises. This checks
        // ALL Speech imports, including those deliberately left untouched.
        let observed = unsafe { (*(slot as *const AtomicUsize)).load(Ordering::Acquire) };
        let delay_stub = import.delay_stub_rva.and_then(|rva| base.checked_add(rva));
        if observed != original && Some(observed) != delay_stub {
            return Err(IMPORT_MISMATCH);
        }
        originals.insert(import.name.clone(), original);
        if let Some(replacement) = hooks::address(&import.name) {
            patches.push(Patch {
                slot,
                original: observed,
                replacement,
                protection: info.Protect,
                name: import.name,
            });
        }
    }
    for helper in [
        "property_bag_create",
        "property_bag_set_string",
        "property_bag_release",
        "recognizer_get_property_bag",
        "property_bag_get_string",
        "property_bag_free_string",
    ] {
        originals.insert(helper.to_owned(), unsafe { export(core, helper) }?);
    }
    // Block recognition starts first during install; restore them last on disable.
    patches.sort_by_key(|p| {
        if hooks::is_start_or_stop(&p.name) {
            0
        } else {
            1
        }
    });
    Ok((patches, originals))
}

/// Swap exactly one aligned IAT pointer. Restore the original page protection even
/// when compare_exchange detects a concurrent owner. Return whether we wrote it.
unsafe fn swap_slot(patch: &Patch, install: bool) -> Result<(), (u32, bool)> {
    let (expected, replacement) = if install {
        (patch.original, patch.replacement)
    } else {
        (patch.replacement, patch.original)
    };
    let mut old = 0;
    if unsafe {
        VirtualProtect(
            patch.slot as *const _,
            core::mem::size_of::<usize>(),
            PAGE_READWRITE,
            &mut old,
        )
    } == 0
    {
        return Err((PATCH_FAILED, false));
    }
    let changed = unsafe {
        (*(patch.slot as *const AtomicUsize)).compare_exchange(
            expected,
            replacement,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
    }
    .is_ok();
    let mut ignored = 0;
    let restored = unsafe {
        VirtualProtect(
            patch.slot as *const _,
            core::mem::size_of::<usize>(),
            patch.protection,
            &mut ignored,
        )
    } != 0;
    if !restored || !changed {
        return Err((PATCH_FAILED, changed));
    }
    Ok(())
}

unsafe fn swap_all(patches: &[Patch], install: bool) -> Result<(), u32> {
    let order = if install {
        patches.iter().collect::<Vec<_>>()
    } else {
        patches.iter().rev().collect()
    };
    let mut changed = Vec::new();
    for patch in order {
        match unsafe { swap_slot(patch, install) } {
            Ok(()) => changed.push(patch),
            Err((error, wrote)) => {
                if wrote {
                    changed.push(patch);
                }
                let mut rollback_ok = true;
                for previous in changed.into_iter().rev() {
                    if unsafe { swap_slot(previous, !install) }.is_err() {
                        rollback_ok = false;
                    }
                }
                return Err(if rollback_ok { error } else { ROLLBACK_FAILED });
            }
        }
    }
    Ok(())
}

fn spawn_workers(
    engine: &Arc<state::Engine>,
    requests: mpsc::Receiver<ipc::BridgeRequest>,
    jobs: mpsc::Receiver<state::Job>,
    mut request_file: File,
    event_file: File,
    reader_done: Arc<AtomicBool>,
) -> io::Result<()> {
    let weak = Arc::downgrade(engine);
    let (initialized, ready) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("winh-callbacks".into())
        .spawn(move || {
            let (mut apartment, mut qualifier) = (0, 0);
            if unsafe { CoGetApartmentType(&mut apartment, &mut qualifier) } < 0 {
                hooks::diagnostic_or(1 << 14);
            }
            // Carbon's callbacks consume WinRT text sessions. Their thread must
            // own a WinRT apartment, just as SDK worker threads normally do.
            let status = unsafe { RoInitialize(1) }; // RO_INIT_MULTITHREADED
            let _ = initialized.send(status);
            if status < 0 {
                hooks::diagnostic_or(1 << 16);
                return;
            }
            hooks::diagnostic_or(1 << 15);
            let _apartment = RuntimeApartment;
            while let Ok(job) = jobs.recv() {
                let Some(engine) = weak.upgrade() else {
                    break;
                };
                engine.dispatch(job);
            }
        })?;
    if !matches!(ready.recv_timeout(std::time::Duration::from_secs(2)), Ok(status) if status >= 0) {
        return Err(io::Error::other("callback WinRT initialization failed"));
    }
    let weak = Arc::downgrade(engine);
    std::thread::Builder::new()
        .name("winh-requests".into())
        .spawn(move || {
            while let Ok(request) = requests.recv() {
                let result = serde_json::to_writer(&mut request_file, &request)
                    .map_err(io::Error::other)
                    .and_then(|_| request_file.write_all(b"\n"));
                if result.is_err() {
                    if let Some(engine) = weak.upgrade() {
                        engine.disconnected("bridge request pipe closed");
                    }
                    break;
                }
            }
        })?;
    let weak = Arc::downgrade(engine);
    std::thread::Builder::new()
        .name("winh-events".into())
        .spawn(move || {
            let mut reader = BufReader::with_capacity(8192, event_file);
            loop {
                match ipc::read_event(&mut reader) {
                    Ok(Some(event)) => {
                        let Some(engine) = weak.upgrade() else {
                            break;
                        };
                        engine.receive(event);
                    }
                    Ok(None) | Err(_) => {
                        if let Some(engine) = weak.upgrade() {
                            engine.disconnected("bridge event pipe closed or invalid");
                        }
                        break;
                    }
                }
            }
            reader_done.store(true, Ordering::Release);
        })?;
    Ok(())
}

/// After syntactic magic/handle checks, ownership transfers on EVERY result path.
pub(crate) unsafe fn install(args: InstallArgs) -> u32 {
    if args.magic != INSTALL_MAGIC
        || args.request_write == 0
        || args.event_read == 0
        || args.request_write == usize::MAX
        || args.event_read == usize::MAX
        || args.request_write == args.event_read
    {
        return BAD_ARGS;
    }
    // SAFETY: the host transferred these process-local, distinct owning handles.
    let request_file = unsafe { File::from_raw_handle(args.request_write as HANDLE) };
    let event_file = unsafe { File::from_raw_handle(args.event_read as HANDLE) };
    if unsafe { GetFileType(args.request_write as HANDLE) } != FILE_TYPE_PIPE
        || unsafe { GetFileType(args.event_read as HANDLE) } != FILE_TYPE_PIPE
    {
        return BAD_PIPE;
    }
    let mut installation =
        state::lock(INSTALLATION.get_or_init(|| Mutex::new(Installation::default())));
    if installation.poisoned {
        return ROLLBACK_FAILED;
    }
    if installation.active.is_some() {
        return ALREADY_INSTALLED;
    }
    if installation
        .previous_reader
        .as_ref()
        .is_some_and(|done| !done.load(Ordering::Acquire))
    {
        return BUSY;
    }
    let result = (|| -> Result<(), u32> {
        let (carbon, carbon_bytes) =
            unsafe { checked_module("TextInput.CarbonDictation.dll", pe::CARBON_HASH) }?;
        let (core, _) = unsafe { checked_module(pe::CORE_NAME, pe::CORE_HASH) }?;
        let (patches, originals) = unsafe { plan(carbon, core, &carbon_bytes) }?;
        // Hooks and worker entrypoints must survive even if the loader's client
        // drops its reference. Disable restores imports but never unloads this DLL.
        let mut pinned = core::ptr::null_mut();
        if unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                crate::WinhBridgeInstall as *const () as *const u16,
                &mut pinned,
            )
        } == 0
        {
            return Err(MODULE_MISSING);
        }
        for name in ["TextInput.CarbonDictation.dll", pe::CORE_NAME] {
            if unsafe {
                GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_PIN,
                    wide(name).as_ptr(),
                    &mut pinned,
                )
            } == 0
            {
                return Err(MODULE_MISSING);
            }
        }
        let (engine, requests, jobs) = state::Engine::new();
        hooks::reset_diagnostics();
        let runtime = Arc::new(hooks::Runtime {
            core_base: core as usize,
            engine: engine.clone(),
            originals,
            ready: AtomicBool::new(false),
        });
        hooks::publish(Some(runtime.clone()));
        if let Err(error) = unsafe { swap_all(&patches, true) } {
            // Keep originals available after an exceptional rollback failure, so
            // any remaining getter hooks still reject fake handles safely.
            if error != ROLLBACK_FAILED {
                hooks::publish(None);
            } else {
                installation.poisoned = true;
            }
            return Err(error);
        }
        let reader_done = Arc::new(AtomicBool::new(false));
        if spawn_workers(
            &engine,
            requests,
            jobs,
            request_file,
            event_file,
            reader_done.clone(),
        )
        .is_err()
        {
            let rollback = unsafe { swap_all(&patches, false) };
            if rollback.is_ok() {
                hooks::publish(None);
            } else {
                installation.poisoned = true;
            }
            return Err(if rollback.is_ok() {
                WORKER_FAILED
            } else {
                ROLLBACK_FAILED
            });
        }
        runtime.ready.store(true, Ordering::Release);
        installation.active = Some(Installed {
            runtime,
            patches,
            reader_done,
        });
        installation.previous_reader = None;
        Ok(())
    })();
    result.err().unwrap_or(0)
}

pub(crate) unsafe fn disable() -> u32 {
    let mut installation =
        state::lock(INSTALLATION.get_or_init(|| Mutex::new(Installation::default())));
    if installation.poisoned {
        return ROLLBACK_FAILED;
    }
    let Some(installed) = installation.active.as_ref() else {
        return 0;
    };
    if !installed.runtime.engine.suspend_if_idle() {
        return BUSY;
    }
    installed.runtime.ready.store(false, Ordering::Release);
    if let Err(error) = unsafe { swap_all(&installed.patches, false) } {
        if error != ROLLBACK_FAILED {
            installed.runtime.engine.resume();
            installed.runtime.ready.store(true, Ordering::Release);
        } else {
            installation.poisoned = true;
        }
        return error;
    }
    hooks::publish(None);
    let installed = installation
        .active
        .take()
        .expect("installation remains serialized");
    installation.previous_reader = Some(installed.reader_done);
    // Drop the runtime/request sender: request-read sees EOF after writer exits.
    // Host closes event-write before Disable; reader observes EOF and exits too.
    drop(installed.runtime);
    0
}

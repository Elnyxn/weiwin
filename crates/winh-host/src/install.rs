//! Target-allowlisted adapter loading. No client injection or process termination APIs.
use sha2::{Digest, Sha256};
use std::{
    ffi::{CString, c_void},
    fs::File,
    mem,
    os::windows::io::FromRawHandle,
    path::{Path, PathBuf},
    ptr,
};
use wechat_winh_bridge::{INSTALL_MAGIC, InstallArgs};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, FreeLibrary, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    },
    System::{
        Diagnostics::{
            Debug::WriteProcessMemory,
            ToolHelp::{
                CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW,
                PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPMODULE,
                TH32CS_SNAPPROCESS,
            },
        },
        LibraryLoader::{
            DONT_RESOLVE_DLL_REFERENCES, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW, GetModuleHandleExW,
            GetModuleHandleW, GetProcAddress, LoadLibraryExW,
        },
        Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
        },
        Pipes::CreatePipe,
        Threading::{
            CreateRemoteThread, GetCurrentProcess, GetExitCodeThread, OpenProcess,
            PROCESS_CREATE_THREAD, PROCESS_DUP_HANDLE, PROCESS_QUERY_INFORMATION,
            PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE, QueryFullProcessImageNameW,
            WaitForSingleObject,
        },
    },
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
unsafe impl Send for Handle {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn text(b: &[u16]) -> String {
    String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())])
}

fn processes() -> Result<Vec<(u32, String)>> {
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    let snapshot = Handle(raw);
    let mut e: PROCESSENTRY32W = unsafe { mem::zeroed() };
    e.dwSize = mem::size_of_val(&e) as _;
    let mut next = unsafe { Process32FirstW(snapshot.0, &mut e) };
    let mut result = Vec::new();
    while next != 0 && result.len() < 4096 {
        result.push((e.th32ProcessID, text(&e.szExeFile)));
        next = unsafe { Process32NextW(snapshot.0, &mut e) };
    }
    Ok(result)
}

fn module(pid: u32, name: &str) -> Result<(usize, PathBuf)> {
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    let snapshot = Handle(raw);
    let mut e: MODULEENTRY32W = unsafe { mem::zeroed() };
    e.dwSize = mem::size_of_val(&e) as _;
    let mut next = unsafe { Module32FirstW(snapshot.0, &mut e) };
    for _ in 0..512 {
        if next == 0 {
            break;
        }
        if text(&e.szModule).eq_ignore_ascii_case(name) {
            return Ok((e.modBaseAddr as usize, text(&e.szExePath).into()));
        }
        next = unsafe { Module32NextW(snapshot.0, &mut e) };
    }
    Err(format!("模块尚未加载：{name}").into())
}

pub fn wechat_pid() -> Result<u32> {
    let mut candidates = processes()?
        .into_iter()
        .filter(|(_, name)| name.eq_ignore_ascii_case("Weixin.exe"))
        .filter_map(|(pid, _)| module(pid, "ilink_stream.dll").ok().map(|_| pid));
    let pid = candidates.next().ok_or("请先手动打开并登录微信")?;
    if candidates.next().is_some() {
        return Err("检测到多个微信账号，请关闭多余实例后重试".into());
    }
    Ok(pid)
}

fn remote_call(process: HANDLE, function: usize, argument: usize) -> Result<u32> {
    let raw = unsafe {
        CreateRemoteThread(
            process,
            ptr::null(),
            0,
            Some(mem::transmute::<
                usize,
                unsafe extern "system" fn(*mut c_void) -> u32,
            >(function)),
            argument as _,
            0,
            ptr::null_mut(),
        )
    };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let thread = Handle(raw);
    // Never terminate a thread or its containing process on timeout.
    if unsafe { WaitForSingleObject(thread.0, 10_000) } != WAIT_OBJECT_0 {
        return Err("适配器调用超时；已保留在途资源，不结束任何进程".into());
    }
    let mut code = 0;
    if unsafe { GetExitCodeThread(thread.0, &mut code) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(code)
}

fn upload(process: HANDLE, bytes: &[u8]) -> Result<usize> {
    let address = unsafe {
        VirtualAllocEx(
            process,
            ptr::null(),
            bytes.len(),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if address.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut count = 0;
    if unsafe {
        WriteProcessMemory(
            process,
            address,
            bytes.as_ptr().cast(),
            bytes.len(),
            &mut count,
        )
    } == 0
        || count != bytes.len()
    {
        unsafe {
            VirtualFreeEx(process, address, 0, MEM_RELEASE);
        }
        return Err("无法传递适配器参数".into());
    }
    Ok(address as usize)
}

fn local_export(library: windows_sys::Win32::Foundation::HMODULE, name: &str) -> Result<usize> {
    let name = CString::new(name)?;
    unsafe { GetProcAddress(library, name.as_ptr().cast()) }
        .map(|f| f as usize)
        .ok_or_else(|| "适配器导出缺失".into())
}

fn loader_address(pid: u32) -> Result<usize> {
    let kernel = unsafe { GetModuleHandleW(wide("kernel32.dll").as_ptr()) };
    let function = local_export(kernel, "LoadLibraryW")?;
    let mut owner = ptr::null_mut();
    if unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            function as _,
            &mut owner,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut path = [0_u16; 1024];
    let n = unsafe { GetModuleFileNameW(owner, path.as_mut_ptr(), path.len() as _) } as usize;
    let path = PathBuf::from(String::from_utf16_lossy(&path[..n]));
    let name = path
        .file_name()
        .ok_or("加载器路径不可用")?
        .to_string_lossy();
    Ok(module(pid, &name)?.0 + function - owner as usize)
}

fn pipes() -> Result<(Handle, Handle)> {
    let (mut read, mut write) = (ptr::null_mut(), ptr::null_mut());
    if unsafe { CreatePipe(&mut read, &mut write, ptr::null(), 65_536) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((Handle(read), Handle(write)))
}

fn duplicate_to(process: HANDLE, handle: HANDLE) -> Result<usize> {
    let mut remote = ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            process,
            &mut remote,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(remote as usize)
}

fn verify_file(path: &Path, expected: &str) -> Result<()> {
    if std::fs::metadata(path)?.len() > 32 * 1024 * 1024 {
        return Err("系统模块超过预期大小".into());
    }
    let mut actual = String::with_capacity(64);
    for byte in Sha256::digest(std::fs::read(path)?) {
        use std::fmt::Write;
        write!(&mut actual, "{byte:02x}")?;
    }
    if actual != expected {
        return Err("当前系统版本不在已验证范围内".into());
    }
    Ok(())
}

fn load_checked(process: HANDLE, pid: u32, name: &str, path: &str) -> Result<()> {
    if module(pid, name).is_ok() {
        return Ok(());
    }
    let path = wide(path);
    let bytes = unsafe { std::slice::from_raw_parts(path.as_ptr().cast::<u8>(), path.len() * 2) };
    let argument = upload(process, bytes)?;
    remote_call(process, loader_address(pid)?, argument)?;
    unsafe {
        VirtualFreeEx(process, argument as _, 0, MEM_RELEASE);
    }
    module(pid, name).map_err(|_| format!("系统模块未能初始化：{name}"))?;
    Ok(())
}

pub struct Adapter {
    pub pid: u32,
    process: Handle,
    pub requests: File,
    pub events: Option<File>,
    disable: usize,
    diagnostics: usize,
}

impl Adapter {
    pub fn install(dll: &Path) -> Result<Self> {
        let mut targets = processes()?
            .into_iter()
            .filter(|(_, name)| name.eq_ignore_ascii_case("TextInputHost.exe"));
        let pid = targets.next().ok_or("请先按一次 Win+H，随后重新运行")?.0;
        if targets.next().is_some() {
            return Err("存在多个 TextInputHost，无法安全选择".into());
        }
        let raw = unsafe {
            OpenProcess(
                PROCESS_CREATE_THREAD
                    | PROCESS_DUP_HANDLE
                    | PROCESS_QUERY_INFORMATION
                    | PROCESS_VM_OPERATION
                    | PROCESS_VM_WRITE
                    | PROCESS_VM_READ,
                0,
                pid,
            )
        };
        if raw.is_null() {
            return Err(format!(
                "无法取得 TextInputHost 接入权限：{}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        let process = Handle(raw);
        let mut path = [0_u16; 1024];
        let mut size = path.len() as u32;
        if unsafe { QueryFullProcessImageNameW(raw, 0, path.as_mut_ptr(), &mut size) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let image = String::from_utf16_lossy(&path[..size as usize]);
        if !image.eq_ignore_ascii_case(
            r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\TextInputHost.exe",
        ) {
            return Err("TextInputHost 路径与受支持系统不符".into());
        }
        verify_file(
            Path::new(&image),
            "ab4c670a7f05ff99ae7615eec97de78e2b520427bc30f420b7d90a7a85bcee44",
        )?;
        // Initialize the known system boundary before first Win+H activation. This
        // loads existing signed OS modules, without starting recognition or UI.
        let system_modules = [
            (
                "MSVCP140_CODECVT_IDS_APP.dll",
                r"C:\Windows\SystemApps\MicrosoftWindows.Client.CoreAI_cw5n1h2txyewy\MSVCP140_CODECVT_IDS_APP.dll",
                "b9a43374c780b58f56575b4af88eac241089b32df2a8d9458d12ac2f1a68b9e2",
            ),
            (
                "Microsoft.CognitiveServices.Speech.core.dll",
                r"C:\Windows\SystemApps\SxS\MicrosoftWindows.61869836.InpApp_cw5n1h2txyewy\TextInput\Microsoft.CognitiveServices.Speech.core.dll",
                "00b8c930a414ab37148beb359adbcecf828ef7e9dd4452dc1dde378a6a5fd3bd",
            ),
            (
                "TextInput.CarbonDictation.dll",
                r"C:\Windows\SystemApps\SxS\MicrosoftWindows.61869836.InpApp_cw5n1h2txyewy\TextInput.CarbonDictation.dll",
                "b6188de140529f87f7fb8c38e6f43c44ed778f310d069d02332371e2c446433b",
            ),
        ];
        for (_, path, hash) in &system_modules {
            verify_file(Path::new(path), hash)?;
        }
        for (name, path, _) in &system_modules {
            load_checked(raw, pid, name, path)?;
        }
        let dll = dll.canonicalize()?;
        let dll_path = wide(&dll.to_string_lossy());
        let local = unsafe {
            LoadLibraryExW(
                dll_path.as_ptr(),
                ptr::null_mut(),
                DONT_RESOLVE_DLL_REFERENCES,
            )
        };
        if local.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let install_rva = local_export(local, "WinhBridgeInstall")? - local as usize;
        let disable_rva = local_export(local, "WinhBridgeDisable")? - local as usize;
        let diagnostics_rva = local_export(local, "WinhBridgeDiagnostics")? - local as usize;
        unsafe {
            FreeLibrary(local);
        }
        let name = dll.file_name().ok_or("无效DLL路径")?.to_string_lossy();
        let base = match module(pid, &name) {
            Ok((base, remote_path)) => {
                if remote_path.canonicalize()? != dll {
                    return Err("已加载其他路径的同名适配器".into());
                }
                base
            }
            Err(_) => {
                let bytes = unsafe {
                    std::slice::from_raw_parts(dll_path.as_ptr().cast::<u8>(), dll_path.len() * 2)
                };
                let argument = upload(raw, bytes)?;
                remote_call(raw, loader_address(pid)?, argument)?;
                unsafe {
                    VirtualFreeEx(raw, argument as _, 0, MEM_RELEASE);
                }
                module(pid, &name)?.0
            }
        };
        let (requests, request_write) = pipes()?;
        let (event_read, events) = pipes()?;
        let args = InstallArgs {
            magic: INSTALL_MAGIC,
            request_write: duplicate_to(raw, request_write.0)?,
            event_read: duplicate_to(raw, event_read.0)?,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&args as *const InstallArgs).cast(),
                mem::size_of_val(&args),
            )
        };
        let argument = upload(raw, bytes)?;
        let code = remote_call(raw, base + install_rva, argument)?;
        unsafe {
            VirtualFreeEx(raw, argument as _, 0, MEM_RELEASE);
        }
        if code != 0 {
            return Err(format!("适配器未启用（{code}）").into());
        }
        // Drop the local duplicates of the remote endpoints, so EOF remains meaningful.
        drop(request_write);
        drop(event_read);
        let request_raw = requests.0;
        mem::forget(requests);
        let event_raw = events.0;
        mem::forget(events);
        let requests = unsafe { File::from_raw_handle(request_raw) };
        let events = unsafe { File::from_raw_handle(event_raw) };
        Ok(Self {
            pid,
            process,
            requests,
            events: Some(events),
            disable: base + disable_rva,
            diagnostics: base + diagnostics_rva,
        })
    }

    pub fn disable(&mut self) -> Result<()> {
        drop(self.events.take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let code = remote_call(self.process.0, self.disable, 0)?;
            if code == 0 {
                return Ok(());
            }
            if code != 10 || std::time::Instant::now() >= deadline {
                return Err(format!("适配器尚未退出（{code}）").into());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    pub fn diagnostics(&self) -> Result<u32> {
        remote_call(self.process.0, self.diagnostics, 0)
    }
}

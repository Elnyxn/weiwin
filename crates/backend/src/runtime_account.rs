//! Bounded, read-only access to an already initialized SDK session.
//! No debug attachment, target function calls, writes, remote threads, or termination.
use crate::Result;
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, mem, path::PathBuf};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::{
            Debug::ReadProcessMemory,
            ToolHelp::{
                CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW,
                TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
            },
        },
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ},
    },
};

const STREAM_HASH: &str = "860658c95745bff7fd3732c943d5866f05ca620d341512c26f50334f9f2943da";
const WRAPPER_HASH: &str = "345233f74ffc490d7dc406e3c17d41e79ba7fa808632d05a2369923dc64e166f";

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // Closing a read handle releases our reference; it cannot stop the process.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct Module {
    base: usize,
    path: PathBuf,
}

fn wide(value: &[u16]) -> String {
    String::from_utf16_lossy(&value[..value.iter().position(|&c| c == 0).unwrap_or(value.len())])
}

fn modules(pid: u32) -> Result<(Module, Module)> {
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    let snapshot = Handle(raw);
    let mut entry: MODULEENTRY32W = unsafe { mem::zeroed() };
    entry.dwSize = mem::size_of::<MODULEENTRY32W>() as u32;
    let mut ok = unsafe { Module32FirstW(snapshot.0, &mut entry) };
    let (mut stream, mut wrapper) = (None, None);
    let mut count = 0;
    while ok != 0 && count < 512 {
        count += 1;
        let name = wide(&entry.szModule);
        if name.eq_ignore_ascii_case("ilink_stream.dll") {
            stream = Some(Module {
                base: entry.modBaseAddr as usize,
                path: wide(&entry.szExePath).into(),
            });
        } else if name.eq_ignore_ascii_case("ilink_wrapper.dll") {
            wrapper = Some(Module {
                base: entry.modBaseAddr as usize,
                path: wide(&entry.szExePath).into(),
            });
        }
        if stream.is_some() && wrapper.is_some() {
            break;
        }
        ok = unsafe { Module32NextW(snapshot.0, &mut entry) };
    }
    Ok((
        stream.ok_or("stream SDK is not loaded")?,
        wrapper.ok_or("stream wrapper is not loaded")?,
    ))
}

fn verify(module: &Module, expected: &str) -> Result<()> {
    let mut file = File::open(&module.path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 16_384];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    let mut actual = String::with_capacity(64);
    for byte in digest.finalize() {
        use std::fmt::Write;
        write!(&mut actual, "{byte:02x}")?;
    }
    if actual != expected {
        return Err("unsupported SDK version".into());
    }
    Ok(())
}

struct Reader(Handle);
impl Reader {
    fn read(&self, address: usize, size: usize) -> Result<Vec<u8>> {
        if !(0x10000..0x8000_0000_0000).contains(&address) || size > 4096 {
            return Err("session address or size is invalid".into());
        }
        let mut bytes = vec![0; size];
        let mut count = 0;
        let ok = unsafe {
            ReadProcessMemory(
                self.0.0,
                address as _,
                bytes.as_mut_ptr().cast(),
                size,
                &mut count,
            )
        };
        if ok == 0 || count != size {
            return Err("session memory is unavailable".into());
        }
        Ok(bytes)
    }
    fn pointer(&self, address: usize) -> Result<usize> {
        Ok(usize::from_le_bytes(
            self.read(address, 8)?.try_into().unwrap(),
        ))
    }
    fn string(&self, object: usize) -> Result<String> {
        let bytes = self.read(object, 32)?;
        let size = usize::from_le_bytes(bytes[16..24].try_into().unwrap());
        let capacity = usize::from_le_bytes(bytes[24..32].try_into().unwrap());
        if size > 2048 || capacity < size || capacity > (1 << 20) {
            return Err("SDK string layout is unsupported".into());
        }
        if size == 0 {
            return Ok(String::new());
        }
        let data = if capacity > 15 {
            self.read(usize::from_le_bytes(bytes[..8].try_into().unwrap()), size)?
        } else {
            bytes[..size].to_vec()
        };
        Ok(String::from_utf8(data)?)
    }
    fn string_field(&self, message: usize, offset: usize) -> Result<String> {
        self.string(self.pointer(message + offset)?)
    }
}

pub struct RuntimeAccount {
    pub app_uin: u64,
    pub wss_token: String,
    pub device_id: String,
    pub app_id: String,
    pub client_version: u32,
    pub context_type: u32,
    pub session_fields: u32,
    pub server_group_key: u32,
}

impl RuntimeAccount {
    pub fn auth_type(&self) -> &'static str {
        // Exact branch from IlinkStreamChannelImpl constructor SDK+0xee50.
        if self.app_id == "wechat" {
            "wx_jwt_token"
        } else {
            "ilink_jwt_token"
        }
    }
}

pub fn read(pid: u32) -> Result<RuntimeAccount> {
    let (stream, wrapper) = modules(pid)?;
    verify(&stream, STREAM_HASH)?;
    verify(&wrapper, WRAPPER_HASH)?;
    let raw = unsafe { OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let reader = Reader(Handle(raw));
    let api = reader.pointer(wrapper.base + 0x37b30)?;
    if reader.pointer(api)? != wrapper.base + 0x21c30 {
        return Err("stream API is not initialized".into());
    }
    let public = reader.pointer(api + 8)?;
    let base = reader.pointer(public + 0x10)?;
    let holder = reader.pointer(base + 8)?;
    let mut context = reader.pointer(holder + 0x28)?;
    let mut context_vtable = reader.pointer(context)?;
    if context_vtable == stream.base + 0xcabd0 {
        // DispatcherBridgeCppImpl::ConfigContext at SDK+0x6a4e0 delegates
        // directly through the native implementation pointer at +0x18.
        context = reader.pointer(context + 0x18)?;
        context_vtable = reader.pointer(context)?;
    }
    if context_vtable != stream.base + 0xc3468 {
        let relative = context_vtable
            .checked_sub(stream.base)
            .ok_or("context belongs to another module")?;
        return Err(
            format!("context interface vtable RVA={relative:#x} is not yet supported").into(),
        );
    }
    let session = context + 0x170;
    if reader.pointer(session)? != stream.base + 0xc2d70 {
        return Err("session configuration layout is unsupported".into());
    }
    let config = context + 0x1c0;
    let account = RuntimeAccount {
        app_uin: reader.pointer(session + 40)? as u64,
        wss_token: reader.string_field(session, 56)?,
        device_id: reader.string_field(session, 32)?,
        app_id: reader.string_field(session, 48)?,
        client_version: u32::from_le_bytes(reader.read(session + 72, 4)?.try_into().unwrap()),
        context_type: u32::from_le_bytes(reader.read(config + 40, 4)?.try_into().unwrap()),
        session_fields: u32::from_le_bytes(reader.read(session + 12, 4)?.try_into().unwrap()),
        server_group_key: u32::from_le_bytes(reader.read(session + 76, 4)?.try_into().unwrap()),
    };
    if account.app_uin == 0 || account.wss_token.is_empty() {
        return Err("speech session credentials are not ready".into());
    }
    Ok(account)
}

//! The local computer name reported by the operating system.
use crate::Result;

#[cfg(windows)]
pub(super) fn name() -> Result<String> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNamePhysicalDnsHostname, GetComputerNameExW,
    };
    let mut buffer = [0u16; 256];
    let mut length = buffer.len() as u32;
    if unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            buffer.as_mut_ptr(),
            &mut length,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let name = String::from_utf16(&buffer[..length as usize])?;
    if name.is_empty() {
        return Err("system computer name is empty".into());
    }
    Ok(name)
}

#[cfg(not(windows))]
pub(super) fn name() -> Result<String> {
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname")?
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    if name.is_empty() {
        return Err("system computer name is empty".into());
    }
    Ok(name)
}

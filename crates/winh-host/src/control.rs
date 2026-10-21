//! User-session lifecycle for our own broker. Stop signals an event; never kills a process.
use crate::install::Result;
use std::{
    fs::{self, File},
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    ptr,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0},
    System::Threading::{
        CREATE_NO_WINDOW, CreateEventW, EVENT_MODIFY_STATE, OpenEventW, ResetEvent,
        SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
    },
};

const STOP: &str = "Local\\WeChatWinH.Stop";
const READY: &str = "Local\\WeChatWinH.Ready";
const DONE: &str = "Local\\WeChatWinH.Restored";
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
struct Event(HANDLE);
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn open(name: &str, modify: bool) -> Option<Event> {
    let raw = unsafe {
        OpenEventW(
            SYNCHRONIZATION_SYNCHRONIZE | if modify { EVENT_MODIFY_STATE } else { 0 },
            0,
            wide(name).as_ptr(),
        )
    };
    (!raw.is_null()).then_some(Event(raw))
}

pub struct Control {
    stop: Event,
    ready: Event,
    done: Event,
}
impl Control {
    pub fn new() -> Result<Self> {
        let raw = unsafe { CreateEventW(ptr::null(), 1, 0, wide(STOP).as_ptr()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let stop = Event(raw);
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Err("服务已在运行".into());
        }
        let raw = unsafe { CreateEventW(ptr::null(), 1, 0, wide(READY).as_ptr()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        unsafe {
            ResetEvent(raw);
        }
        let ready = Event(raw);
        let raw = unsafe { CreateEventW(ptr::null(), 1, 0, wide(DONE).as_ptr()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        unsafe {
            ResetEvent(raw);
        }
        Ok(Self {
            stop,
            ready,
            done: Event(raw),
        })
    }
    pub fn ready(&self) {
        unsafe {
            SetEvent(self.ready.0);
        }
    }
    pub fn stopping(&self) {
        unsafe {
            ResetEvent(self.ready.0);
        }
    }
    pub fn should_stop(&self) -> bool {
        unsafe { WaitForSingleObject(self.stop.0, 0) == WAIT_OBJECT_0 }
    }
    pub fn completed(&self) {
        unsafe {
            SetEvent(self.done.0);
        }
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        self.stopping();
    }
}

pub fn command() -> Result<bool> {
    let mut arguments = std::env::args_os().skip(1);
    match arguments.next().as_deref().and_then(|s| s.to_str()) {
        Some("stop") => {
            if let Some(event) = open(STOP, true) {
                let done = open(DONE, false).ok_or("服务正在启动，请稍后再退出")?;
                unsafe {
                    SetEvent(event.0);
                }
                if unsafe { WaitForSingleObject(done.0, 12_000) } != WAIT_OBJECT_0 {
                    return Err("恢复尚未确认；未结束任何进程，请查看service.log".into());
                }
                println!("已停止并恢复原生识别。");
            } else {
                println!("服务未运行。");
            }
            Ok(true)
        }
        Some("status") => {
            let ready = open(READY, false)
                .is_some_and(|event| unsafe { WaitForSingleObject(event.0, 0) == WAIT_OBJECT_0 });
            if ready {
                println!("已启用{}。", crate::status::active_backend_label());
            } else {
                println!("服务未就绪。");
            }
            Ok(true)
        }
        Some("start") => {
            if open(STOP, false).is_some() {
                return Err("服务已在运行".into());
            }
            let root = std::path::PathBuf::from(
                std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA不可用")?,
            )
            .join("WeChatWinH");
            fs::create_dir_all(&root)?;
            let log = File::options()
                .create(true)
                .append(true)
                .open(root.join("service.log"))?;
            let mut child = Command::new(std::env::current_exe()?)
                .arg("run")
                .args(arguments)
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()?;
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if let Some(code) = child.try_wait()? {
                    return Err(format!(
                        "启动失败（{code}），请查看 {}",
                        root.join("service.log").display()
                    )
                    .into());
                }
                if open(READY, false).is_some_and(|event| unsafe {
                    WaitForSingleObject(event.0, 0) == WAIT_OBJECT_0
                }) {
                    println!(
                        "已启用；Win+H 使用{}。退出：wechat-winh.exe stop",
                        crate::status::active_backend_label()
                    );
                    return Ok(true);
                }
                if Instant::now() >= deadline {
                    return Err("启动尚未就绪，未结束任何进程".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        _ => Ok(false),
    }
}

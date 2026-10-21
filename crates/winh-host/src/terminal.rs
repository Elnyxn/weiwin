//! Terminal compatibility: own only the dictation tail, stop on other input.
use crate::{
    input_guard::{InputEnd, MousePermits},
    install::Result,
    revision::{Deferred, Revision, Update},
};
use std::{
    mem, ptr,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, POINT, WAIT_TIMEOUT},
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
            SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
        },
    },
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, GetLastInputInfo, LASTINPUTINFO, VK_LWIN, VK_RWIN,
        },
        Input::{
            GetRawInputData, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK,
            RIDEV_REMOVE, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
        },
        WindowsAndMessaging::*,
    },
};

static OTHER_INPUT: AtomicU64 = AtomicU64::new(0);
static WATCHING: AtomicBool = AtomicBool::new(false);
static LAST_ALLOWED: AtomicU32 = AtomicU32::new(0);
static PREVIOUS_ALLOWED: AtomicU32 = AtomicU32::new(0);
static PANEL_PID: AtomicU32 = AtomicU32::new(0);
static PANEL_DRAG: AtomicBool = AtomicBool::new(false);
static MOUSE_PERMITS: Mutex<MousePermits> = Mutex::new(MousePermits::new());
fn panel_window(window: windows_sys::Win32::Foundation::HWND) -> bool {
    let expected = PANEL_PID.load(Ordering::Acquire);
    if expected == 0 {
        return false;
    }
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(window, &mut pid);
    };
    pid == expected
}
fn panel_point(point: POINT) -> bool {
    panel_window(unsafe { WindowFromPoint(point) })
}
fn stop_chord(key: u32) -> bool {
    key == u32::from(VK_LWIN)
        || key == u32::from(VK_RWIN)
        || key == 0x48
            && unsafe {
                GetAsyncKeyState(VK_LWIN as i32) < 0 || GetAsyncKeyState(VK_RWIN as i32) < 0
            }
}
unsafe extern "system" fn raw_input(
    window: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wp: usize,
    lp: isize,
) -> isize {
    if message == WM_INPUT {
        let mut input = RAWINPUT::default();
        let mut size = mem::size_of::<RAWINPUT>() as u32;
        let count = unsafe {
            GetRawInputData(
                lp as _,
                RID_INPUT,
                (&mut input as *mut RAWINPUT).cast(),
                &mut size,
                mem::size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        let interrupt = if count == u32::MAX {
            true
        } else if input.header.dwType == RIM_TYPEKEYBOARD {
            let key = unsafe { input.data.keyboard };
            key.Flags & 1 == 0 && !stop_chord(u32::from(key.VKey))
        } else if input.header.dwType == RIM_TYPEMOUSE {
            let flags = unsafe { input.data.mouse.Anonymous.Anonymous.usButtonFlags };
            let editing = flags & 0x0D55 != 0;
            editing
                && !MOUSE_PERMITS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .consume(unsafe { GetMessageTime() } as u32, flags & 0x0D55)
        } else {
            true
        };
        if interrupt {
            OTHER_INPUT.fetch_add(1, Ordering::Release);
        }
    }
    unsafe { DefWindowProcW(window, message, wp, lp) }
}
fn allowed(time: u32) {
    PREVIOUS_ALLOWED.store(LAST_ALLOWED.swap(time, Ordering::AcqRel), Ordering::Release);
}
fn last_input() -> Result<u32> {
    let mut info = LASTINPUTINFO {
        cbSize: mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return Err("无法确认输入状态".into());
    }
    Ok(info.dwTime)
}
unsafe extern "system" fn keyboard(code: i32, wp: usize, lp: isize) -> isize {
    if code >= 0 {
        let event = unsafe { &*(lp as *const KBDLLHOOKSTRUCT) };
        let win = event.vkCode == u32::from(VK_LWIN) || event.vkCode == u32::from(VK_RWIN);
        let stop_shortcut = event.vkCode == 0x48
            && unsafe {
                GetAsyncKeyState(VK_LWIN as i32) < 0 || GetAsyncKeyState(VK_RWIN as i32) < 0
            };
        if matches!(wp as u32, WM_KEYDOWN | WM_SYSKEYDOWN) && !win && !stop_shortcut {
            OTHER_INPUT.fetch_add(1, Ordering::Release);
        } else {
            allowed(event.time);
        }
    }
    unsafe { CallNextHookEx(ptr::null_mut(), code, wp, lp) }
}
unsafe extern "system" fn mouse(code: i32, wp: usize, lp: isize) -> isize {
    if code >= 0 {
        let event = unsafe { &*(lp as *const MSLLHOOKSTRUCT) };
        let buttons = match wp as u32 {
            WM_LBUTTONDOWN => 1,
            WM_RBUTTONDOWN => 4,
            WM_MBUTTONDOWN => 16,
            WM_XBUTTONDOWN => {
                if event.mouseData >> 16 == 1 {
                    64
                } else {
                    256
                }
            }
            WM_MOUSEWHEEL => 1024,
            WM_MOUSEHWHEEL => 2048,
            _ => 0,
        };
        let panel = buttons != 0 && panel_point(event.pt);
        if wp as u32 == WM_LBUTTONDOWN {
            PANEL_DRAG.store(panel, Ordering::Release);
        }
        if wp as u32 == WM_LBUTTONUP {
            PANEL_DRAG.store(false, Ordering::Release);
        }
        if panel {
            MOUSE_PERMITS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record(event.time, buttons);
        }
        if matches!(
            wp as u32,
            WM_LBUTTONDOWN
                | WM_RBUTTONDOWN
                | WM_MBUTTONDOWN
                | WM_XBUTTONDOWN
                | WM_MOUSEWHEEL
                | WM_MOUSEHWHEEL
        ) && !panel
        {
            OTHER_INPUT.fetch_add(1, Ordering::Release);
        } else {
            allowed(event.time);
        }
    }
    unsafe { CallNextHookEx(ptr::null_mut(), code, wp, lp) }
}
struct Monitor {
    thread: u32,
    worker: Option<JoinHandle<()>>,
    initial: u64,
}
impl Monitor {
    fn new(panel_pid: u32) -> Result<Self> {
        let time = last_input()?;
        if WATCHING.swap(true, Ordering::AcqRel) {
            return Err("终端听写尚未结束".into());
        }
        let initial = OTHER_INPUT.load(Ordering::Acquire);
        PANEL_PID.store(panel_pid, Ordering::Release);
        PANEL_DRAG.store(false, Ordering::Release);
        *MOUSE_PERMITS.lock().unwrap_or_else(|e| e.into_inner()) = MousePermits::new();
        LAST_ALLOWED.store(time, Ordering::Release);
        PREVIOUS_ALLOWED.store(time, Ordering::Release);
        let (sender, ready) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || unsafe {
            use windows_sys::Win32::System::Threading::GetCurrentThreadId;
            use windows_sys::Win32::UI::HiDpi::{
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
            };
            // Low-level mouse hooks report physical screen points. Keep hit
            // testing in that same space, including on scaled monitors.
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let module = GetModuleHandleW(ptr::null());
            let name: Vec<u16> = "WeChatWinH.TerminalGuard"
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let class = WNDCLASSW {
                lpfnWndProc: Some(raw_input),
                hInstance: module,
                lpszClassName: name.as_ptr(),
                ..mem::zeroed()
            };
            RegisterClassW(&class);
            let window = CreateWindowExW(
                0,
                name.as_ptr(),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                ptr::null_mut(),
                module,
                ptr::null(),
            );
            let devices = [
                RAWINPUTDEVICE {
                    usUsagePage: 1,
                    usUsage: 6,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: window,
                },
                RAWINPUTDEVICE {
                    usUsagePage: 1,
                    usUsage: 2,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: window,
                },
            ];
            let raw_ok = !window.is_null()
                && RegisterRawInputDevices(
                    devices.as_ptr(),
                    2,
                    mem::size_of::<RAWINPUTDEVICE>() as u32,
                ) != 0;
            let key = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), module, 0);
            let pointer = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), module, 0);
            let mut message = mem::zeroed();
            PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_NOREMOVE);
            let ok = raw_ok && !key.is_null() && !pointer.is_null();
            let delivered = sender.send(ok.then_some(GetCurrentThreadId())).is_ok();
            if ok && delivered {
                while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            if !key.is_null() {
                UnhookWindowsHookEx(key);
            }
            if !pointer.is_null() {
                UnhookWindowsHookEx(pointer);
            }
            let remove = devices.map(|device| RAWINPUTDEVICE {
                dwFlags: RIDEV_REMOVE,
                hwndTarget: ptr::null_mut(),
                ..device
            });
            RegisterRawInputDevices(remove.as_ptr(), 2, mem::size_of::<RAWINPUTDEVICE>() as u32);
            if !window.is_null() {
                DestroyWindow(window);
            }
            WATCHING.store(false, Ordering::Release);
        });
        match ready.recv_timeout(Duration::from_secs(2)) {
            Ok(Some(thread)) => Ok(Self {
                thread,
                worker: Some(worker),
                initial,
            }),
            Ok(None) => {
                let _ = worker.join();
                Err("无法保护终端输入位置".into())
            }
            Err(_) => Err("终端输入保护未就绪".into()),
        }
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        unsafe {
            PostThreadMessageW(self.thread, WM_QUIT, 0, 0);
        };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
pub struct Terminal {
    window: usize,
    focus: usize,
    pid: u32,
    process: usize,
    monitor: Monitor,
    revision: Revision,
    deferred: Deferred,
    flight: Option<Flight>,
    paused: bool,
}
struct Flight {
    update: Update,
    units: Vec<u16>,
    position: usize,
}
impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.process as HANDLE);
        }
    }
}
impl Terminal {
    pub fn foreground(panel_pid: u32) -> Result<Option<Self>> {
        unsafe {
            let window = GetForegroundWindow();
            let mut class = [0_u16; 128];
            let count = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32);
            if count <= 0
                || String::from_utf16_lossy(&class[..count as usize])
                    != "CASCADIA_HOSTING_WINDOW_CLASS"
            {
                return Ok(None);
            }
            let mut pid = 0;
            GetWindowThreadProcessId(window, &mut pid);
            let mut info: GUITHREADINFO = mem::zeroed();
            info.cbSize = mem::size_of::<GUITHREADINFO>() as u32;
            let mut focus_pid = 0;
            if GetGUIThreadInfo(0, &mut info) == 0
                || info.hwndFocus.is_null()
                || GetAncestor(info.hwndFocus, GA_ROOT) != window
            {
                return Err("无法绑定终端输入控件".into());
            }
            GetWindowThreadProcessId(info.hwndFocus, &mut focus_pid);
            if focus_pid != pid {
                return Err("终端输入控件身份不匹配".into());
            }
            let process = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZATION_SYNCHRONIZE,
                0,
                pid,
            );
            if process.is_null() {
                return Err("无法确认终端窗口".into());
            }
            let mut path = [0_u16; 1024];
            let mut size = path.len() as u32;
            let valid = QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut size) != 0
                && String::from_utf16_lossy(&path[..size as usize])
                    .to_ascii_lowercase()
                    .ends_with("\\windowsterminal.exe");
            if !valid {
                CloseHandle(process);
                return Err("终端进程身份不匹配".into());
            }
            let monitor = match Monitor::new(panel_pid) {
                Ok(m) => m,
                Err(e) => {
                    CloseHandle(process);
                    return Err(e);
                }
            };
            let target = Self {
                window: window as usize,
                focus: info.hwndFocus as usize,
                pid,
                process: process as usize,
                monitor,
                revision: Revision::default(),
                deferred: Deferred::default(),
                flight: None,
                paused: false,
            };
            target.check()?;
            Ok(Some(target))
        }
    }
    fn check(&self) -> Result<bool> {
        if !WATCHING.load(Ordering::Acquire)
            || self
                .monitor
                .worker
                .as_ref()
                .is_none_or(|worker| worker.is_finished())
        {
            return Err("输入保护已停止，听写已停止".into());
        }
        if OTHER_INPUT.load(Ordering::Acquire) != self.monitor.initial {
            return Err(InputEnd::ManualInput.into());
        }
        let time = last_input()?;
        if time != LAST_ALLOWED.load(Ordering::Acquire)
            && time != PREVIOUS_ALLOWED.load(Ordering::Acquire)
        {
            return Err(InputEnd::InputChanged.into());
        }
        unsafe {
            let window = GetForegroundWindow();
            if (window as usize == self.window || panel_window(window))
                && PANEL_DRAG.load(Ordering::Acquire)
            {
                return Ok(false);
            }
            if panel_window(window) {
                return Ok(false);
            }
            let mut pid = 0;
            GetWindowThreadProcessId(window, &mut pid);
            let mut info: GUITHREADINFO = mem::zeroed();
            info.cbSize = mem::size_of::<GUITHREADINFO>() as u32;
            if window as usize != self.window
                || pid != self.pid
                || GetGUIThreadInfo(0, &mut info) == 0
                || info.hwndFocus as usize != self.focus
                || WaitForSingleObject(self.process as HANDLE, 0) != WAIT_TIMEOUT
            {
                return Err(InputEnd::FocusChanged.into());
            }
        }
        Ok(true)
    }
    pub fn update(&mut self, text: &str, final_result: bool, stop: &AtomicBool) -> Result<()> {
        self.deferred.push(text, final_result)?;
        self.poll(stop)
    }
    pub fn poll(&mut self, stop: &AtomicBool) -> Result<()> {
        // Address the bound Terminal control, rather than the global keyboard.
        // Global injected typing makes native Win+H stop its listening session.
        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            if !self.check()? {
                if !self.paused {
                    println!("终端面板交互：暂缓写入");
                    self.paused = true;
                }
                return Ok(());
            }
            if self.paused {
                println!("终端面板交互：恢复写入");
                self.paused = false;
            }
            if self.flight.is_none() {
                let Some(event) = self.deferred.pop() else {
                    return Ok(());
                };
                let update = self.revision.prepare(&event.text, event.final_result)?;
                let units = std::iter::repeat_n(8_u16, update.erase)
                    .chain(update.insert.encode_utf16())
                    .collect();
                self.flight = Some(Flight {
                    update,
                    units,
                    position: 0,
                });
            }
            let flight = self.flight.as_ref().unwrap();
            let Some(unit) = flight.units.get(flight.position).copied() else {
                let flight = self.flight.take().unwrap();
                self.revision.accept(flight.update);
                continue;
            };
            if [0x10, 0x11, 0x12, 0x5B, 0x5C]
                .into_iter()
                .any(|key| unsafe { GetAsyncKeyState(key) < 0 })
            {
                return Err(InputEnd::Modifier.into());
            }
            let mut result = 0;
            if unsafe {
                SendMessageTimeoutW(
                    self.focus as _,
                    WM_CHAR,
                    unit as usize,
                    1,
                    SMTO_BLOCK | SMTO_ABORTIFHUNG,
                    250,
                    &mut result,
                )
            } == 0
            {
                return Err("终端写入未完成，听写已停止".into());
            }
            self.flight.as_mut().unwrap().position += 1;
        }
    }
}

//! Native Win32 control panel. Only our broker executable changes backend state.
#![allow(unsafe_op_in_unsafe_fn)]
use crate::{
    model::{Status, now_ms},
    view,
};
use std::{
    fs::File,
    io::Read,
    mem,
    os::windows::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    ptr,
    sync::mpsc,
    thread,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::{
        LibraryLoader::GetModuleHandleW,
        SystemServices::{SS_CENTER, SS_ENDELLIPSIS, SS_LEFT},
        Threading::*,
    },
    UI::{
        Controls::{DRAWITEMSTRUCT, ODS_DISABLED, ODS_FOCUS, ODS_NOFOCUSRECT, ODS_SELECTED},
        HiDpi::*,
        Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled},
        Shell::*,
        WindowsAndMessaging::*,
    },
};

const CLASS: &str = "WeChatWinH.ControlPanel";
const APP_NAME: &str = "微win";
const TRAY: u32 = WM_APP + 1;
const DONE: u32 = WM_APP + 2;
const UPDATE_READY: u32 = WM_APP + 3;
const OPEN: usize = 101;
const START: usize = 102;
const STOP: usize = 103;
const EXIT: usize = 104;
const TOGGLE: usize = 105;
const MINIMIZE: usize = 106;
const CLOSE: usize = 107;
const GITHUB: usize = 108;
const UPDATE_DOWNLOAD: usize = 109;
const UPDATE_SKIP: usize = 110;
const UPDATE_CLOSE: usize = 111;
const UPDATE_TITLE: usize = 222;
const UPDATE_BODY: usize = 223;
const APP_TITLE: usize = 221;
const LABEL_STATUS: usize = 203;
const LABEL_ERROR: usize = 206;
const LABEL_SERVICE: usize = 208;
const LABEL_SOURCE: usize = 209;
const LABEL_MIC_TITLE: usize = 210;
const LABEL_SILENCE: usize = 211;
const LABEL_SHORTCUT: usize = 212;
const LABEL_MIC_VALUE: usize = 216;
const LABEL_SILENCE_VALUE: usize = 217;
const LABEL_WIN: usize = 218;
const LABEL_PLUS: usize = 219;
const LABEL_H: usize = 220;
const GREEN: u32 = 0x00447916;
const INK: u32 = 0x00251e18;
const MUTED: u32 = 0x00776d65;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn scale(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 48) / 96
}

unsafe fn work_area(window: HWND) -> RECT {
    let monitor = if window.is_null() {
        let mut point = mem::zeroed();
        GetCursorPos(&mut point);
        MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST)
    } else {
        MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST)
    };
    let mut info: MONITORINFO = mem::zeroed();
    info.cbSize = mem::size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(monitor, &mut info);
    info.rcWork
}
fn fitted_dpi(actual: u32, work: RECT) -> u32 {
    actual
        .min(((work.bottom - work.top - 32).max(1) * 96 / view::HEIGHT) as u32)
        .min(((work.right - work.left - 32).max(1) * 96 / view::WIDTH) as u32)
        .max(72)
}

unsafe fn ui_font(window: HWND) -> &'static str {
    unsafe extern "system" fn found(
        _: *const LOGFONTW,
        _: *const TEXTMETRICW,
        _: u32,
        state: LPARAM,
    ) -> i32 {
        unsafe {
            *(state as *mut bool) = true;
        }
        0
    }
    let dc = GetDC(window);
    for name in ["HarmonyOS Sans SC", "MiSans", "Microsoft YaHei UI"] {
        let mut request: LOGFONTW = mem::zeroed();
        request.lfCharSet = DEFAULT_CHARSET;
        for (to, from) in request.lfFaceName.iter_mut().zip(name.encode_utf16()) {
            *to = from;
        }
        let mut exists = false;
        EnumFontFamiliesExW(
            dc,
            &request,
            Some(found),
            (&mut exists as *mut bool) as isize,
            0,
        );
        if exists {
            ReleaseDC(window, dc);
            return name;
        }
    }
    ReleaseDC(window, dc);
    "Segoe UI"
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() {
                CloseHandle(self.0);
            }
        }
    }
}

fn event(name: &str, signaled: bool) -> bool {
    unsafe {
        let handle = Handle(OpenEventW(
            SYNCHRONIZATION_SYNCHRONIZE,
            0,
            wide(name).as_ptr(),
        ));
        !handle.0.is_null() && (!signaled || WaitForSingleObject(handle.0, 0) == WAIT_OBJECT_0)
    }
}

fn alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid));
        if process.0.is_null() {
            return false;
        }
        let mut code = 0;
        GetExitCodeProcess(process.0, &mut code) != 0 && code == STILL_ACTIVE as u32
    }
}

#[derive(Clone, Copy)]
enum Action {
    Start,
    Stop,
    Exit,
}
impl Action {
    fn argument(self) -> &'static str {
        if matches!(self, Self::Start) {
            "start"
        } else {
            "stop"
        }
    }
}
struct Completed {
    action: Action,
    result: Result<(), String>,
}

fn backend_command(action: Action) -> Result<(), String> {
    let executable = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("无法定位程序目录")?
        .join("wechat-winh.exe");
    // An uninstalled GUI may still exit when no broker owns the restoration event.
    if matches!(action, Action::Exit)
        && !executable.is_file()
        && !event("Local\\WeChatWinH.Stop", false)
    {
        return Ok(());
    }
    // A detached descendant can retain a redirected pipe on Windows. Wait for
    // the command process itself and read a bounded file after it exits.
    let folder = std::env::var_os("LOCALAPPDATA")
        .map(|root| PathBuf::from(root).join("WeChatWinH"))
        .ok_or("无法定位应用数据目录")?;
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let path = folder.join(format!("gui-command-{}.log", std::process::id()));
    let log = File::create(&path).map_err(|e| e.to_string())?;
    let exit = Command::new(&executable)
        .arg(action.argument())
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .status()
        .map_err(|e| format!("无法运行识别服务：{e}"))?;
    let mut bytes = Vec::new();
    if let Ok(file) = File::open(&path) {
        let _ = file.take(4096).read_to_end(&mut bytes);
    }
    let _ = std::fs::remove_file(path);
    if exit.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&bytes);
    let message = message.trim();
    Err(if message.is_empty() {
        format!("操作失败（{exit}）")
    } else {
        message.chars().take(300).collect()
    })
}

struct App {
    hwnd: HWND,
    dpi: u32,
    font_family: &'static str,
    controls: Vec<(usize, HWND)>,
    fonts: Vec<HFONT>,
    white: HBRUSH,
    caption: HBRUSH,
    icon: HICON,
    github_icon: HICON,
    taskbar_created: u32,
    path: Option<PathBuf>,
    status: Option<Status>,
    ready: bool,
    installed: bool,
    healthy: bool,
    busy: Option<Action>,
    pending_exit: bool,
    command_error: String,
    sender: mpsc::Sender<Completed>,
    receiver: mpsc::Receiver<Completed>,
    tray_ok: bool,
    tray_tip: String,
    _renderer: Option<view::Renderer>,
    animate: bool,
    settings: wechat_voice_backend::settings::Settings,
    microphones: Vec<(String, String)>,
    update_sender: mpsc::Sender<(bool, Result<Option<crate::update::Release>, String>)>,
    update_receiver: mpsc::Receiver<(bool, Result<Option<crate::update::Release>, String>)>,
    update_preferences: crate::update::Preferences,
    update_release: Option<crate::update::Release>,
    update_checking: bool,
    update_visible: bool,
    update_dismissed: Option<String>,
    update_error: String,
}

impl App {
    unsafe fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        let (update_sender, update_receiver) = mpsc::channel();
        let (update_preferences, update_error) = match crate::update::Preferences::load() {
            Ok(value) => (value, String::new()),
            Err(error) => (Default::default(), error),
        };
        let (settings, settings_error) =
            match wechat_voice_backend::settings::Settings::load_default() {
                Ok(value) => (value, String::new()),
                Err(error) => (Default::default(), format!("无法读取设置：{error}")),
            };
        let module = GetModuleHandleW(ptr::null());
        let mut icon = LoadIconW(module, 100_usize as *const u16);
        if icon.is_null() {
            icon = LoadIconW(ptr::null_mut(), IDI_APPLICATION);
        }
        Self {
            hwnd: ptr::null_mut(),
            dpi: 96,
            font_family: "Microsoft YaHei UI",
            controls: Vec::new(),
            fonts: Vec::new(),
            white: CreateSolidBrush(view::BACKGROUND),
            caption: CreateSolidBrush(0x00fcf7f2),
            icon,
            github_icon: LoadIconW(module, 101_usize as *const u16),
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            path: std::env::var_os("LOCALAPPDATA")
                .map(|p| PathBuf::from(p).join("WeChatWinH/status.json")),
            status: None,
            ready: false,
            installed: false,
            healthy: false,
            busy: None,
            pending_exit: false,
            command_error: settings_error,
            sender,
            receiver,
            tray_ok: false,
            tray_tip: String::new(),
            _renderer: view::Renderer::new().ok(),
            animate: false,
            settings,
            microphones: Vec::new(),
            update_sender,
            update_receiver,
            update_preferences,
            update_release: None,
            update_checking: false,
            update_visible: false,
            update_dismissed: None,
            update_error,
        }
    }

    fn control(&self, id: usize) -> HWND {
        self.controls
            .iter()
            .find(|(key, _)| *key == id)
            .map_or(ptr::null_mut(), |(_, hwnd)| *hwnd)
    }

    unsafe fn label(&mut self, id: usize, text: &str) {
        let control = CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD
                | WS_VISIBLE
                | if matches!(
                    id,
                    LABEL_STATUS | LABEL_ERROR | LABEL_WIN | LABEL_PLUS | LABEL_H
                ) {
                    SS_CENTER
                } else {
                    SS_LEFT
                }
                | SS_ENDELLIPSIS,
            0,
            0,
            0,
            0,
            self.hwnd,
            id as HMENU,
            GetModuleHandleW(ptr::null()),
            ptr::null(),
        );
        self.controls.push((id, control));
    }

    unsafe fn create(&mut self) -> Result<(), String> {
        for (id, text) in [
            (APP_TITLE, APP_NAME),
            (LABEL_STATUS, "正在读取状态"),
            (LABEL_ERROR, ""),
            (LABEL_SERVICE, "后台服务"),
            (LABEL_SOURCE, "识别来源"),
            (LABEL_MIC_TITLE, "麦克风"),
            (LABEL_SILENCE, "静音收句"),
            (LABEL_SHORTCUT, "唤起快捷键"),
            (LABEL_PLUS, "+"),
            (LABEL_H, "H"),
            (UPDATE_TITLE, ""),
            (UPDATE_BODY, ""),
        ] {
            self.label(id, text);
        }
        for (id, text, style) in [
            (START, "输入法识别", BS_OWNERDRAW),
            (STOP, "Windows 原生", BS_OWNERDRAW),
            (TOGGLE, "开启后台服务", BS_OWNERDRAW),
            (MINIMIZE, "最小化", BS_OWNERDRAW),
            (CLOSE, "关闭窗口", BS_OWNERDRAW),
            (GITHUB, "GitHub · Elnyxn", BS_OWNERDRAW),
            (LABEL_MIC_VALUE, "系统默认", BS_OWNERDRAW),
            (LABEL_SILENCE_VALUE, "3 秒", BS_OWNERDRAW),
            (UPDATE_DOWNLOAD, "下载更新", BS_OWNERDRAW),
            (UPDATE_SKIP, "跳过此版本", BS_OWNERDRAW),
            (UPDATE_CLOSE, "关闭更新提示", BS_OWNERDRAW),
        ] {
            let control = CreateWindowExW(
                0,
                wide("BUTTON").as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | style as u32,
                0,
                0,
                0,
                0,
                self.hwnd,
                id as HMENU,
                GetModuleHandleW(ptr::null()),
                ptr::null(),
            );
            self.controls.push((id, control));
        }
        if self.controls.iter().any(|(_, h)| h.is_null()) {
            return Err("无法创建控制面板".into());
        }
        self.dpi = fitted_dpi(GetDpiForWindow(self.hwnd).max(96), work_area(self.hwnd));
        self.font_family = ui_font(self.hwnd);
        self.layout();
        self.update_bubble(false);
        let _ = self.read_microphones();
        SendMessageW(self.hwnd, WM_CHANGEUISTATE, 1 | (3 << 16), 0);
        self.tray(NIM_ADD);
        SetTimer(self.hwnd, 1, 750, None);
        SetTimer(self.hwnd, 3, 6 * 60 * 60 * 1000, None);
        SetTimer(self.hwnd, 4, 3000, None);
        self.refresh();
        Ok(())
    }

    unsafe fn set_text(&self, id: usize, text: &str) {
        let control = self.control(id);
        let mut previous = vec![0_u16; GetWindowTextLengthW(control).max(0) as usize + 1];
        GetWindowTextW(control, previous.as_mut_ptr(), previous.len() as i32);
        if previous != wide(text) {
            SetWindowTextW(control, wide(text).as_ptr());
        }
    }

    unsafe fn layout(&mut self) {
        let old = mem::take(&mut self.fonts);
        for (size, weight) in [(14, 400), (12, 400), (18, 500), (13, 400), (15, 500)] {
            let font = CreateFontW(
                -scale(size, self.dpi),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                DEFAULT_PITCH as u32,
                wide(self.font_family).as_ptr(),
            );
            self.fonts.push(font);
        }
        let mut client: RECT = mem::zeroed();
        GetClientRect(self.hwnd, &mut client);
        let width = client.right * 96 / self.dpi as i32;
        let inner = width - 40;
        let positions = [
            (LABEL_STATUS, 20, 190, inner, 28, 2),
            (LABEL_ERROR, 20, 217, inner, 12, 1),
            (LABEL_SERVICE, 52, 249, 230, 22, 0),
            (TOGGLE, 336, 245, 44, 28, 3),
            (LABEL_SOURCE, 52, 307, 110, 22, 0),
            (START, 172, 300, 104, 34, 3),
            (STOP, 277, 300, 103, 34, 3),
            (LABEL_MIC_TITLE, 52, 365, 130, 22, 0),
            (LABEL_MIC_VALUE, 212, 357, 168, 36, 3),
            (LABEL_SILENCE, 52, 423, 130, 22, 0),
            (LABEL_SILENCE_VALUE, 264, 415, 116, 36, 3),
            (LABEL_SHORTCUT, 52, 481, 180, 22, 0),
            (LABEL_PLUS, 326, 480, 18, 22, 3),
            (LABEL_H, 352, 480, 24, 22, 3),
            (UPDATE_TITLE, 128, 62, 216, 22, 4),
            (UPDATE_BODY, 128, 89, 216, 18, 1),
            (UPDATE_DOWNLOAD, 128, 119, 85, 30, 3),
            (UPDATE_SKIP, 219, 119, 105, 30, 3),
            (UPDATE_CLOSE, 346, 54, 24, 24, 3),
        ];
        for (id, x, y, w, h, font) in positions {
            let control = self.control(id);
            SetWindowPos(
                control,
                ptr::null_mut(),
                scale(x, self.dpi),
                scale(view::body_y(y), self.dpi),
                scale(w, self.dpi),
                scale(h, self.dpi),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            SendMessageW(control, WM_SETFONT, self.fonts[font] as usize, 0);
        }
        for (id, x, y, w, h) in [
            (APP_TITLE, 50, 10, 80, 24),
            (MINIMIZE, 312, 0, 44, 42),
            (CLOSE, 356, 0, 44, 42),
            (GITHUB, 140, 5, 32, 32),
        ] {
            SetWindowPos(
                self.control(id),
                ptr::null_mut(),
                scale(x, self.dpi),
                scale(y, self.dpi),
                scale(w, self.dpi),
                scale(h, self.dpi),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            SendMessageW(self.control(id), WM_SETFONT, self.fonts[4] as usize, 0);
        }
        for font in old {
            if !font.is_null() {
                DeleteObject(font);
            }
        }
        InvalidateRect(self.hwnd, ptr::null(), 1);
    }

    unsafe fn tray(&mut self, action: u32) {
        let mut data: NOTIFYICONDATAW = mem::zeroed();
        data.cbSize = mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = 1;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = TRAY;
        data.hIcon = self.icon;
        let tip = format!(
            "微win · {} · {}",
            self.status
                .as_ref()
                .map_or("语音输入", Status::backend_label),
            if self.busy.is_some() {
                "正在切换"
            } else {
                self.status.as_ref().map_or(
                    if self.ready {
                        "等待状态"
                    } else {
                        "Windows 原生"
                    },
                    |s| s.headline(now_ms(), self.healthy, self.ready),
                )
            }
        );
        if action == NIM_MODIFY && tip == self.tray_tip {
            return;
        }
        self.tray_tip = tip.clone();
        let tip = wide(&tip);
        let len = tip.len().min(data.szTip.len() - 1);
        data.szTip[..len].copy_from_slice(&tip[..len]);
        let success = Shell_NotifyIconW(action, &data) != 0;
        if action == NIM_ADD {
            self.tray_ok = success;
        }
    }

    unsafe fn refresh(&mut self) {
        let previous = (self.ready, self.installed, self.animate);
        self.ready = event("Local\\WeChatWinH.Ready", true);
        self.installed = event("Local\\WeChatWinH.Stop", false);
        let read = self
            .path
            .as_deref()
            .and_then(|path| Status::read(path).ok());
        if let Some(status) = read {
            self.status = Some(status);
        }
        self.healthy = self.status.as_ref().is_some_and(|s| alive(s.pid));
        let headline = if let Some(action) = self.busy {
            match action {
                Action::Start => "正在启用",
                Action::Stop => "正在恢复 Windows",
                Action::Exit => "正在退出",
            }
        } else if !self.command_error.is_empty() {
            "操作失败"
        } else {
            self.status.as_ref().map_or(
                if self.ready {
                    "等待状态"
                } else {
                    "Windows 原生"
                },
                |s| s.headline(now_ms(), self.healthy, self.ready),
            )
        };
        self.set_text(LABEL_STATUS, headline);
        let microphone = self
            .settings
            .microphone
            .as_ref()
            .map(|id| {
                self.microphones
                    .iter()
                    .find(|(key, _)| key == id)
                    .map(|(_, name)| name.as_str())
                    .unwrap_or("已选麦克风")
            })
            .unwrap_or("系统默认");
        self.set_text(LABEL_MIC_VALUE, microphone);
        self.set_text(
            LABEL_SILENCE_VALUE,
            &format!("{} 秒", self.settings.silence_seconds),
        );
        let error = if !self.command_error.is_empty() {
            self.command_error.as_str()
        } else {
            self.status
                .as_ref()
                .filter(|s| s.phase == "error")
                .map_or("", |s| s.last_error.as_str())
        };
        self.set_text(LABEL_ERROR, &error.chars().take(180).collect::<String>());
        let available = self.busy.is_none();
        let mut buttons_changed = previous.1 != self.installed;
        for id in [START, STOP, TOGGLE] {
            if (IsWindowEnabled(self.control(id)) != 0) != available {
                EnableWindow(self.control(id), available as i32);
                buttons_changed = true;
            }
        }
        self.set_text(
            TOGGLE,
            if self.installed {
                "关闭后台服务"
            } else {
                "开启后台服务"
            },
        );
        self.animate = self.ready
            && self.healthy
            && self
                .status
                .as_ref()
                .is_some_and(|s| s.phase == "recognizing" && s.peak > 0 && !s.stale(now_ms()));
        let mut animations = 1_i32;
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&mut animations as *mut i32).cast(),
            0,
        );
        if self.animate && animations != 0 && IsWindowVisible(self.hwnd) != 0 {
            SetTimer(self.hwnd, 2, 60, None);
        } else {
            KillTimer(self.hwnd, 2);
        }
        if previous.0 != self.ready || previous.2 != self.animate {
            let rect = RECT {
                left: scale(80, self.dpi),
                top: scale(50, self.dpi),
                right: scale(320, self.dpi),
                bottom: scale(187, self.dpi),
            };
            InvalidateRect(self.hwnd, &rect, 0);
        }
        if buttons_changed {
            for id in [START, STOP, TOGGLE] {
                InvalidateRect(self.control(id), ptr::null(), 0);
            }
        }
        self.tray(if self.tray_ok { NIM_MODIFY } else { NIM_ADD });
    }

    unsafe fn begin(&mut self, action: Action) {
        if self.busy.is_some() {
            if matches!(action, Action::Exit) {
                self.pending_exit = true;
            }
            return;
        }
        self.busy = Some(action);
        self.command_error.clear();
        self.refresh();
        let sender = self.sender.clone();
        let hwnd = self.hwnd as usize;
        thread::spawn(move || {
            let result = backend_command(action);
            let _ = sender.send(Completed { action, result });
            PostMessageW(hwnd as HWND, DONE, 0, 0);
        });
    }

    unsafe fn completed(&mut self) {
        while let Ok(done) = self.receiver.try_recv() {
            self.busy = None;
            match done.result {
                Ok(()) if matches!(done.action, Action::Exit) => {
                    DestroyWindow(self.hwnd);
                    return;
                }
                Ok(()) => self.command_error.clear(),
                Err(error) => {
                    self.command_error = error;
                    self.show();
                }
            }
        }
        if self.pending_exit && self.busy.is_none() {
            self.pending_exit = false;
            self.begin(Action::Exit);
            return;
        }
        self.refresh();
    }

    unsafe fn show(&self) {
        ShowWindow(self.hwnd, SW_RESTORE);
        SetForegroundWindow(self.hwnd);
    }

    unsafe fn check_updates(&mut self, manual: bool) {
        if self.update_checking {
            return;
        }
        self.update_checking = true;
        self.set_text(GITHUB, "GitHub · 正在检查更新");
        let sender = self.update_sender.clone();
        let hwnd = self.hwnd as usize;
        thread::spawn(move || {
            let result = crate::update::latest(env!("CARGO_PKG_VERSION"));
            if sender.send((manual, result)).is_ok() {
                PostMessageW(hwnd as HWND, UPDATE_READY, 0, 0);
            }
        });
    }

    unsafe fn updates_ready(&mut self) {
        while let Ok((manual, result)) = self.update_receiver.try_recv() {
            self.update_checking = false;
            match result {
                Ok(Some(release)) => {
                    self.update_error.clear();
                    let notify = manual
                        || self.update_preferences.skipped_version.as_ref()
                            != Some(&release.version)
                            && self.update_dismissed.as_ref() != Some(&release.version);
                    self.set_text(UPDATE_TITLE, &format!("发现新版本 v{}", release.version));
                    self.set_text(
                        UPDATE_BODY,
                        &format!("当前版本 v{}", env!("CARGO_PKG_VERSION")),
                    );
                    self.update_release = Some(release);
                    if notify {
                        self.update_bubble(true);
                    }
                }
                Ok(None) => {
                    self.update_error.clear();
                    self.update_release = None;
                    if manual {
                        self.set_text(UPDATE_TITLE, "当前已是最新版本");
                        self.set_text(
                            UPDATE_BODY,
                            &format!("微win v{}", env!("CARGO_PKG_VERSION")),
                        );
                        self.update_bubble(true);
                    } else {
                        self.update_bubble(false);
                    }
                }
                Err(error) => {
                    self.update_error = error;
                    if manual {
                        self.update_release = None;
                        self.set_text(UPDATE_TITLE, "暂时无法检查更新");
                        self.set_text(UPDATE_BODY, "请稍后再试");
                        self.update_bubble(true);
                    }
                }
            }
            self.set_text(
                GITHUB,
                if self.update_release.is_some() {
                    "GitHub · 有可用更新"
                } else {
                    "GitHub · Elnyxn/weiwin"
                },
            );
            InvalidateRect(self.control(GITHUB), ptr::null(), 0);
        }
    }

    unsafe fn update_bubble(&mut self, show: bool) {
        self.update_visible = show;
        for id in [UPDATE_TITLE, UPDATE_BODY, UPDATE_CLOSE] {
            ShowWindow(self.control(id), if show { SW_SHOW } else { SW_HIDE });
        }
        for id in [UPDATE_DOWNLOAD, UPDATE_SKIP] {
            ShowWindow(
                self.control(id),
                if show && self.update_release.is_some() {
                    SW_SHOW
                } else {
                    SW_HIDE
                },
            );
        }
        let rect = RECT {
            left: scale(108, self.dpi),
            top: scale(40, self.dpi),
            right: scale(388, self.dpi),
            bottom: scale(190, self.dpi),
        };
        InvalidateRect(self.hwnd, &rect, 0);
    }

    unsafe fn dismiss_update(&mut self, skip: bool) {
        if let Some(release) = self.update_release.as_ref() {
            if skip {
                let previous = self
                    .update_preferences
                    .skipped_version
                    .replace(release.version.clone());
                if let Err(error) = self.update_preferences.save() {
                    self.update_preferences.skipped_version = previous;
                    self.update_error = error;
                    self.set_text(UPDATE_BODY, "跳过设置保存失败，请重试");
                    return;
                }
            }
            self.update_dismissed = Some(release.version.clone());
        }
        if skip {
            self.update_release = None;
            InvalidateRect(self.control(GITHUB), ptr::null(), 0);
        }
        self.update_bubble(false);
    }

    unsafe fn github_menu(&mut self) {
        let mut rect = mem::zeroed();
        GetWindowRect(self.control(GITHUB), &mut rect);
        let choices = vec![
            (1, "打开 GitHub".to_owned(), false),
            (2, "检查更新".to_owned(), false),
        ];
        match crate::menu::popup(
            self.hwnd,
            rect.left,
            rect.bottom + scale(4, self.dpi),
            &choices,
            self.fonts[3],
            self.dpi,
        ) {
            1 => self.action(GITHUB),
            2 => self.check_updates(true),
            _ => {}
        }
    }

    unsafe fn menu(&mut self) {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let disabled = if self.busy.is_some() { MF_GRAYED } else { 0 };
        AppendMenuW(menu, MF_STRING, OPEN, wide("打开控制面板").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            menu,
            MF_STRING | disabled | if self.installed { MF_GRAYED } else { 0 },
            START,
            wide("启用输入法识别").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING | disabled | if !self.installed { MF_GRAYED } else { 0 },
            STOP,
            wide("Windows 原生").as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING | disabled, EXIT, wide("退出").as_ptr());
        let mut point: POINT = mem::zeroed();
        GetCursorPos(&mut point);
        SetForegroundWindow(self.hwnd);
        let selected = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            self.hwnd,
            ptr::null(),
        );
        DestroyMenu(menu);
        PostMessageW(self.hwnd, WM_NULL, 0, 0);
        self.action(selected as usize);
    }

    unsafe fn action(&mut self, id: usize) {
        match id {
            OPEN => self.show(),
            START if !self.installed => self.begin(Action::Start),
            STOP if self.installed => self.begin(Action::Stop),
            TOGGLE => self.begin(if self.installed {
                Action::Stop
            } else {
                Action::Start
            }),
            MINIMIZE => {
                ShowWindow(self.hwnd, SW_MINIMIZE);
            }
            CLOSE => {
                PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
            }
            GITHUB => {
                let result = ShellExecuteW(
                    self.hwnd,
                    wide("open").as_ptr(),
                    wide(env!("CARGO_PKG_HOMEPAGE")).as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    SW_SHOWNORMAL,
                );
                if result as isize <= 32 {
                    self.command_error = "无法打开 GitHub".to_owned();
                    self.refresh();
                }
            }
            UPDATE_DOWNLOAD => {
                if let Some(release) = self.update_release.as_ref() {
                    ShellExecuteW(
                        self.hwnd,
                        wide("open").as_ptr(),
                        wide(&release.url).as_ptr(),
                        ptr::null(),
                        ptr::null(),
                        SW_SHOWNORMAL,
                    );
                }
            }
            UPDATE_SKIP => self.dismiss_update(true),
            UPDATE_CLOSE => self.dismiss_update(false),
            2 if self.update_visible => self.dismiss_update(false),
            LABEL_MIC_VALUE => self.settings_menu(true),
            LABEL_SILENCE_VALUE => self.settings_menu(false),
            EXIT => self.begin(Action::Exit),
            _ => {}
        }
    }

    fn read_microphones(&mut self) -> Result<(), String> {
        let devices =
            thread::spawn(|| wechat_voice_audio::microphone::devices().map_err(|e| e.to_string()))
                .join()
                .map_err(|_| "无法读取麦克风".to_owned())??;
        self.microphones.clear();
        for (_, device) in devices {
            if !self.microphones.iter().any(|(id, _)| *id == device.id) {
                self.microphones.push((device.id, device.name));
            }
        }
        Ok(())
    }

    unsafe fn settings_menu(&mut self, microphone: bool) {
        if microphone {
            if let Err(error) = self.read_microphones() {
                self.command_error = error;
                self.refresh();
                return;
            }
        }
        let choices = if microphone {
            let mut choices = vec![(
                1000,
                "系统默认".to_owned(),
                self.settings.microphone.is_none(),
            )];
            choices.extend(self.microphones.iter().enumerate().map(|(i, (id, name))| {
                (
                    1001 + i,
                    name.clone(),
                    self.settings.microphone.as_ref() == Some(id),
                )
            }));
            choices
        } else {
            [1, 2, 3, 5, 10]
                .into_iter()
                .enumerate()
                .map(|(i, s)| {
                    (
                        1001 + i,
                        format!("{s} 秒"),
                        self.settings.silence_seconds == s,
                    )
                })
                .collect()
        };
        let mut rect = mem::zeroed();
        GetWindowRect(
            self.control(if microphone {
                LABEL_MIC_VALUE
            } else {
                LABEL_SILENCE_VALUE
            }),
            &mut rect,
        );
        let selected = crate::menu::popup(
            self.hwnd,
            rect.left,
            rect.bottom + scale(4, self.dpi),
            &choices,
            self.fonts[3],
            self.dpi,
        );
        if selected == 0 {
            return;
        }
        let mut candidate = self.settings.clone();
        if microphone {
            candidate.microphone = if selected == 1000 {
                None
            } else {
                self.microphones
                    .get(selected as usize - 1001)
                    .map(|(id, _)| id.clone())
            };
        } else if let Some(seconds) = [1, 2, 3, 5, 10].get(selected as usize - 1001) {
            candidate.silence_seconds = *seconds;
        }
        match candidate.save_default() {
            Ok(()) => {
                self.settings = candidate;
                self.command_error.clear();
            }
            Err(error) => self.command_error = format!("无法保存设置：{error}"),
        }
        self.refresh();
    }

    unsafe fn draw_button(&self, draw: &DRAWITEMSTRUCT) {
        let width = draw.rcItem.right - draw.rcItem.left;
        let height = draw.rcItem.bottom - draw.rcItem.top;
        let dc = CreateCompatibleDC(draw.hDC);
        let bitmap = CreateCompatibleBitmap(draw.hDC, width, height);
        if dc.is_null() || bitmap.is_null() {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            if !dc.is_null() {
                DeleteDC(dc);
            }
            self.draw_button_content(draw);
            return;
        }
        let old = SelectObject(dc, bitmap);
        let mut buffered = *draw;
        buffered.hDC = dc;
        self.draw_button_content(&buffered);
        BitBlt(
            draw.hDC,
            draw.rcItem.left,
            draw.rcItem.top,
            width,
            height,
            dc,
            0,
            0,
            SRCCOPY,
        );
        SelectObject(dc, old);
        DeleteObject(bitmap);
        DeleteDC(dc);
    }

    unsafe fn draw_button_content(&self, draw: &DRAWITEMSTRUCT) {
        let disabled = draw.itemState & ODS_DISABLED != 0;
        let pressed = draw.itemState & ODS_SELECTED != 0;
        FillRect(draw.hDC, &draw.rcItem, self.white);
        if draw.CtlID == GITHUB as u32 {
            FillRect(draw.hDC, &draw.rcItem, self.caption);
            DrawIconEx(
                draw.hDC,
                (draw.rcItem.right - scale(18, self.dpi)) / 2,
                (draw.rcItem.bottom - scale(18, self.dpi)) / 2,
                self.github_icon,
                scale(18, self.dpi),
                scale(18, self.dpi),
                0,
                ptr::null_mut(),
                DI_NORMAL,
            );
            if self.update_release.as_ref().is_some_and(|release| {
                self.update_preferences.skipped_version.as_ref() != Some(&release.version)
            }) {
                view::update_badge(draw.hDC, self.dpi);
            }
            return;
        }
        if draw.CtlID == UPDATE_CLOSE as u32 {
            view::close_glyph(draw.hDC, self.dpi, draw.rcItem.right, draw.rcItem.bottom);
            return;
        }
        if matches!(draw.CtlID as usize, LABEL_MIC_VALUE | LABEL_SILENCE_VALUE) {
            view::value_button(
                draw.hDC,
                self.dpi,
                draw.rcItem.right,
                draw.rcItem.bottom,
                pressed,
            );
            SetBkMode(draw.hDC, TRANSPARENT as i32);
            SetTextColor(draw.hDC, INK);
            SelectObject(draw.hDC, self.fonts[3]);
            let mut rect = draw.rcItem;
            rect.left += scale(12, self.dpi);
            rect.right -= scale(30, self.dpi);
            let mut text = [0u16; 512];
            let len = GetWindowTextW(draw.hwndItem, text.as_mut_ptr(), 512);
            DrawTextW(
                draw.hDC,
                text.as_ptr(),
                len,
                &mut rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
            return;
        }
        if matches!(draw.CtlID as usize, MINIMIZE | CLOSE) {
            FillRect(draw.hDC, &draw.rcItem, self.caption);
            if pressed {
                let b = CreateSolidBrush(0x00eee7e0);
                FillRect(draw.hDC, &draw.rcItem, b);
                DeleteObject(b);
            }
            let center_x = draw.rcItem.right / 2;
            let center_y = draw.rcItem.bottom / 2;
            let pen = CreatePen(PS_SOLID, scale(1, self.dpi).max(1), INK);
            let old = SelectObject(draw.hDC, pen);
            let radius = scale(5, self.dpi);
            MoveToEx(
                draw.hDC,
                center_x - radius,
                center_y
                    - if draw.CtlID == CLOSE as u32 {
                        radius
                    } else {
                        0
                    },
                ptr::null_mut(),
            );
            LineTo(
                draw.hDC,
                center_x + radius,
                center_y
                    + if draw.CtlID == CLOSE as u32 {
                        radius
                    } else {
                        0
                    },
            );
            if draw.CtlID == CLOSE as u32 {
                MoveToEx(
                    draw.hDC,
                    center_x - radius,
                    center_y + radius,
                    ptr::null_mut(),
                );
                LineTo(draw.hDC, center_x + radius, center_y - radius);
            }
            SelectObject(draw.hDC, old);
            DeleteObject(pen);
            if draw.itemState & (ODS_FOCUS | ODS_NOFOCUSRECT) == ODS_FOCUS {
                let mut r = draw.rcItem;
                InflateRect(&mut r, -4, -4);
                DrawFocusRect(draw.hDC, &r);
            }
            return;
        }
        let toggle = draw.CtlID == TOGGLE as u32;
        let selected = if draw.CtlID == UPDATE_DOWNLOAD as u32 {
            true
        } else if draw.CtlID == UPDATE_SKIP as u32 {
            false
        } else if draw.CtlID == STOP as u32 {
            !self.installed
        } else {
            self.installed
        };
        view::button(
            draw.hDC,
            self.dpi,
            draw.rcItem.right,
            draw.rcItem.bottom,
            toggle,
            selected,
            disabled,
            pressed,
        );
        SetBkMode(draw.hDC, TRANSPARENT as i32);
        SetTextColor(
            draw.hDC,
            if disabled {
                MUTED
            } else if selected {
                GREEN
            } else {
                INK
            },
        );
        SelectObject(draw.hDC, self.fonts[3]);
        let mut rect = draw.rcItem;
        let mut text = [0_u16; 128];
        let len = GetWindowTextW(draw.hwndItem, text.as_mut_ptr(), text.len() as i32);
        if !toggle {
            DrawTextW(
                draw.hDC,
                text.as_ptr(),
                len,
                &mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
        if draw.itemState & (ODS_FOCUS | ODS_NOFOCUSRECT) == ODS_FOCUS {
            InflateRect(&mut rect, -scale(4, self.dpi), -scale(4, self.dpi));
            DrawFocusRect(draw.hDC, &rect);
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        unsafe {
            for font in &self.fonts {
                if !font.is_null() {
                    DeleteObject(*font);
                }
            }
            DeleteObject(self.white);
            DeleteObject(self.caption);
        }
    }
}

unsafe extern "system" fn procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let creation = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, creation.lpCreateParams as isize);
    }
    let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if raw.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let app = &mut *raw;
    if app.taskbar_created != 0 && message == app.taskbar_created {
        app.tray(NIM_ADD);
        return 0;
    }
    match message {
        UPDATE_READY => {
            app.updates_ready();
            0
        }
        WM_CONTEXTMENU if wparam as HWND == app.control(GITHUB) => {
            app.github_menu();
            0
        }
        WM_SETCURSOR if wparam as HWND == app.control(GITHUB) => {
            SetCursor(LoadCursorW(ptr::null_mut(), IDC_HAND));
            1
        }
        WM_MEASUREITEM if lparam != 0 => {
            if crate::menu::measure(hwnd, lparam as *mut _) {
                1
            } else {
                DefWindowProcW(hwnd, message, wparam, lparam)
            }
        }
        WM_NCCALCSIZE => 0,
        WM_NCHITTEST => {
            let mut point = POINT {
                x: (lparam as i16) as i32,
                y: ((lparam >> 16) as i16) as i32,
            };
            ScreenToClient(hwnd, &mut point);
            if point.y >= 0
                && point.y < scale(view::HEADER, app.dpi)
                && point.x >= scale(40, app.dpi)
                && point.x < scale(312, app.dpi)
                && !(point.x >= scale(140, app.dpi) && point.x < scale(172, app.dpi))
            {
                HTCAPTION as isize
            } else {
                HTCLIENT as isize
            }
        }
        WM_CREATE => {
            app.hwnd = hwnd;
            if let Err(error) = app.create() {
                MessageBoxW(
                    hwnd,
                    wide(&error).as_ptr(),
                    wide(APP_NAME).as_ptr(),
                    MB_OK | MB_ICONERROR,
                );
                return -1;
            }
            0
        }
        WM_CLOSE => {
            KillTimer(hwnd, 2);
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        WM_COMMAND => {
            app.action(wparam & 0xffff);
            0
        }
        WM_TIMER => {
            if wparam == 3 || wparam == 4 {
                if wparam == 4 {
                    KillTimer(hwnd, 4);
                }
                app.check_updates(false);
            } else if wparam == 2 {
                if IsWindowVisible(hwnd) == 0 {
                    KillTimer(hwnd, 2);
                } else {
                    let rect = RECT {
                        left: scale(80, app.dpi),
                        top: scale(160, app.dpi),
                        right: scale(320, app.dpi),
                        bottom: scale(187, app.dpi),
                    };
                    InvalidateRect(hwnd, &rect, 0);
                }
            } else {
                app.refresh();
            }
            0
        }
        DONE => {
            app.completed();
            0
        }
        TRAY => {
            match lparam as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => app.show(),
                WM_RBUTTONUP | WM_CONTEXTMENU => app.menu(),
                _ => {}
            }
            0
        }
        WM_DPICHANGED => {
            let work = work_area(hwnd);
            app.dpi = fitted_dpi((wparam as u32 & 0xffff).max(96), work);
            let rect = &*(lparam as *const RECT);
            let width = scale(view::WIDTH, app.dpi);
            let height = scale(view::HEIGHT, app.dpi);
            SetWindowPos(
                hwnd,
                ptr::null_mut(),
                rect.left.clamp(work.left, work.right - width),
                rect.top.clamp(work.top, work.bottom - height),
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            app.layout();
            0
        }
        WM_SIZE => {
            if !app.controls.is_empty() {
                app.layout();
            }
            0
        }
        WM_CTLCOLORSTATIC => {
            let dc = wparam as HDC;
            let title = lparam as HWND == app.control(APP_TITLE);
            SetBkColor(dc, if title { 0x00fcf7f2 } else { view::BACKGROUND });
            SetTextColor(
                dc,
                if lparam as HWND == app.control(LABEL_ERROR) {
                    0x003f39bd
                } else if [
                    APP_TITLE,
                    UPDATE_TITLE,
                    LABEL_STATUS,
                    LABEL_SERVICE,
                    LABEL_SOURCE,
                    LABEL_MIC_TITLE,
                    LABEL_SILENCE,
                    LABEL_SHORTCUT,
                    LABEL_MIC_VALUE,
                    LABEL_SILENCE_VALUE,
                    LABEL_WIN,
                    LABEL_PLUS,
                    LABEL_H,
                ]
                .iter()
                .any(|&id| lparam as HWND == app.control(id))
                {
                    INK
                } else {
                    MUTED
                },
            );
            if title {
                app.caption as isize
            } else {
                app.white as isize
            }
        }
        WM_DRAWITEM => {
            let draw = &*(lparam as *const DRAWITEMSTRUCT);
            if crate::menu::draw(draw) {
                return 1;
            }
            if [
                START,
                STOP,
                TOGGLE,
                MINIMIZE,
                CLOSE,
                GITHUB,
                UPDATE_DOWNLOAD,
                UPDATE_SKIP,
                UPDATE_CLOSE,
                LABEL_MIC_VALUE,
                LABEL_SILENCE_VALUE,
            ]
            .contains(&(draw.CtlID as usize))
            {
                app.draw_button(draw);
                return 1;
            }
            0
        }
        WM_ERASEBKGND => {
            // WM_PAINT draws the complete invalid region through a memory DC.
            // Erasing first exposes a blank frame during native control updates.
            1
        }
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = mem::zeroed();
            let dc = BeginPaint(hwnd, &mut paint);
            let mut client: RECT = mem::zeroed();
            GetClientRect(hwnd, &mut client);
            let memory = CreateCompatibleDC(dc);
            let bitmap = CreateCompatibleBitmap(dc, client.right, client.bottom);
            let old = SelectObject(memory, bitmap);
            FillRect(memory, &client, app.white);
            let caption = RECT {
                left: 0,
                top: 0,
                right: client.right,
                bottom: scale(view::HEADER, app.dpi),
            };
            FillRect(memory, &caption, app.caption);
            DrawIconEx(
                memory,
                scale(16, app.dpi),
                scale(10, app.dpi),
                app.icon,
                scale(22, app.dpi),
                scale(22, app.dpi),
                0,
                ptr::null_mut(),
                DI_NORMAL,
            );
            let peak = if app.animate {
                app.status.as_ref().map_or(0, |s| s.peak)
            } else {
                0
            };
            view::background(memory, app.dpi, app.ready, peak, now_ms() % 60000 / 60);
            if app.update_visible {
                view::update_bubble(memory, app.dpi, app.update_release.is_some());
            }
            BitBlt(dc, 0, 0, client.right, client.bottom, memory, 0, 0, SRCCOPY);
            SelectObject(memory, old);
            DeleteObject(bitmap);
            DeleteDC(memory);
            EndPaint(hwnd, &paint);
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, 1);
            KillTimer(hwnd, 2);
            KillTimer(hwnd, 3);
            KillTimer(hwnd, 4);
            app.tray(NIM_DELETE);
            PostQuitMessage(0);
            0
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// Used by installers and normal automation; never terminates a process.
pub fn quit() -> Result<(), String> {
    unsafe {
        let window = FindWindowW(wide(CLASS).as_ptr(), ptr::null());
        if !window.is_null() {
            let mut pid = 0;
            GetWindowThreadProcessId(window, &mut pid);
            let process = Handle(OpenProcess(SYNCHRONIZATION_SYNCHRONIZE, 0, pid));
            if process.0.is_null() {
                return Err("无法等待微win退出".to_owned());
            }
            if PostMessageW(window, WM_COMMAND, EXIT, 0) == 0 {
                return Err("无法请求微win退出".to_owned());
            }
            if WaitForSingleObject(process.0, 30_000) != WAIT_OBJECT_0 {
                return Err("微win尚未退出，安装已暂停".to_owned());
            }
        }
        let stop = Handle(OpenEventW(
            EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
            0,
            wide("Local\\WeChatWinH.Stop").as_ptr(),
        ));
        if stop.0.is_null() {
            return Ok(());
        }
        let restored = Handle(OpenEventW(
            SYNCHRONIZATION_SYNCHRONIZE,
            0,
            wide("Local\\WeChatWinH.Restored").as_ptr(),
        ));
        if restored.0.is_null() || SetEvent(stop.0) == 0 {
            return Err("无法请求后台恢复Windows原生识别".to_owned());
        }
        if WaitForSingleObject(restored.0, 30_000) != WAIT_OBJECT_0 {
            return Err("Windows原生识别尚未恢复，安装已暂停".to_owned());
        }
        Ok(())
    }
}

pub fn run() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let mutex = Handle(CreateMutexW(
            ptr::null(),
            0,
            wide("Local\\WeChatWinH.ControlPanel").as_ptr(),
        ));
        if !mutex.0.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
            let hwnd = FindWindowW(wide(CLASS).as_ptr(), ptr::null());
            if !hwnd.is_null() {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
            return;
        }
        let mut app = Box::new(App::new());
        let module = GetModuleHandleW(ptr::null());
        let class_name = wide(CLASS);
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(procedure),
            hInstance: module,
            hIcon: app.icon,
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: ptr::null_mut(),
            lpszClassName: class_name.as_ptr(),
            ..mem::zeroed()
        };
        if RegisterClassW(&class) == 0 {
            return;
        }
        let style = WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        let work = work_area(ptr::null_mut());
        let dpi = fitted_dpi(GetDpiForSystem().max(96), work);
        let rect = RECT {
            left: 0,
            top: 0,
            right: scale(view::WIDTH, dpi),
            bottom: scale(view::HEIGHT, dpi),
        };
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide(APP_NAME).as_ptr(),
            style,
            work.left + ((work.right - work.left) - (rect.right - rect.left)) / 2,
            work.top + ((work.bottom - work.top) - (rect.bottom - rect.top)) / 2,
            rect.right - rect.left,
            rect.bottom - rect.top,
            ptr::null_mut(),
            ptr::null_mut(),
            module,
            (&mut *app as *mut App).cast(),
        );
        if hwnd.is_null() {
            return;
        }
        SetWindowPos(
            hwnd,
            ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
        use windows_sys::Win32::Graphics::Dwm::*;
        let margins = windows_sys::Win32::UI::Controls::MARGINS {
            cxLeftWidth: 1,
            cxRightWidth: 1,
            cyTopHeight: 1,
            cyBottomHeight: 1,
        };
        DwmExtendFrameIntoClientArea(hwnd, &margins);
        let caption = 0x00fcf7f2_u32;
        let rounded = 2_u32;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR as u32,
            (&caption as *const u32).cast(),
            4,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&rounded as *const u32).cast(),
            4,
        );
        ShowWindow(hwnd, SW_SHOW);
        let mut message: MSG = mem::zeroed();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

//! `searchpop` – the hotkey popup.
//!
//! A tiny always-resident, unelevated Win32 program. The window is created
//! once and kept hidden; the hotkey just positions and shows it, so it appears
//! in about a millisecond. All searching is done by the elevated helper
//! (`searchd`) over a named pipe; this program only draws and launches.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("searchpop only runs on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
mod content;
#[cfg(windows)]
mod icons;
#[cfg(windows)]
mod paint;
#[cfg(windows)]
mod preview;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod worker;

#[cfg(windows)]
fn main() {
    app::run();
}

#[cfg(windows)]
use app::*;

#[cfg(windows)]
mod app {
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::ptr::{null, null_mut};
    use std::sync::{Arc, Mutex};

    use search::content::config::{self as content_config, ContentConfig};
    use search::content::status;
    use search::ipc::HitMsg;
    use search::pipe;
    use search::popup::{Config, DEFAULT_CONFIG_FILE, ListView, Theme, parse_hotkey, split_path, step_across};
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Dwm::*;
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows_sys::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
    use windows_sys::Win32::UI::Controls::{EM_GETSEL, EM_REPLACESEL, EM_SETSEL, MARGINS};
    use windows_sys::Win32::UI::HiDpi::*;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    use crate::content::{Answer as ContentAnswer, ContentRow, ContentWorker, Slot as ContentSlot};
    use crate::icons::Icons;
    use crate::paint::{Fonts, Palette};
    use crate::tray;
    use crate::worker::{Answer, Job, Outcome, Slot, Worker};

    pub const WM_APP_RESULT: u32 = WM_APP + 1;
    pub const WM_APP_ICON: u32 = WM_APP + 2;
    pub const WM_APP_TRAY: u32 = WM_APP + 3;
    pub const WM_APP_SHOW: u32 = WM_APP + 4;
    pub const WM_APP_CONTENT: u32 = WM_APP + 5;
    const TIMER_STATUS: usize = 1;

    fn class_name() -> String {
        if dev_dir().is_some() { "SearchPopupWindowDev".into() } else { "SearchPopupWindow".into() }
    }
    const HOTKEY_ID: i32 = 1;
    const IDM_SHOW: usize = 1;
    const IDM_CONFIG: usize = 2;
    const IDM_LOG: usize = 3;
    const IDM_QUIT: usize = 4;

    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    pub struct Row {
        pub path: String,
        pub is_dir: bool,
    }

    pub enum Mode {
        /// Just the input bar.
        Idle,
        Results,
        /// One line under the input bar ("No results", "Starting the helper…").
        Message(String),
    }

    /// Which list the selection is in.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Focus {
        Names,
        Content,
    }

    /// Content rows shown at once (the rest scroll).
    const CONTENT_ROWS: usize = 3;

    /// Pixel geometry for the current DPI and state.
    pub struct Layout {
        pub dpi: u32,
        pub w: i32,
        pub h: i32,
        pub pad: i32,
        pub input_h: i32,
        /// Top of the name rows; the whole list region (names + content) is `list_h` tall.
        pub list_y: i32,
        pub list_h: i32,
        /// Height of the name rows (0 when there are none).
        pub names_h: i32,
        /// Top of the "In files" caption, of its first row, and its total height (0: no section).
        pub content_y: i32,
        pub content_rows_y: i32,
        pub content_h: i32,
        pub row_h: i32,
        pub footer_y: i32,
        pub footer_h: i32,
    }

    impl Layout {
        pub fn s(&self, px: i32) -> i32 {
            (px * self.dpi as i32 + 48) / 96
        }
    }

    pub struct App {
        pub hwnd: HWND,
        pub edit: HWND,
        edit_proc: WNDPROC,
        pub cfg: Config,
        config_path: PathBuf,
        log_path: PathBuf,
        pub dpi: u32,
        pub fonts: Fonts,
        pub pal: Palette,
        bg_brush: HBRUSH,
        pub query: String,
        seq: u64,
        pub rows: Vec<Row>,
        pub total: u64,
        pub list: ListView,
        pub mode: Mode,
        /// Matches inside files, shown below the names.
        pub content: Vec<ContentRow>,
        pub content_list: ListView,
        pub focus: Focus,
        /// The content index is still being built, so the list may be incomplete.
        pub content_indexing: bool,
        content_worker: Option<ContentWorker>,
        content_slot: ContentSlot,
        /// A content search for the current text has not answered yet.
        content_pending: bool,
        /// The name search for the current text has answered.
        names_done: bool,
        content_dir: PathBuf,
        status_line: String,
        worker: Worker,
        slot: Slot,
        pub icons: Icons,
        visible: bool,
        quiet_change: bool,
        hotkey_label: String,
        taskbar_created: u32,
    }

    fn log(path: &PathBuf, msg: &str) {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{msg}");
        }
    }

    /// Development: run a second popup beside the installed one, with its own
    /// settings, helper pipe and content index.
    fn dev_dir() -> Option<PathBuf> {
        std::env::var_os("SEARCHPOP_DATA_DIR").map(PathBuf::from)
    }

    fn data_dir() -> PathBuf {
        let d = dev_dir().unwrap_or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(|d| PathBuf::from(d).join("Search"))
                .unwrap_or_else(|| std::env::temp_dir().join("Search"))
        });
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn system_prefers_dark() -> bool {
        // AppsUseLightTheme = 0 means dark.
        let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
        let val = wide("AppsUseLightTheme");
        let mut data = 1u32;
        let mut size = 4u32;
        // SAFETY: valid strings and an out buffer of the stated size.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                val.as_ptr(),
                RRF_RT_REG_DWORD,
                null_mut(),
                &mut data as *mut u32 as *mut _,
                &mut size,
            )
        };
        r == 0 && data == 0
    }

    pub fn run() {
        search::sys::quiet_errors();
        let dir = data_dir();
        let log_path = dir.join("searchpop.log");
        search::sys::log_native_crashes(log_path.clone());
        {
            let lp = log_path.clone();
            std::panic::set_hook(Box::new(move |info| log(&lp, &format!("panic: {info}"))));
        }
        // SAFETY: process-wide setup before any window exists.
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            CoInitializeEx(null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);

            // One popup per user session: a second start just shows the first.
            let dev = dev_dir().is_some();
            let mutex = if dev { "Local\\SearchPopupSingletonDev" } else { "Local\\SearchPopupSingleton" };
            let m = CreateMutexW(null(), 0, wide(mutex).as_ptr());
            if m.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
                let other = FindWindowW(wide(&class_name()).as_ptr(), null());
                if !other.is_null() {
                    PostMessageW(other, WM_APP_SHOW, 0, 0);
                }
                return;
            }
        }

        let config_path = dir.join("config.toml");
        if !config_path.exists() {
            let _ = std::fs::write(&config_path, format!("{DEFAULT_CONFIG_FILE}{}", content_config::DEFAULT_SECTION));
        }
        let config_text = std::fs::read_to_string(&config_path).unwrap_or_default();
        let (cfg, problems) = Config::parse(&config_text);
        let (content_cfg, content_problems) = ContentConfig::parse(&config_text);
        let content_problems = if config_text.is_empty() { Vec::new() } else { content_problems };
        for p in problems.iter().chain(&content_problems) {
            log(&log_path, &format!("config: {p}"));
        }

        // SAFETY: standard window-class registration and creation; the App box is
        // leaked into GWLP_USERDATA and lives as long as the window.
        unsafe {
            let hinst = GetModuleHandleW(null());
            let class_name = wide(&class_name());
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_DBLCLKS,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: class_name.as_ptr(),
                hIconSm: null_mut(),
            };
            RegisterClassExW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                class_name.as_ptr(),
                wide("Search").as_ptr(),
                WS_POPUP | WS_CLIPCHILDREN,
                0,
                0,
                100,
                100,
                null_mut(),
                null_mut(),
                hinst,
                null(),
            );
            if hwnd.is_null() {
                log(&log_path, "could not create the window");
                return;
            }
            let dpi = GetDpiForWindow(hwnd).max(96);
            let dark = match cfg.theme {
                Theme::Dark => true,
                Theme::Light => false,
                Theme::System => system_prefers_dark(),
            };
            let pal = Palette::new(dark);

            let edit = CreateWindowExW(
                0,
                wide("EDIT").as_ptr(),
                wide("").as_ptr(),
                WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32,
                0,
                0,
                10,
                10,
                hwnd,
                1 as _,
                hinst,
                null(),
            );

            let slot: Slot = Arc::new(Mutex::new(None));
            let pipe_tag = std::env::var("SEARCHPOP_PIPE_TAG").ok();
            let pipe_name = pipe::pipe_name(pipe_tag.as_deref()).unwrap_or_default();
            let worker = Worker::start(hwnd, slot.clone(), pipe_name);
            let content_dir = std::env::var_os("SEARCHPOP_CONTENT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(content_config::content_dir);
            let content_slot: ContentSlot = Arc::new(Mutex::new(None));
            let content_worker = content_cfg
                .enabled
                .then(|| ContentWorker::start(hwnd, content_slot.clone(), content_dir.clone(), content_cfg.max_results));
            let icons = Icons::start(hwnd);
            let hk = match parse_hotkey(&cfg.hotkey) {
                Ok(h) => h,
                Err(_) => parse_hotkey("Alt+Space").unwrap(),
            };

            let mut app = Box::new(App {
                hwnd,
                edit,
                edit_proc: None,
                list: ListView {
                    visible: cfg.visible_rows,
                    ..Default::default()
                },
                cfg: cfg.clone(),
                config_path,
                log_path: log_path.clone(),
                dpi,
                fonts: Fonts::new(dpi),
                pal,
                bg_brush: CreateSolidBrush(0),
                query: String::new(),
                seq: 0,
                rows: Vec::new(),
                total: 0,
                mode: Mode::Idle,
                content: Vec::new(),
                content_list: ListView { visible: CONTENT_ROWS, ..Default::default() },
                focus: Focus::Names,
                content_indexing: false,
                content_worker,
                content_slot,
                content_pending: false,
                names_done: false,
                content_dir,
                status_line: String::new(),
                worker,
                slot,
                icons,
                visible: false,
                quiet_change: false,
                hotkey_label: cfg.hotkey.clone(),
                taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            });
            app.apply_font();
            // Subclass the edit box so the arrow keys, Enter and Esc reach us.
            let old = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as usize as isize);
            app.edit_proc = std::mem::transmute::<isize, WNDPROC>(old);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);
            let app = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App);

            app.apply_theme(dark);

            app.layout_and_resize(false);

            if RegisterHotKey(hwnd, HOTKEY_ID, hk.modifiers | MOD_NOREPEAT, hk.vk) == 0 {
                let msg = format!("hotkey {} is already taken by another program", cfg.hotkey);
                log(&log_path, &msg);
                app.hotkey_label = format!("{} (unavailable: in use)", cfg.hotkey);
            }
            if let Some(i) = std::env::args().position(|a| a == "--preview") {
                // Development: paint a sample result list to a bitmap file and exit.
                let out = std::env::args().nth(i + 1).unwrap_or_else(|| "preview.bmp".into());
                app.preview(&out);
                return;
            }
            app.refresh_status(false);
            tray::add(hwnd, &app.tray_tip(), dark);
            app.worker.send(Job::Warm);
            if app.content_worker.is_some() {
                SetTimer(hwnd, TIMER_STATUS, 5000, None);
            }

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            UnregisterHotKey(hwnd, HOTKEY_ID);
        }
    }

    impl App {
        /// Colours plus the DWM look: rounded corners always; acrylic glass only
        /// for the dark theme (see `paint.rs`).
        fn apply_theme(&mut self, dark: bool) {
            self.pal = Palette::new(dark);
            // SAFETY: GDI/DWM calls on our own window; the old brush is no longer in use.
            unsafe {
                DeleteObject(self.bg_brush);
                self.bg_brush = CreateSolidBrush(self.pal.bg);
                let hwnd = self.hwnd;
                let corner = DWMWCP_ROUND;
                DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as u32, &corner as *const _ as *const _, 4);
                let backdrop = if dark { DWMSBT_TRANSIENTWINDOW } else { 1 /* DWMSBT_NONE */ };
                DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE as u32, &backdrop as *const _ as *const _, 4);
                let dark_attr: i32 = dark as i32;
                DwmSetWindowAttribute(hwnd, 20, &dark_attr as *const _ as *const _, 4); // USE_IMMERSIVE_DARK_MODE
                let e = if dark { -1 } else { 0 };
                let m = MARGINS { cxLeftWidth: e, cxRightWidth: e, cyTopHeight: e, cyBottomHeight: e };
                DwmExtendFrameIntoClientArea(hwnd, &m);
                DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR as u32, &self.pal.border as *const _ as *const _, 4);
                InvalidateRect(hwnd, null(), 0);
            }
        }

        fn tray_tip(&self) -> String {
            if self.status_line.is_empty() {
                format!("Search ({})", self.hotkey_label)
            } else {
                format!("Search ({})\n{}", self.hotkey_label, self.status_line)
            }
        }

        /// Re-read the indexer's progress file; update the tray tooltip if it changed.
        fn refresh_status(&mut self, update_tray: bool) {
            if self.content_worker.is_none() {
                return;
            }
            let line = match status::read(&self.content_dir) {
                Some(p) => p.summary(status::now_ms()),
                None => "Content search: indexer not started yet".to_string(),
            };
            if line != self.status_line {
                self.status_line = line;
                if update_tray {
                    tray::set_tip(self.hwnd, &self.tray_tip());
                }
            }
        }

        fn apply_font(&self) {
            // SAFETY: valid window and font handles.
            unsafe { SendMessageW(self.edit, WM_SETFONT, self.fonts.input as usize, 1) };
        }

        fn rows_shown(&self) -> usize {
            match self.mode {
                Mode::Results => self.rows.len().min(self.cfg.visible_rows),
                // The "no results" line gives way to the content section.
                Mode::Message(_) if !self.content.is_empty() => 0,
                Mode::Message(_) => 1,
                Mode::Idle => 0,
            }
        }

        pub fn layout(&self) -> Layout {
            let s = |px: i32| (px * self.dpi as i32 + 48) / 96;
            let w = s(self.cfg.width);
            let input_h = s(56);
            let row_h = s(52);
            let footer_h = s(28);
            let rows = self.rows_shown() as i32;
            let names_h = if rows > 0 { rows * row_h + s(4) } else { 0 };
            let crows = if matches!(self.mode, Mode::Idle) { 0 } else { self.content.len().min(CONTENT_ROWS) as i32 };
            let head_h = if crows > 0 { s(24) } else { 0 };
            let content_h = if crows > 0 { head_h + crows * row_h + s(4) } else { 0 };
            let list_h = names_h + content_h;
            let footer = if list_h > 0 { footer_h } else { 0 };
            let list_y = input_h + 1 + s(2);
            Layout {
                dpi: self.dpi,
                w,
                h: input_h + if list_h > 0 { 1 } else { 0 } + list_h + footer,
                pad: s(16),
                input_h,
                list_y,
                list_h,
                names_h,
                content_y: input_h + 1 + names_h,
                content_rows_y: input_h + 1 + names_h + head_h,
                content_h,
                row_h,
                footer_y: input_h + 1 + list_h,
                footer_h: footer,
            }
        }

        /// Recompute geometry; resize the window (keeping its position) and move the edit box.
        pub fn layout_and_resize(&mut self, keep_pos: bool) {
            let lay = self.layout();
            self.list.visible = self.cfg.visible_rows.min(self.rows.len().max(1));
            self.content_list.visible = CONTENT_ROWS.min(self.content.len().max(1));
            // SAFETY: valid window handles; the DC is released.
            unsafe {
                let mut flags = SWP_NOZORDER | SWP_NOACTIVATE;
                if keep_pos {
                    flags |= SWP_NOMOVE;
                }
                SetWindowPos(self.hwnd, null_mut(), 0, 0, lay.w, lay.h, flags | if keep_pos { 0 } else { SWP_NOMOVE });
                // Text box: one line, vertically centred in the input bar.
                let dc = GetDC(self.edit);
                let old = SelectObject(dc, self.fonts.input);
                let mut tm: TEXTMETRICW = std::mem::zeroed();
                GetTextMetricsW(dc, &mut tm);
                SelectObject(dc, old);
                ReleaseDC(self.edit, dc);
                let eh = tm.tmHeight;
                let ex = lay.pad + lay.s(36);
                MoveWindow(self.edit, ex, (lay.input_h - eh) / 2, lay.w - ex - lay.pad, eh, 1);
                InvalidateRect(self.hwnd, null(), 0);
            }
        }

        fn work_area_for_cursor(&self) -> (RECT, u32) {
            // SAFETY: plain monitor queries with valid out-structs.
            unsafe {
                let mut p = POINT { x: 0, y: 0 };
                GetCursorPos(&mut p);
                let mon = MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST);
                let mut mi: MONITORINFO = std::mem::zeroed();
                mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                GetMonitorInfoW(mon, &mut mi);
                let (mut dx, mut dy) = (96u32, 96u32);
                GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
                (mi.rcWork, dx.max(96))
            }
        }

        fn set_edit_text(&mut self, text: &str) {
            self.quiet_change = true;
            // SAFETY: valid window and NUL-terminated text.
            unsafe { SetWindowTextW(self.edit, wide(text).as_ptr()) };
            self.quiet_change = false;
        }

        pub fn show(&mut self) {
            let t0 = std::time::Instant::now();
            let (work, dpi) = self.work_area_for_cursor();
            // SAFETY: window calls on our own handles.
            unsafe {
                if dpi != self.dpi {
                    self.dpi = dpi;
                    self.fonts.destroy();
                    self.fonts = Fonts::new(dpi);
                    self.apply_font();
                }
                let dark = match self.cfg.theme {
                    Theme::Dark => true,
                    Theme::Light => false,
                    Theme::System => system_prefers_dark(),
                };
                if dark != self.pal.dark {
                    self.apply_theme(dark);
                }
                let t1 = t0.elapsed();
                self.set_edit_text("");
                self.query.clear();
                self.seq += 1;
                self.rows.clear();
                self.total = 0;
                self.mode = Mode::Idle;
                self.list.reset(0);
                self.clear_content();
                self.worker.send(Job::Query {
                    seq: self.seq,
                    text: String::new(),
                    limit: 1,
                });
                self.worker.send(Job::Warm);

                let lay = self.layout();
                let x = work.left + (work.right - work.left - lay.w) / 2;
                let y = work.top + (work.bottom - work.top) / 5;
                SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, lay.w, lay.h, SWP_NOACTIVATE);
                let t2 = t0.elapsed();
                self.layout_and_resize(true);
                let t3 = t0.elapsed();
                ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                self.visible = true;
                let t4 = t0.elapsed();
                self.grab_focus();
                if std::env::var_os("SEARCHPOP_TRACE").is_some() {
                    log(&self.log_path, &format!("  show parts: prep {t1:?}, edit+pos {t2:?}, layout {t3:?}, ShowWindow {t4:?}, focus {:?}", t0.elapsed()));
                }
            }
        }

        /// Windows only lets the foreground thread hand out focus; borrowing its
        /// input queue for a moment makes the request reliable from a hotkey.
        unsafe fn grab_focus(&self) {
            unsafe {
                // A hotkey press normally already grants us the foreground; only
                // borrow the other thread's input queue if that did not work.
                SetForegroundWindow(self.hwnd);
                if GetForegroundWindow() == self.hwnd {
                    SetFocus(self.edit);
                    return;
                }
                let fg = GetForegroundWindow();
                let me = GetCurrentThreadId();
                let fg_thread = if fg.is_null() { 0 } else { GetWindowThreadProcessId(fg, null_mut()) };
                let attached = fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, 1) != 0;
                SetForegroundWindow(self.hwnd);
                SetFocus(self.edit);
                if attached {
                    AttachThreadInput(me, fg_thread, 0);
                }
            }
        }

        pub fn hide(&mut self) {
            if self.visible {
                self.visible = false;
                // SAFETY: hiding our own window.
                unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            }
        }

        fn toggle(&mut self) {
            if self.visible {
                self.hide();
            } else {
                self.show();
            }
        }

        /// The text box changed: ask the helper.
        fn text_changed(&mut self) {
            if self.quiet_change {
                return;
            }
            let mut buf = [0u16; 1200];
            // SAFETY: buffer length passed matches the allocation.
            let n = unsafe { GetWindowTextW(self.edit, buf.as_mut_ptr(), buf.len() as i32) } as usize;
            let text = String::from_utf16_lossy(&buf[..n]);
            // The helper refuses queries over its limit; search the start of a long paste.
            let text = search::ipc::clamp_query(&text).to_string();
            if text == self.query {
                return;
            }
            self.query = text;
            self.seq += 1;
            self.names_done = false;
            if self.query.trim().is_empty() {
                self.rows.clear();
                self.mode = Mode::Idle;
                self.list.reset(0);
                self.clear_content();
                self.layout_and_resize(true);
            }
            self.worker.send(Job::Query {
                seq: self.seq,
                text: self.query.clone(),
                limit: self.cfg.max_results as u16,
            });
            // Content results follow on their own thread; the ones already on
            // screen stay until the new answer replaces them.
            if let Some(cw) = &self.content_worker {
                self.content_pending = !self.query.trim().is_empty();
                cw.send(self.seq, self.query.clone());
            }
        }

        fn clear_content(&mut self) {
            self.content.clear();
            self.content_list.reset(0);
            self.content_pending = false;
            self.focus = Focus::Names;
            if let Some(cw) = &self.content_worker {
                // Forget the previous query (and cancel anything in flight).
                cw.send(self.seq, String::new());
            }
        }

        /// Show "No results" only once neither search has anything to show.
        fn settle_empty(&mut self) {
            if self.names_done
                && !self.content_pending
                && self.rows.is_empty()
                && self.content.is_empty()
                && !self.query.trim().is_empty()
                && !matches!(self.mode, Mode::Message(_))
            {
                self.mode = Mode::Message(format!("No results for \u{201C}{}\u{201D}", self.query.trim()));
            }
        }

        fn take_content(&mut self) {
            let ans: Option<ContentAnswer> = self.content_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            let Some(ContentAnswer { seq, rows, still_indexing }) = ans else { return };
            if seq != self.seq || self.query.trim().is_empty() {
                return; // an older query's answer
            }
            self.content_pending = false;
            self.content_indexing = still_indexing;
            let same = self.content.len() == rows.len()
                && self.content.iter().zip(&rows).all(|(a, b)| a.path == b.path);
            self.content = rows;
            if !same {
                self.content_list.reset(self.content.len());
            }
            self.content_list.visible = CONTENT_ROWS.min(self.content.len().max(1));
            if self.content.is_empty() && self.focus == Focus::Content {
                self.focus = Focus::Names;
            }
            // Nothing among the names: the content hits take the selection.
            if self.rows.is_empty() && !self.content.is_empty() {
                self.focus = Focus::Content;
            }
            self.settle_empty();
            self.layout_and_resize(true);
        }

        fn take_answer(&mut self) {
            let ans: Option<Answer> = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            let Some(Answer { seq, outcome }) = ans else { return };
            if seq != self.seq || self.query.trim().is_empty() {
                return; // an older query's answer
            }
            self.names_done = true;
            match outcome {
                Outcome::Hits { total, hits } => {
                    self.total = total;
                    let keep = self.rows.first().map(|r| r.path.clone());
                    self.rows = hits.into_iter().map(|HitMsg { path, is_dir }| Row { path, is_dir }).collect();
                    if self.rows.is_empty() {
                        // No names: the content section (if any) fills the list. "No results"
                        // appears only when the content search has nothing either.
                        self.mode = Mode::Results;
                        self.list.reset(0);
                        if !self.content.is_empty() {
                            self.focus = Focus::Content;
                        }
                        self.settle_empty();
                    } else {
                        self.mode = Mode::Results;
                        self.focus = Focus::Names;
                        // Keep the selection on top while typing; only reset when the list changed.
                        if keep.as_deref() != self.rows.first().map(|r| r.path.as_str()) || self.list.len != self.rows.len() {
                            self.list.reset(self.rows.len());
                        }
                        self.list.visible = self.cfg.visible_rows.min(self.rows.len());
                    }
                }
                Outcome::Loading(why) => {
                    self.rows.clear();
                    self.list.reset(0);
                    self.mode = Mode::Message(format!("Still indexing\u{2026} ({why})"));
                }
                Outcome::Offline(msg) | Outcome::Error(msg) => {
                    self.rows.clear();
                    self.list.reset(0);
                    self.mode = Mode::Message(msg);
                }
            }
            self.layout_and_resize(true);
        }

        fn selected_path(&self) -> Option<&str> {
            if !matches!(self.mode, Mode::Results | Mode::Message(_)) {
                return None;
            }
            match self.focus {
                Focus::Content => self.content.get(self.content_list.selected).map(|r| r.path.as_str()),
                Focus::Names if matches!(self.mode, Mode::Results) => {
                    self.rows.get(self.list.selected).map(|r| r.path.as_str())
                }
                Focus::Names => None,
            }
        }

        fn refuse(&mut self, msg: String) {
            self.mode = Mode::Message(msg);
            self.rows.clear();
            self.list.reset(0);
            self.layout_and_resize(true);
        }

        pub fn open_selected(&mut self) {
            let Some(path) = self.selected_path().map(str::to_string) else { return };
            if !has_drive_root(&path) || !std::path::Path::new(&path).exists() {
                self.refuse("That file no longer exists. Search again.".into());
                return;
            }
            let (_, folder) = split_path(&path);
            // These buffers must outlive the call: a temporary created inside the
            // `if` branch below would be freed before ShellExecute reads it.
            let (verb, file, dir) = (wide("open"), wide(&path), wide(folder));
            // SAFETY: NUL-terminated strings; ShellExecute copies them.
            let r = unsafe {
                ShellExecuteW(
                    null_mut(),
                    verb.as_ptr(),
                    file.as_ptr(),
                    null(),
                    if folder.is_empty() { null() } else { dir.as_ptr() },
                    SW_SHOWNORMAL,
                ) as usize
            };
            if r <= 32 {
                log(&self.log_path, &format!("open failed ({r}): {path}"));
                self.refuse(format!("Windows couldn't open that (error {r})."));
                return;
            }
            self.hide();
        }

        pub fn reveal_selected(&mut self) {
            let Some(path) = self.selected_path().map(str::to_string) else { return };
            if !has_drive_root(&path) || !std::path::Path::new(&path).exists() {
                self.refuse("That file no longer exists. Search again.".into());
                return;
            }
            let _ = std::process::Command::new("explorer.exe")
                .raw_arg(format!("/select,\"{path}\""))
                .spawn();
            self.hide();
        }

        /// Move the selection through the name rows and on into the content rows.
        fn step(&mut self, delta: isize) {
            let names = if matches!(self.mode, Mode::Results) { self.rows.len() } else { 0 };
            let at = match self.focus {
                Focus::Names => (false, self.list.selected),
                Focus::Content => (true, self.content_list.selected),
            };
            match step_across(names, self.content.len(), at, delta) {
                (false, i) => {
                    self.focus = Focus::Names;
                    self.list.select(i);
                }
                (true, i) => {
                    self.focus = Focus::Content;
                    self.content_list.select(i);
                }
            }
            // SAFETY: invalidating our window.
            unsafe { InvalidateRect(self.hwnd, null(), 0) };
        }

        fn page(&mut self, dir: isize) {
            match self.focus {
                Focus::Names => self.list.page(dir),
                Focus::Content => self.content_list.page(dir),
            }
            // SAFETY: invalidating our window.
            unsafe { InvalidateRect(self.hwnd, null(), 0) };
        }

        /// The row under pixel row `y`, in whichever list it is.
        fn row_at(&self, y: i32) -> Option<(Focus, usize)> {
            let lay = self.layout();
            if y >= lay.content_rows_y && lay.content_h > 0 {
                let slot = ((y - lay.content_rows_y) / lay.row_h) as usize;
                let i = self.content_list.top + slot;
                return (slot < self.content_list.visible && i < self.content.len()).then_some((Focus::Content, i));
            }
            if !matches!(self.mode, Mode::Results) || y < lay.list_y || y >= lay.list_y + lay.names_h {
                return None;
            }
            let slot = ((y - lay.list_y) / lay.row_h) as usize;
            let i = self.list.top + slot;
            (slot < self.list.visible && i < self.rows.len()).then_some((Focus::Names, i))
        }

        fn show_menu(&mut self) {
            // SAFETY: standard popup-menu sequence on our own window.
            unsafe {
                let menu = CreatePopupMenu();
                let items: [(usize, String); 3] = [
                    (IDM_SHOW, format!("Search\t{}", self.cfg.hotkey)),
                    (IDM_CONFIG, "Open settings file".into()),
                    (IDM_LOG, "Open log folder".into()),
                ];
                for (id, text) in &items {
                    AppendMenuW(menu, MF_STRING, *id, wide(text).as_ptr());
                }
                AppendMenuW(menu, MF_SEPARATOR, 0, null());
                AppendMenuW(menu, MF_STRING, IDM_QUIT, wide("Quit").as_ptr());
                let mut p = POINT { x: 0, y: 0 };
                GetCursorPos(&mut p);
                SetForegroundWindow(self.hwnd);
                let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, 0, self.hwnd, null());
                PostMessageW(self.hwnd, WM_NULL, 0, 0);
                DestroyMenu(menu);
                match cmd as usize {
                    IDM_SHOW => self.show(),
                    IDM_CONFIG => {
                        ShellExecuteW(null_mut(), wide("open").as_ptr(), wide(&self.config_path.to_string_lossy()).as_ptr(), null(), null(), SW_SHOWNORMAL);
                    }
                    IDM_LOG => {
                        if let Some(dir) = self.log_path.parent() {
                            ShellExecuteW(null_mut(), wide("open").as_ptr(), wide(&dir.to_string_lossy()).as_ptr(), null(), null(), SW_SHOWNORMAL);
                        }
                    }
                    IDM_QUIT => {
                        tray::remove(self.hwnd);
                        DestroyWindow(self.hwnd);
                    }
                    _ => {}
                }
            }
        }
    }

    /// `C:\...` only: never hand the shell a path with no drive (it would try to
    /// resolve it against whatever drive or share it likes).
    fn has_drive_root(path: &str) -> bool {
        let b = path.as_bytes();
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == 0x5C // backslash
    }

    unsafe fn app_of(hwnd: HWND) -> Option<&'static mut App> {
        // SAFETY: GWLP_USERDATA is either 0 or the leaked Box<App> set in `run`.
        unsafe {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
            p.as_mut()
        }
    }

    fn ctrl_down() -> bool {
        // SAFETY: key-state query.
        unsafe { GetKeyState(VK_CONTROL as i32) < 0 }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        // SAFETY: called by the system on our window's thread.
        unsafe {
            let Some(app) = app_of(hwnd) else {
                return DefWindowProcW(hwnd, msg, wp, lp);
            };
            match msg {
                WM_HOTKEY => {
                    let t = std::time::Instant::now();
                    let was_visible = app.visible;
                    app.toggle();
                    let took = t.elapsed();
                    // The budget for showing is ~16 ms; note it when missed (or when tracing).
                    if !was_visible && (took.as_millis() > 16 || std::env::var_os("SEARCHPOP_TRACE").is_some()) {
                        log(&app.log_path, &format!("show took {took:?}"));
                    }
                    0
                }
                WM_APP_SHOW => {
                    app.show();
                    0
                }
                WM_APP_RESULT => {
                    app.take_answer();
                    0
                }
                WM_APP_CONTENT => {
                    app.take_content();
                    0
                }
                WM_TIMER if wp == TIMER_STATUS => {
                    app.refresh_status(true);
                    0
                }
                WM_APP_ICON => {
                    InvalidateRect(hwnd, null(), 0);
                    0
                }
                WM_APP_TRAY => {
                    match (lp & 0xFFFF) as u32 {
                        WM_LBUTTONUP => app.toggle(),
                        WM_RBUTTONUP | WM_CONTEXTMENU => app.show_menu(),
                        _ => {}
                    }
                    0
                }
                WM_COMMAND => {
                    if (wp >> 16) as u32 == EN_CHANGE && lp as HWND == app.edit {
                        app.text_changed();
                    }
                    0
                }
                WM_ACTIVATE => {
                    if (wp & 0xFFFF) as u32 == WA_INACTIVE {
                        app.hide();
                    }
                    0
                }
                WM_ERASEBKGND => 1,
                WM_PAINT => {
                    let mut ps: PAINTSTRUCT = std::mem::zeroed();
                    let dc = BeginPaint(hwnd, &mut ps);
                    let lay = app.layout();
                    app.paint(dc, &lay);
                    EndPaint(hwnd, &ps);
                    0
                }
                WM_CTLCOLOREDIT => {
                    SetTextColor(wp as HDC, app.pal.text);
                    SetBkColor(wp as HDC, app.pal.bg);
                    SetBkMode(wp as HDC, OPAQUE as i32);
                    app.bg_brush as LRESULT
                }
                WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                    let y = ((lp >> 16) & 0xFFFF) as i16 as i32;
                    if let Some((focus, i)) = app.row_at(y) {
                        app.focus = focus;
                        match focus {
                            Focus::Names => app.list.select(i),
                            Focus::Content => app.content_list.select(i),
                        }
                        InvalidateRect(hwnd, null(), 0);
                        if msg == WM_LBUTTONDBLCLK {
                            app.open_selected();
                        }
                    }
                    0
                }
                WM_MOUSEWHEEL => {
                    let delta = ((wp >> 16) & 0xFFFF) as i16 as i32;
                    // The wheel position is in screen coordinates.
                    let mut p = POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 };
                    ScreenToClient(hwnd, &mut p);
                    let lay = app.layout();
                    let step = if delta > 0 { -3 } else { 3 };
                    if lay.content_h > 0 && p.y >= lay.content_y {
                        app.content_list.scroll(step);
                    } else {
                        app.list.scroll(step);
                    }
                    InvalidateRect(hwnd, null(), 0);
                    0
                }
                WM_DPICHANGED => {
                    app.dpi = (wp & 0xFFFF) as u32;
                    app.fonts.destroy();
                    app.fonts = Fonts::new(app.dpi);
                    app.apply_font();
                    app.layout_and_resize(true);
                    0
                }
                // Releasing Alt after Alt+Space must not open a system menu or beep.
                WM_SYSCOMMAND if (wp & 0xFFF0) as u32 == SC_KEYMENU => 0,
                WM_SYSCHAR => 0,
                WM_NCHITTEST => HTCLIENT as LRESULT,
                WM_DESTROY => {
                    PostQuitMessage(0);
                    0
                }
                m if m == app.taskbar_created => {
                    tray::add(hwnd, &app.tray_tip(), app.pal.dark);
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
    }

    /// Placeholder text drawn over an empty text box.
    unsafe fn draw_placeholder(edit: HWND, app: &App) {
        unsafe {
            let dc = GetDC(edit);
            let old = SelectObject(dc, app.fonts.input);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, app.pal.muted);
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(edit, &mut r);
            let t: Vec<u16> = "Search files and folders".encode_utf16().collect();
            DrawTextW(dc, t.as_ptr(), t.len() as i32, &mut r, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(dc, old);
            ReleaseDC(edit, dc);
        }
    }

    /// Delete the word before the caret (Ctrl+Backspace; the stock box lacks it).
    unsafe fn delete_word_before_caret(edit: HWND) {
        unsafe {
            let mut sel_start = 0u32;
            let mut sel_end = 0u32;
            SendMessageW(edit, EM_GETSEL, &mut sel_start as *mut u32 as usize, &mut sel_end as *mut u32 as isize);
            let mut buf = [0u16; 1200];
            let n = GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32) as usize;
            let caret = (sel_start as usize).min(n);
            let mut i = caret;
            while i > 0 && buf[i - 1] == b' ' as u16 {
                i -= 1;
            }
            while i > 0 && buf[i - 1] != b' ' as u16 {
                i -= 1;
            }
            if i < caret {
                SendMessageW(edit, EM_SETSEL, i, caret as isize);
                SendMessageW(edit, EM_REPLACESEL, 1, wide("").as_ptr() as isize);
            }
        }
    }

    unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        // SAFETY: called by the system for our edit control.
        unsafe {
            let parent = GetParent(hwnd);
            let Some(app) = app_of(parent) else {
                return DefWindowProcW(hwnd, msg, wp, lp);
            };
            match msg {
                WM_KEYDOWN => {
                    let vk = wp as u16;
                    match vk {
                        VK_UP => {
                            app.step(-1);
                            return 0;
                        }
                        VK_DOWN => {
                            app.step(1);
                            return 0;
                        }
                        VK_PRIOR | VK_NEXT => {
                            app.page(if vk == VK_NEXT { 1 } else { -1 });
                            return 0;
                        }
                        VK_RETURN => {
                            if ctrl_down() {
                                app.reveal_selected();
                            } else {
                                app.open_selected();
                            }
                            return 0;
                        }
                        VK_ESCAPE => {
                            app.hide();
                            return 0;
                        }
                        VK_BACK if ctrl_down() => {
                            delete_word_before_caret(hwnd);
                            return 0;
                        }
                        0x41 if ctrl_down() => {
                            SendMessageW(hwnd, EM_SETSEL, 0, -1);
                            return 0;
                        }
                        _ => {}
                    }
                }
                // Enter, Ctrl+Enter, Esc, Tab and Ctrl+Backspace would otherwise beep or insert junk.
                WM_CHAR if matches!(wp, 0x0D | 0x0A | 0x1B | 0x09 | 0x7F) => return 0,
                WM_PAINT => {
                    let orig = app.edit_proc;
                    let r = CallWindowProcW(orig, hwnd, msg, wp, lp);
                    if GetWindowTextLengthW(hwnd) == 0 {
                        draw_placeholder(hwnd, app);
                    }
                    return r;
                }
                _ => {}
            }
            CallWindowProcW(app.edit_proc, hwnd, msg, wp, lp)
        }
    }
}

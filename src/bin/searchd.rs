//! `searchd` – the elevated helper behind the hotkey popup.
//!
//! Owns the in-RAM index (MFT scan or snapshot, then the live USN tail) and
//! answers the popup's queries over a per-user named pipe. It is started at
//! logon by a scheduled task with highest privileges (see `scripts\install.ps1`)
//! so the popup itself can stay unelevated. It never launches anything.
//!
//! Run it by hand from an elevated terminal with `--console` to watch the log.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("searchd only runs on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    imp::main()
}

#[cfg(windows)]
mod imp {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::exit;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::{Duration, Instant};

    use clap::Parser;
    use search::fmt::{commas, dur};
    use search::index::Kind;
    use search::ipc::{self, HitMsg, KindFilter, Reply, Request, StatusMsg};
    use search::live::{Config, Engine, FailureKind, SavePolicy};
    use search::mft::{self, NtfsSource};
    use search::{pipe, sys};

    #[derive(Parser)]
    #[command(name = "searchd", version, about = "Elevated index helper for the Search popup")]
    struct Cli {
        /// Drive letter(s) to index (default: every fixed NTFS drive).
        #[arg(short = 'v', long = "volume")]
        volumes: Vec<String>,
        /// Where snapshots and the log live (default: C:\Program Files\Search\data
        /// when elevated, which only admins can change; else %LOCALAPPDATA%\Search).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Print the log to the terminal this was started from.
        #[arg(long)]
        console: bool,
        /// Use a separate pipe name (for tests, so a real helper keeps running).
        #[arg(long)]
        pipe_tag: Option<String>,
        /// Ignore snapshots and rescan.
        #[arg(long)]
        rescan: bool,
        /// MFT reader threads per volume.
        #[arg(long, default_value_t = 8)]
        mft_threads: usize,
        /// Development: index this folder by walking it (no elevation, static)
        /// instead of reading the MFT.
        #[arg(long)]
        walk: Vec<PathBuf>,
    }

    /// A hidden top-level window whose only job is to hear Windows say "the
    /// session is ending" (WM_ENDSESSION) or "please close" (WM_CLOSE, which
    /// Task Scheduler sends when it stops a task), and turn that into a clean
    /// save-and-exit. Services and message-only windows never get these.
    mod session {
        use std::ptr::{null, null_mut};
        use std::sync::OnceLock;
        use std::sync::mpsc::Sender;
        use std::sync::Mutex;
        use std::time::{Duration, Instant};

        use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::System::Threading::SetProcessShutdownParameters;
        use windows_sys::Win32::UI::WindowsAndMessaging::*;

        static QUIT: OnceLock<Mutex<Sender<()>>> = OnceLock::new();

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain([0]).collect()
        }

        fn request_quit() {
            if let Some(q) = QUIT.get() {
                let _ = q.lock().unwrap_or_else(|e| e.into_inner()).send(());
            }
        }

        unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            // SAFETY: called by the system on the watcher thread.
            unsafe {
                match msg {
                    WM_QUERYENDSESSION => 1, // fine with ending; we save in WM_ENDSESSION
                    WM_ENDSESSION => {
                        if wp != 0 {
                            request_quit();
                            // The main thread saves and exits the process; give it
                            // the time Windows allows before it kills us anyway.
                            let t = Instant::now();
                            while t.elapsed() < Duration::from_secs(10) {
                                std::thread::sleep(Duration::from_millis(50));
                            }
                        }
                        0
                    }
                    WM_CLOSE => {
                        request_quit();
                        0
                    }
                    _ => DefWindowProcW(hwnd, msg, wp, lp),
                }
            }
        }

        /// `title` is `searchd-<tag>`, so a test can find its own helper.
        pub fn start(title: String, quit: Sender<()>, log: std::sync::Arc<super::Log>) {
            let _ = QUIT.set(Mutex::new(quit));
            let _ = std::thread::Builder::new().name("session-watch".into()).spawn(move || {
                // SAFETY: plain window-class registration and a message loop on this thread.
                unsafe {
                    // Be told about shutdown early, so there is time to write snapshots.
                    SetProcessShutdownParameters(0x3FF, 0);
                    let hinst = GetModuleHandleW(null());
                    let class = wide("SearchdSessionWatcher");
                    let wc = WNDCLASSEXW {
                        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                        style: 0,
                        lpfnWndProc: Some(proc),
                        cbClsExtra: 0,
                        cbWndExtra: 0,
                        hInstance: hinst,
                        hIcon: null_mut(),
                        hCursor: null_mut(),
                        hbrBackground: null_mut(),
                        lpszMenuName: null(),
                        lpszClassName: class.as_ptr(),
                        hIconSm: null_mut(),
                    };
                    RegisterClassExW(&wc);
                    // Top-level (not message-only) and never shown.
                    let hwnd = CreateWindowExW(
                        WS_EX_TOOLWINDOW,
                        class.as_ptr(),
                        wide(&title).as_ptr(),
                        WS_OVERLAPPED,
                        0,
                        0,
                        0,
                        0,
                        null_mut(),
                        null_mut(),
                        hinst,
                        null(),
                    );
                    if hwnd.is_null() {
                        log.line("could not create the session window: no save when Windows logs off or Task Scheduler stops this");
                        return;
                    }
                    let mut msg: MSG = std::mem::zeroed();
                    while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            });
        }
    }

    struct Log {
        file: Mutex<Option<std::fs::File>>,
        echo: bool,
    }

    impl Log {
        fn open(dir: &std::path::Path, echo: bool) -> Log {
            let _ = std::fs::create_dir_all(dir);
            let path = dir.join("searchd.log");
            // Keep it small: start over when it grows past 1 MB.
            if std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false) {
                let _ = std::fs::rename(&path, dir.join("searchd.old.log"));
            }
            let file = OpenOptions::new().create(true).append(true).open(&path).ok();
            Log {
                file: Mutex::new(file),
                echo,
            }
        }

        fn line(&self, s: &str) {
            let now = {
                // UTC time of day is enough to correlate with the task's own log.
                let d = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                format!("{:02}:{:02}:{:02}Z", d / 3600 % 24, d / 60 % 60, d % 60)
            };
            if let Some(f) = self.file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                let _ = writeln!(f, "{now} {s}");
            }
            if self.echo {
                eprintln!("{s}");
            }
        }
    }

    struct Shared {
        engine: RwLock<Option<Arc<Engine>>>,
        progress: Mutex<String>,
        failures: Mutex<Vec<String>>,
        quit: Mutex<mpsc::Sender<()>>,
        elevated: bool,
    }

    impl Shared {
        fn engine(&self) -> Option<Arc<Engine>> {
            self.engine.read().unwrap_or_else(|e| e.into_inner()).clone()
        }

        fn status(&self) -> Reply {
            let Some(e) = self.engine() else {
                return Reply::Loading(self.progress.lock().unwrap_or_else(|e| e.into_inner()).clone());
            };
            let vols = e.volumes();
            let mut detail: Vec<String> = vols
                .iter()
                .map(|v| {
                    format!(
                        "{} {} entries, {}{}",
                        v.label,
                        commas(v.entries),
                        if v.live { "live" } else { "not live" },
                        if v.stale { ", stale snapshot" } else { "" }
                    )
                })
                .collect();
            detail.extend(self.failures.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned());
            Reply::Status(StatusMsg {
                version: ipc::PROTOCOL_VERSION,
                entries: e.len() as u64,
                volumes: vols.len() as u16,
                elevated: self.elevated,
                stale: vols.iter().any(|v| v.stale),
                detail: detail.join("; "),
            })
        }

        fn handle(&self, req: Request) -> Reply {
            match req {
                Request::Status => self.status(),
                Request::Search { query, limit, kind } => {
                    let Some(e) = self.engine() else {
                        return Reply::Loading(self.progress.lock().unwrap_or_else(|e| e.into_inner()).clone());
                    };
                    if e.volumes().is_empty() {
                        let why = self.failures.lock().unwrap_or_else(|e| e.into_inner()).join("; ");
                        return Reply::Error(format!("no volumes are indexed: {why}"));
                    }
                    let kind = match kind {
                        KindFilter::Any => Kind::Any,
                        KindFilter::Files => Kind::Files,
                        KindFilter::Dirs => Kind::Dirs,
                    };
                    let limit = (limit as usize).clamp(1, ipc::MAX_LIMIT);
                    let r = e.search_ranked(&query, kind, limit);
                    Reply::Hits {
                        total: r.total as u64,
                        hits: r
                            .hits
                            .into_iter()
                            .map(|h| HitMsg {
                                path: h.path,
                                is_dir: h.is_dir,
                            })
                            .collect(),
                    }
                }
                Request::Save => match self.engine() {
                    Some(e) => {
                        e.save(SavePolicy {
                            force: true,
                            ..Default::default()
                        });
                        Reply::Done
                    }
                    None => Reply::Loading(self.progress.lock().unwrap_or_else(|e| e.into_inner()).clone()),
                },
                Request::Shutdown => {
                    let _ = self.quit.lock().unwrap_or_else(|e| e.into_inner()).send(());
                    Reply::Done
                }
            }
        }
    }

    pub fn main() {
        sys::quiet_errors();
        let cli = Cli::parse();
        let echo = cli.console && sys::attach_parent_console();
        let data_dir = cli.data_dir.clone().unwrap_or_else(sys::default_data_dir);
        let log = Arc::new(Log::open(&data_dir, echo));
        let elevated = sys::is_elevated();
        log.line(&format!(
            "searchd {} starting (elevated: {elevated}, data: {})",
            env!("CARGO_PKG_VERSION"),
            data_dir.display()
        ));

        let (quit_tx, quit_rx) = mpsc::channel();
        session::start(
            format!("searchd-{}", cli.pipe_tag.as_deref().unwrap_or("main")),
            quit_tx.clone(),
            log.clone(),
        );
        let shared = Arc::new(Shared {
            engine: RwLock::new(None),
            progress: Mutex::new("starting".into()),
            failures: Mutex::new(Vec::new()),
            quit: Mutex::new(quit_tx),
            elevated,
        });

        // The pipe comes up first so the popup can show "indexing…" instead of
        // "helper not running" while the snapshot loads or the MFT is scanned.
        let name = match pipe::pipe_name(cli.pipe_tag.as_deref()) {
            Ok(n) => n,
            Err(e) => {
                log.line(&format!("cannot determine the pipe name: {e}"));
                exit(1);
            }
        };
        let up = Arc::new(AtomicBool::new(false));
        {
            let (shared, log, up, name) = (shared.clone(), log.clone(), up.clone(), name.clone());
            std::thread::Builder::new()
                .name("pipe-server".into())
                .spawn(move || {
                    let handler: Arc<dyn Fn(Request) -> Reply + Send + Sync> = {
                        let shared = shared.clone();
                        Arc::new(move |r| shared.handle(r))
                    };
                    let up2 = up.clone();
                    match pipe::serve(&name, handler, move || up2.store(true, Ordering::Release)) {
                        Err(e) if pipe::is_already_running(&e) => {
                            log.line("another searchd already owns the pipe; exiting");
                            exit(0);
                        }
                        Err(e) => {
                            log.line(&format!("pipe server failed: {e}"));
                            exit(1);
                        }
                        Ok(()) => {}
                    }
                })
                .expect("spawn pipe server");
        }
        let t = Instant::now();
        while !up.load(Ordering::Acquire) && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        log.line(&format!("pipe: {name}"));

        // Load the index.
        let t = Instant::now();
        let engine = if !cli.walk.is_empty() {
            *shared.progress.lock().unwrap_or_else(|e| e.into_inner()) = "walking folders".into();
            let mut indexes = Vec::new();
            for root in &cli.walk {
                let mut b = search::index::Builder::new();
                match search::walk::index_tree(root, &mut b) {
                    Ok(st) => {
                        log.line(&format!("  {}: {} entries", root.display(), commas(st.entries)));
                        let ix = b.build();
                        indexes.push((ix.path(0).trim_end_matches('\\').to_string(), ix));
                    }
                    Err(e) => log.line(&format!("  {}: {e}", root.display())),
                }
            }
            Arc::new(Engine::from_indexes(indexes))
        } else {
            let mut letters: Vec<char> = if cli.volumes.is_empty() {
                mft::fixed_ntfs_volumes()
            } else {
                cli.volumes
                    .iter()
                    .filter_map(|v| v.trim_end_matches([':', '\\']).chars().next())
                    .collect()
            };
            letters.sort_unstable();
            letters.dedup();
            let threads = cli.mft_threads.max(1);
            let sources = letters
                .iter()
                .map(|&l| Arc::new(NtfsSource::new(l, threads)) as Arc<dyn search::live::Source>)
                .collect();
            let cfg = Config {
                data_dir: Some(data_dir),
                tail: true,
                force_rescan: cli.rescan,
                // A save only happens when worth it (see SavePolicy); checking hourly is free.
                autosave: Some(Duration::from_secs(3600)),
                log: {
                    let (log, shared) = (log.clone(), shared.clone());
                    Arc::new(move |line: &str| {
                        let line = line.trim();
                        *shared.progress.lock().unwrap_or_else(|e| e.into_inner()) = line.to_string();
                        log.line(line);
                    })
                },
                ..Default::default()
            };
            *shared.progress.lock().unwrap_or_else(|e| e.into_inner()) = format!(
                "loading {}",
                letters.iter().map(|l| format!("{l}:")).collect::<Vec<_>>().join(" ")
            );
            Arc::new(Engine::start(sources, cfg))
        };
        {
            let mut f = shared.failures.lock().unwrap_or_else(|e| e.into_inner());
            for fail in &engine.failures {
                let why = match fail.kind {
                    FailureKind::AccessDenied => "needs an elevated process (and has no snapshot)".to_string(),
                    FailureKind::NoJournal => "USN journal not active".to_string(),
                    FailureKind::Other => fail.message.clone(),
                };
                f.push(format!("{} {why}", fail.label));
                log.line(&format!("  {} not indexed: {why}", fail.label));
            }
        }
        log.line(&format!(
            "ready: {} entries on {} volume(s) in {}",
            commas(engine.len()),
            engine.volumes().len(),
            dur(t.elapsed())
        ));
        *shared.engine.write().unwrap_or_else(|e| e.into_inner()) = Some(engine.clone());

        // A volume that had to be scanned (first run, or its snapshot was no use)
        // gets a snapshot now rather than at the next hourly check. Volumes that
        // resumed from a good snapshot are left alone (SavePolicy: only when
        // there is none, enough changed, or it is old). Queries keep working.
        {
            let (engine, log) = (engine.clone(), log.clone());
            let _ = std::thread::Builder::new().name("first-save".into()).spawn(move || {
                let n = engine.save(SavePolicy::default());
                if n > 0 {
                    log.line(&format!("wrote {n} snapshot(s) after the scan"));
                }
            });
        }

        // Serve until asked to quit: the `Shutdown` request, Windows ending the
        // session, or Task Scheduler asking the window to close. A hard kill
        // (`schtasks /end` on a stuck process, power loss) skips the save; the
        // journal replay on the next start covers it.
        let _ = quit_rx.recv();
        log.line("shutdown requested; saving");
        let n = engine.save(SavePolicy {
            force: true,
            ..Default::default()
        });
        log.line(&format!("saved {n} snapshot(s); bye"));
        exit(0);
    }
}

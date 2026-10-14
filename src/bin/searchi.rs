//! `searchi` – the content indexer.
//!
//! A background program that runs as you (not elevated). It listens to the
//! helper's change events, reads files in extractor child processes
//! (`searchi --worker`, started by itself) and keeps the content index in
//! `%LOCALAPPDATA%\Search\content` up to date. The popup reads that index
//! directly; this program answers no queries.
//!
//!   searchi                 run (installed to start at logon)
//!   searchi --status        print what a running indexer is doing
//!   searchi --stop          ask a running indexer to quit
//!   searchi --walk DIR      development: crawl DIR directly, no helper needed
//!   searchi --once --walk DIR   index DIR, wait until done, print numbers, exit

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("searchi only runs on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    imp::main()
}

#[cfg(windows)]
mod imp {
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::exit;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use clap::Parser;
    use search::content::config::{self, ContentConfig, DEFAULT_SECTION};
    use search::content::{indexer, status, sys, worker};
    use search::pipe;

    /// Names of the stop event and the single-instance mutex. An indexer given its
    /// own `--dir` (tests, experiments) gets names of its own, so it neither
    /// collides with nor stops the installed one.
    fn names(dir: Option<&PathBuf>) -> (String, String) {
        let suffix = dir
            .map(|d| format!("-{:x}", search::content::store::fid_of(&d.to_string_lossy())))
            .unwrap_or_default();
        (
            format!("Local\\SearchIndexerStop{suffix}"),
            format!("Local\\SearchIndexerSingleton{suffix}"),
        )
    }

    #[derive(Parser)]
    #[command(name = "searchi", version, about = "Background content indexer for the Search popup")]
    struct Cli {
        /// Internal: run as an extractor child.
        #[arg(long, hide = true)]
        worker: bool,
        /// Print the running indexer's progress and exit.
        #[arg(long)]
        status: bool,
        /// Ask the running indexer to quit.
        #[arg(long)]
        stop: bool,
        /// Crawl these folders directly instead of asking the helper (development).
        #[arg(long)]
        walk: Vec<PathBuf>,
        /// With --walk: index, wait until done, print numbers, exit.
        #[arg(long)]
        once: bool,
        /// Where the index, status and log live.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Settings file (default: %LOCALAPPDATA%\Search\config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Pipe tag of the helper (tests only).
        #[arg(long)]
        pipe_tag: Option<String>,
        /// Ignore pause-when-active and pause-on-battery.
        #[arg(long)]
        no_pause: bool,
        /// Print the log to the terminal this was started from.
        #[arg(long)]
        console: bool,
        /// Throw the index away and start over.
        #[arg(long)]
        rebuild: bool,
        /// Development: show what would be extracted from these files, and how long it takes.
        #[arg(long)]
        extract: Vec<PathBuf>,
        /// Show what the index holds by file type and folder (for tuning what to index).
        #[arg(long)]
        report: bool,
    }

    struct Log {
        file: Mutex<Option<std::fs::File>>,
        echo: bool,
    }

    impl Log {
        fn open(dir: &std::path::Path, echo: bool) -> Log {
            let _ = std::fs::create_dir_all(dir);
            let path = dir.join("indexer.log");
            if std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false) {
                let _ = std::fs::rename(&path, dir.join("indexer.old.log"));
            }
            let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
            Log { file: Mutex::new(file), echo }
        }

        fn line(&self, s: &str) {
            let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
            let now = format!("{:02}:{:02}:{:02}Z", d / 3600 % 24, d / 60 % 60, d % 60);
            if let Some(f) = self.file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                let _ = writeln!(f, "{now} {s}");
            }
            if self.echo {
                eprintln!("{s}");
            }
        }
    }

    pub fn main() {
        search::sys::quiet_errors();
        let cli = Cli::parse();
        // The indexer and its extractors parse untrusted files: never with administrator rights
        // (an elevated shell, or a script that was run elevated, must not start one by accident).
        if search::sys::is_elevated() {
            let _ = search::sys::attach_parent_console();
            eprintln!("searchi refuses to run elevated: start it from a normal (non-administrator) window.");
            exit(1);
        }
        let echo = cli.console && search::sys::attach_parent_console();
        if cli.worker {
            if let Err(e) = worker::run_child() {
                exit(if e.kind() == std::io::ErrorKind::BrokenPipe { 0 } else { 1 });
            }
            return;
        }
        let dir = cli.dir.clone().unwrap_or_else(config::content_dir);
        let (stop_event, singleton) = names(cli.dir.as_ref());
        if !cli.extract.is_empty() {
            let _ = search::sys::attach_parent_console();
            for f in &cli.extract {
                let t = Instant::now();
                let r = worker::extract_and_chunk(&f.to_string_lossy(), 512 << 20, 1200);
                let took = t.elapsed();
                match r {
                    worker::Reply::Text { chunks, truncated } => {
                        let bytes: usize = chunks.iter().map(|c| c.text.len()).sum();
                        let first = chunks.first().map(|c| c.text.chars().take(160).collect::<String>()).unwrap_or_default();
                        println!(
                            "{}: {} passages, {} bytes{} in {:.0?}
    {first}",
                            f.display(),
                            chunks.len(),
                            bytes,
                            if truncated { " (cut)" } else { "" },
                            took
                        );
                    }
                    worker::Reply::Skipped(s) => println!("{}: skipped ({}) in {:.0?}", f.display(), s.note(), took),
                }
            }
            return;
        }
        if cli.report {
            let _ = search::sys::attach_parent_console();
            match search::content::store::report(&dir) {
                Ok(r) => {
                    println!("{} files, {} passages, {:.1} MiB of text", r.files, r.passages, r.text_bytes as f64 / 1048576.0);
                    println!("by type:");
                    for (e, files, passages, bytes) in r.by_ext.iter().take(30) {
                        println!("  {e:<8} {files:>9} files {passages:>10} passages {:>9.1} MiB", *bytes as f64 / 1048576.0);
                    }
                    println!("by folder:");
                    for (d, passages, bytes) in r.by_dir.iter().take(30) {
                        println!("  {:>10} passages {:>9.1} MiB  {d}", passages, *bytes as f64 / 1048576.0);
                    }
                }
                Err(e) => println!("cannot read the index in {}: {e}", dir.display()),
            }
            return;
        }
        if cli.status {
            let _ = search::sys::attach_parent_console();
            match status::read(&dir) {
                Some(p) => {
                    println!("{}", p.summary(status::now_ms()));
                    println!(
                        "{} indexed, {} skipped, {} failed; {} passages, {:.1} MiB on disk; {} queued, {} done, {} waiting for live updates",
                        p.indexed,
                        p.skipped,
                        p.failed,
                        p.passages,
                        p.index_bytes as f64 / 1048576.0,
                        p.queued,
                        p.done,
                        p.pending_events
                    );
                    if !p.note.is_empty() {
                        println!("note: {}", p.note);
                    }
                }
                None => println!("no status yet ({} has no status.json)", dir.display()),
            }
            return;
        }
        if cli.stop {
            let _ = search::sys::attach_parent_console();
            println!("{}", if sys::signal_stop(&stop_event) { "asked the indexer to quit" } else { "no indexer is running" });
            return;
        }

        let log = Arc::new(Log::open(&dir, echo));
        if !cli.once && !sys::single_instance(&singleton) {
            log.line("another indexer is already running; exiting");
            return;
        }
        search::sys::log_native_crashes(dir.join("indexer.log"));
        let config_path = cli.config.clone().unwrap_or_else(|| config::app_dir().join("config.toml"));
        let (cfg, problems) = ContentConfig::load(&config_path);
        for p in &problems {
            log.line(&format!("config: {p}"));
        }
        // Make the settings discoverable: add the [content] section on first run.
        if cli.config.is_none()
            && let Ok(text) = std::fs::read_to_string(&config_path)
            && !text.contains("[content]")
        {
            let _ = std::fs::write(&config_path, format!("{}\n{}", text.trim_end(), DEFAULT_SECTION));
        }
        if !cfg.enabled && !cli.once {
            log.line("content search is turned off in the settings; exiting");
            return;
        }
        if cli.rebuild {
            let _ = std::fs::remove_dir_all(dir.join("index"));
            let _ = std::fs::remove_file(dir.join("schema.ver"));
            log.line("index removed; rebuilding");
        }

        let pipe_name = if cli.walk.is_empty() {
            match pipe::pipe_name(cli.pipe_tag.as_deref()) {
                Ok(n) => Some(n),
                Err(e) => {
                    log.line(&format!("cannot determine the pipe name: {e}"));
                    exit(1);
                }
            }
        } else {
            None
        };
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("searchi.exe"));
        let mut opts = indexer::Options::new(dir.clone(), cfg, exe);
        opts.pipe_name = pipe_name;
        opts.walk_roots = cli.walk.clone();
        opts.gate = !cli.no_pause && !cli.once;
        opts.log = {
            let log = log.clone();
            Arc::new(move |s: &str| log.line(s))
        };
        log.line(&format!("searchi {} starting, index in {}", env!("CARGO_PKG_VERSION"), dir.display()));
        let running = match indexer::start(opts) {
            Ok(r) => r,
            Err(e) => {
                log.line(&format!("cannot start: {e}"));
                exit(1);
            }
        };

        if cli.once {
            let t = Instant::now();
            let ok = running.wait_idle(Duration::from_secs(24 * 3600));
            let p = running.progress();
            let _ = search::sys::attach_parent_console();
            let line = format!(
                "{}: {} indexed, {} skipped, {} failed, {} passages, {:.1} MiB, {:.1}s",
                if ok { "done" } else { "gave up" },
                p.indexed,
                p.skipped,
                p.failed,
                p.passages,
                p.index_bytes as f64 / 1048576.0,
                t.elapsed().as_secs_f64()
            );
            println!("{line}");
            log.line(&line);
            running.stop();
            return;
        }

        let stop = sys::StopEvent::create(&stop_event);
        while !running.stopping() && !stop.as_ref().is_some_and(|s| s.is_set()) {
            std::thread::sleep(Duration::from_millis(500));
        }
        log.line("stopping");
        running.stop();
        log.line("stopped");
    }
}

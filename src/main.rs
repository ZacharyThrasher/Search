//! `s` – filename search CLI over the live in-RAM index.
//!
//! Loads the saved snapshot (or scans the MFT when there is none), replays
//! the USN journal to catch up, then answers queries and reports timings. In
//! interactive mode it keeps tailing the journal, so files you create, rename
//! or delete show up in the next query.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use search::fmt::{age, commas, dur, mib};
use search::index::{Builder, Kind, QueryOpts};
use search::live::{Config, Engine, FailureKind, Hit, SavePolicy};
use search::sys;

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum Source {
    /// Read the NTFS master file table (needs an elevated shell).
    Mft,
    /// Walk directories (any file system, no elevation, much slower).
    Walk,
}

#[derive(Parser)]
#[command(
    name = "s",
    version,
    about = "Instant filename search: live in-RAM index with timings"
)]
struct Cli {
    /// Search terms (all must appear in the file name). Omit for interactive mode.
    query: Vec<String>,

    #[arg(long, value_enum, default_value_t = Source::Mft)]
    source: Source,

    /// Drive letter(s) for --source mft (default: every fixed NTFS drive).
    #[arg(short = 'v', long = "volume", value_name = "LETTER")]
    volumes: Vec<String>,

    /// Folder(s) to index for --source walk.
    #[arg(long, value_name = "PATH")]
    root: Vec<PathBuf>,

    /// Maximum results to print.
    #[arg(short = 'n', long, default_value_t = 50)]
    limit: usize,

    /// Only directories.
    #[arg(short, long, conflicts_with = "files")]
    dirs: bool,

    /// Only files.
    #[arg(short, long)]
    files: bool,

    /// Skip the exact total count (stop after the first page).
    #[arg(long)]
    no_count: bool,

    /// Run each query N times and report p50 / p95 / max latency.
    #[arg(long, value_name = "N")]
    bench: Option<usize>,

    /// Read queries from a file, one per line.
    #[arg(long, value_name = "FILE")]
    queries_file: Option<PathBuf>,

    /// Do not print result paths (useful with --bench).
    #[arg(short, long)]
    quiet: bool,

    /// Threads reading the MFT per volume (1 = serial, 0 = default of 8).
    #[arg(long, value_name = "N", default_value_t = 0)]
    mft_threads: usize,

    /// Where snapshots are kept (default: %LOCALAPPDATA%\Search, or C:\Program Files\Search\data when elevated).
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Neither read nor write snapshots: scan the MFT every run (step-1 behavior).
    #[arg(long)]
    no_snapshot: bool,

    /// Ignore the saved snapshot and rescan, then save the fresh result.
    #[arg(long, conflicts_with = "no_snapshot")]
    rescan: bool,

    /// Write snapshots on exit even if little changed.
    #[arg(long, conflicts_with = "no_snapshot")]
    save: bool,

    /// Do not tail the USN journal in interactive mode (catch up once, then freeze).
    #[arg(long)]
    no_live: bool,

    /// Create, rename and delete files under DIR and time how fast the live
    /// index reflects each change. Exits non-zero if any takes over 1 s.
    #[arg(long, value_name = "DIR")]
    live_test: Option<PathBuf>,
}

/// Measured on a 32-thread Ryzen: C: (14.8M records) reads in 14.6 s at 8
/// threads but 17 s at 32, because NTFS contends on the volume.
#[cfg(windows)]
const DEFAULT_MFT_THREADS: usize = 8;

fn parse_letter(s: &str) -> Option<char> {
    let c = s.trim().chars().next()?;
    c.is_ascii_alphabetic().then(|| c.to_ascii_uppercase())
}

/// Drive letter of an absolute path such as `C:\Users\x`.
fn drive_of(path: &Path) -> Option<char> {
    use std::path::{Component, Prefix};
    let abs = std::path::absolute(path).ok()?;
    match abs.components().next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(b) | Prefix::VerbatimDisk(b) => Some((b as char).to_ascii_uppercase()),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(windows)]
fn mft_sources(cli: &Cli) -> Result<Vec<Arc<dyn search::live::Source>>, String> {
    use search::mft::{self, NtfsSource};
    let mut letters: Vec<char> = if !cli.volumes.is_empty() {
        cli.volumes
            .iter()
            .map(|v| parse_letter(v).ok_or_else(|| format!("bad drive letter: {v}")))
            .collect::<Result<_, _>>()?
    } else if let Some(dir) = &cli.live_test {
        vec![drive_of(dir).ok_or_else(|| format!("no drive letter in {}", dir.display()))?]
    } else {
        mft::fixed_ntfs_volumes()
    };
    letters.sort_unstable();
    letters.dedup();
    if letters.is_empty() {
        return Err("no fixed NTFS volumes found".into());
    }
    let threads = if cli.mft_threads == 0 {
        DEFAULT_MFT_THREADS.min(rayon::current_num_threads())
    } else {
        cli.mft_threads
    };
    Ok(letters
        .into_iter()
        .map(|l| Arc::new(NtfsSource::new(l, threads)) as Arc<dyn search::live::Source>)
        .collect())
}

#[cfg(not(windows))]
fn mft_sources(_: &Cli) -> Result<Vec<Arc<dyn search::live::Source>>, String> {
    Err("--source mft is only available on Windows".into())
}

fn config(cli: &Cli, tail: bool) -> Config {
    Config {
        data_dir: (!cli.no_snapshot).then(|| cli.data_dir.clone().unwrap_or_else(search::sys::default_data_dir)),
        tail,
        force_rescan: cli.rescan,
        log: Arc::new(|line| eprintln!("{line}")),
        ..Default::default()
    }
}

fn build(cli: &Cli, tail: bool) -> Result<Engine, String> {
    let t = Instant::now();
    let engine = match cli.source {
        Source::Mft => {
            let sources = mft_sources(cli)?;
            let cfg = config(cli, tail);
            if let Some(d) = &cfg.data_dir {
                eprintln!("snapshots: {}", d.display());
            }
            let engine = Engine::start(sources, cfg);
            let mut denied = false;
            for f in &engine.failures {
                match f.kind {
                    FailureKind::AccessDenied => {
                        denied = true;
                        eprintln!("  {} access denied", f.label);
                    }
                    FailureKind::NoJournal => eprintln!(
                        "  {} USN journal is not active on this volume (skipped; it is never created)",
                        f.label
                    ),
                    FailureKind::Other => eprintln!("  {} {}", f.label, f.message),
                }
            }
            if denied {
                eprintln!(
                    "\nRaw volume access needs an elevated shell. Re-run from an Administrator\n\
                     terminal, or index folders without elevation: s --source walk --root C:\\Users"
                );
            }
            engine
        }
        Source::Walk => {
            if cli.root.is_empty() {
                return Err("--source walk needs at least one --root PATH".into());
            }
            let mut indexes = Vec::new();
            for root in &cli.root {
                let mut b = Builder::new();
                let s = search::walk::index_tree(root, &mut b)
                    .map_err(|e| format!("{}: {e}", root.display()))?;
                eprintln!(
                    "  {}: {:>12} entries ({} unreadable)",
                    root.display(),
                    commas(s.entries),
                    s.errors
                );
                let ix = b.build();
                indexes.push((ix.path(0).trim_end_matches('\\').to_string(), ix));
            }
            Engine::from_indexes(indexes)
        }
    };
    if engine.volumes().is_empty() {
        return Err("nothing was indexed".into());
    }
    let total = t.elapsed();
    eprintln!(
        "ready: {} entries in {}",
        commas(engine.len()),
        dur(total)
    );
    let n = engine.len();
    let per_entry = if n == 0 {
        0.0
    } else {
        engine.heap_bytes() as f64 / n as f64
    };
    eprint!(
        "index memory: {} ({per_entry:.0} B/entry)",
        mib(engine.heap_bytes())
    );
    if let Some(m) = sys::process_memory() {
        eprint!(
            "   process working set: {} (peak {})",
            mib(m.working_set),
            mib(m.peak_working_set)
        );
    }
    eprintln!();
    Ok(engine)
}

fn kind_of(cli: &Cli) -> Kind {
    match (cli.dirs, cli.files) {
        (true, _) => Kind::Dirs,
        (_, true) => Kind::Files,
        _ => Kind::Any,
    }
}

fn print_hits(hits: &[Hit], total: usize, exact: bool) {
    let out = io::stdout();
    let mut out = out.lock();
    for h in hits {
        let _ = writeln!(out, "{}{}", h.path, if h.is_dir { "\\" } else { "" });
    }
    if total > hits.len() {
        let _ = writeln!(
            out,
            "... {}{} total",
            commas(total),
            if exact { "" } else { "+" }
        );
    }
}

fn run_once(engine: &Engine, cli: &Cli, q: &str) {
    let opts = QueryOpts {
        limit: cli.limit,
        count_total: !cli.no_count,
        kind: kind_of(cli),
    };
    let t = Instant::now();
    let r = engine.search(q, opts);
    let search_t = t.elapsed();
    if !cli.quiet {
        print_hits(&r.hits, r.total, opts.count_total);
    }
    eprintln!(
        "\"{q}\": {} results in {} (incl. paths for {} shown)",
        commas(r.total),
        dur(search_t),
        r.hits.len(),
    );
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

fn bench_variant(engine: &Engine, q: &str, base: QueryOpts, runs: usize) -> (usize, [Duration; 3]) {
    for _ in 0..5 {
        std::hint::black_box(engine.search(q, base));
    }
    let mut times = Vec::with_capacity(runs);
    let mut total = 0;
    for _ in 0..runs {
        let t = Instant::now();
        let r = engine.search(q, base);
        times.push(t.elapsed());
        total = r.total;
        std::hint::black_box(&r);
    }
    times.sort();
    (
        total,
        [
            percentile(&times, 0.5),
            percentile(&times, 0.95),
            *times.last().unwrap(),
        ],
    )
}

fn run_bench(engine: &Engine, cli: &Cli, q: &str, runs: usize) {
    let kind = kind_of(cli);
    let (total, full) = bench_variant(
        engine,
        q,
        QueryOpts {
            limit: cli.limit,
            count_total: true,
            kind,
        },
        runs,
    );
    let (_, page) = bench_variant(
        engine,
        q,
        QueryOpts {
            limit: cli.limit,
            count_total: false,
            kind,
        },
        runs,
    );
    println!(
        "{:<28} {:>11} hits | full count p50 {:>9} p95 {:>9} max {:>9} | first page p50 {:>9} p95 {:>9}",
        format!("\"{q}\""),
        commas(total),
        dur(full[0]),
        dur(full[1]),
        dur(full[2]),
        dur(page[0]),
        dur(page[1]),
    );
}

fn print_stats(engine: &Engine) {
    for v in engine.volumes() {
        eprintln!(
            "  {}: {} entries ({} dead), {}, {}, {} journal records applied{}",
            v.label,
            commas(v.entries),
            commas(v.dead),
            mib(v.heap_bytes),
            if v.stale {
                "STALE (snapshot only)"
            } else if v.live {
                "live"
            } else {
                "static"
            },
            commas(v.applied as usize),
            v.snapshot_age
                .map(|a| format!(", snapshot {} old", age(a)))
                .unwrap_or_default()
        );
    }
}

fn main() -> ExitCode {
    sys::quiet_errors();
    let cli = Cli::parse();

    let mut queries: Vec<String> = Vec::new();
    if !cli.query.is_empty() {
        queries.push(cli.query.join(" "));
    }
    if let Some(f) = &cli.queries_file {
        match std::fs::read_to_string(f) {
            Ok(text) => queries.extend(
                text.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(String::from),
            ),
            Err(e) => {
                eprintln!("error: {}: {e}", f.display());
                return ExitCode::from(1);
            }
        }
    }
    let interactive = queries.is_empty() && cli.live_test.is_none();
    let tail = (interactive && !cli.no_live) || cli.live_test.is_some();
    if cli.live_test.is_some() && cli.source != Source::Mft {
        eprintln!("error: --live-test needs --source mft");
        return ExitCode::from(1);
    }

    eprintln!(
        "source: {}   elevated: {}   threads: {}",
        if cli.source == Source::Mft {
            "NTFS MFT + USN journal"
        } else {
            "directory walk"
        },
        if sys::is_elevated() { "yes" } else { "no" },
        rayon::current_num_threads()
    );
    let mut engine = match build(&cli, tail) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };

    let mut code = ExitCode::SUCCESS;
    if let Some(dir) = &cli.live_test {
        eprintln!("\nlive test in {} (budget {} per change):", dir.display(), dur(search::selftest::BUDGET));
        match search::selftest::run(&engine, dir, &|l| eprintln!("{l}")) {
            Ok(true) => eprintln!("PASS"),
            Ok(false) => {
                eprintln!("FAIL");
                code = ExitCode::from(2);
            }
            Err(e) => {
                eprintln!("error: {e}");
                code = ExitCode::from(1);
            }
        }
    } else if interactive {
        eprintln!(
            "\ninteractive mode: type a query, :stats for index state, empty line or :q to quit{}",
            if tail { "" } else { " (live updates off)" }
        );
        let stdin = io::stdin();
        loop {
            eprint!("> ");
            let _ = io::stderr().flush();
            let mut line = String::new();
            if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let line = line.trim();
            if line.is_empty() || line == ":q" {
                break;
            }
            if line == ":stats" {
                print_stats(&engine);
                continue;
            }
            run_once(&engine, &cli, line);
        }
    } else {
        for q in &queries {
            match cli.bench {
                Some(n) => run_bench(&engine, &cli, q, n.max(1)),
                None => run_once(&engine, &cli, q),
            }
        }
    }

    engine.stop();
    engine.save(SavePolicy {
        force: cli.save,
        ..Default::default()
    });
    code
}

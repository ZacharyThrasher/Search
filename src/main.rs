//! `s` – timing CLI for the in-RAM filename index.
//!
//! Builds the index (MFT by default), reports how long that took and how much
//! memory it uses, then answers queries and reports per-query latency.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use search::index::{Builder, Index, Kind, QueryOpts};
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
    about = "Instant filename search: in-RAM index with timings"
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

    /// Threads reading the MFT per volume (1 = serial, 0 = one per core).
    #[arg(long, value_name = "N", default_value_t = 0)]
    mft_threads: usize,
}

fn fmt_dur(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us < 1000.0 {
        format!("{us:.0} µs")
    } else if us < 1e6 {
        format!("{:.2} ms", us / 1e3)
    } else {
        format!("{:.2} s", us / 1e6)
    }
}

fn mib(bytes: usize) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

fn commas(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn parse_letter(s: &str) -> Option<char> {
    let c = s.trim().chars().next()?;
    c.is_ascii_alphabetic().then(|| c.to_ascii_uppercase())
}

#[cfg(windows)]
fn build_mft(cli: &Cli, builder: &mut Builder) -> Result<usize, String> {
    use search::mft;
    let letters: Vec<char> = if cli.volumes.is_empty() {
        mft::fixed_ntfs_volumes()
    } else {
        cli.volumes
            .iter()
            .map(|v| parse_letter(v).ok_or_else(|| format!("bad drive letter: {v}")))
            .collect::<Result<_, _>>()?
    };
    if letters.is_empty() {
        return Err("no fixed NTFS volumes found".into());
    }
    let mut denied = false;
    let mut done = 0;
    for letter in letters {
        let threads = if cli.mft_threads == 0 {
            rayon::current_num_threads()
        } else {
            cli.mft_threads
        };
        match mft::index_volume(letter, builder, threads) {
            Ok(s) => {
                done += 1;
                eprintln!(
                    "  {}: {:>12} entries ({} MFT records est.)  read {} (summed ioctl {})  merge {}  resolve {}  {} calls, {}",
                    s.letter,
                    commas(s.entries),
                    commas(s.mft_records_estimate as usize),
                    fmt_dur(s.enumerate),
                    fmt_dur(s.ioctl),
                    fmt_dur(s.parse),
                    fmt_dur(s.resolve),
                    s.ioctl_calls,
                    mib(s.bytes as usize),
                );
                if s.orphans > 0 {
                    eprintln!(
                        "     note: {} entries had an unknown parent",
                        commas(s.orphans)
                    );
                }
            }
            Err(e) => {
                let code = e.raw_os_error();
                if code == Some(mft::ERROR_ACCESS_DENIED) {
                    denied = true;
                    eprintln!("  {letter}: access denied");
                } else if code == Some(mft::ERROR_JOURNAL_NOT_ACTIVE) {
                    eprintln!("  {letter}: USN journal is not active on this volume (skipped)");
                } else {
                    eprintln!("  {letter}: {e}");
                }
            }
        }
    }
    if denied {
        eprintln!(
            "\nRaw volume access needs an elevated shell. Re-run from an Administrator\n\
             terminal, or index folders without elevation: s --source walk --root C:\\Users"
        );
    }
    Ok(done)
}

#[cfg(not(windows))]
fn build_mft(_: &Cli, _: &mut Builder) -> Result<usize, String> {
    Err("--source mft is only available on Windows".into())
}

fn build(cli: &Cli) -> Result<Index, String> {
    let mut builder = Builder::new();
    let t = Instant::now();
    let volumes = match cli.source {
        Source::Mft => build_mft(cli, &mut builder)?,
        Source::Walk => {
            if cli.root.is_empty() {
                return Err("--source walk needs at least one --root PATH".into());
            }
            let mut n = 0;
            for root in &cli.root {
                let s = search::walk::index_tree(root, &mut builder)
                    .map_err(|e| format!("{}: {e}", root.display()))?;
                eprintln!(
                    "  {}: {:>12} entries ({} unreadable)",
                    root.display(),
                    commas(s.entries),
                    s.errors
                );
                n += 1;
            }
            n
        }
    };
    if volumes == 0 {
        return Err("nothing was indexed".into());
    }
    let tb = Instant::now();
    let ix = builder.build();
    let finish = tb.elapsed();
    let total = t.elapsed();
    eprintln!(
        "indexed {} entries in {} (final compaction {})",
        commas(ix.len()),
        fmt_dur(total),
        fmt_dur(finish)
    );
    let per_entry = if ix.is_empty() {
        0.0
    } else {
        ix.heap_bytes() as f64 / ix.len() as f64
    };
    eprint!(
        "index memory: {} ({per_entry:.0} B/entry)",
        mib(ix.heap_bytes())
    );
    if let Some(m) = sys::process_memory() {
        eprint!(
            "   process working set: {} (peak {})",
            mib(m.working_set),
            mib(m.peak_working_set)
        );
    }
    eprintln!();
    Ok(ix)
}

fn kind_of(cli: &Cli) -> Kind {
    match (cli.dirs, cli.files) {
        (true, _) => Kind::Dirs,
        (_, true) => Kind::Files,
        _ => Kind::Any,
    }
}

fn print_hits(ix: &Index, hits: &[u32], total: usize, exact: bool) {
    let out = io::stdout();
    let mut out = out.lock();
    for &h in hits {
        let _ = writeln!(
            out,
            "{}{}",
            ix.path(h),
            if ix.is_dir(h) { "\\" } else { "" }
        );
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

fn run_once(ix: &Index, cli: &Cli, q: &str) {
    let opts = QueryOpts {
        limit: cli.limit,
        count_total: !cli.no_count,
        kind: kind_of(cli),
    };
    let t = Instant::now();
    let r = ix.search(q, opts);
    let search_t = t.elapsed();
    let t = Instant::now();
    if !cli.quiet {
        print_hits(ix, &r.hits, r.total, opts.count_total);
    }
    let paths_t = t.elapsed();
    eprintln!(
        "\"{q}\": {} results in {} (paths for {} shown: {})",
        commas(r.total),
        fmt_dur(search_t),
        r.hits.len(),
        fmt_dur(paths_t)
    );
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

fn bench_variant(ix: &Index, q: &str, base: QueryOpts, runs: usize) -> (usize, [Duration; 3]) {
    for _ in 0..5 {
        std::hint::black_box(ix.search(q, base));
    }
    let mut times = Vec::with_capacity(runs);
    let mut total = 0;
    for _ in 0..runs {
        let t = Instant::now();
        let r = ix.search(q, base);
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

fn run_bench(ix: &Index, cli: &Cli, q: &str, runs: usize) {
    let kind = kind_of(cli);
    let (total, full) = bench_variant(
        ix,
        q,
        QueryOpts {
            limit: cli.limit,
            count_total: true,
            kind,
        },
        runs,
    );
    let (_, page) = bench_variant(
        ix,
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
        fmt_dur(full[0]),
        fmt_dur(full[1]),
        fmt_dur(full[2]),
        fmt_dur(page[0]),
        fmt_dur(page[1]),
    );
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    eprintln!(
        "source: {}   elevated: {}   threads: {}",
        if cli.source == Source::Mft {
            "NTFS MFT (FSCTL_ENUM_USN_DATA)"
        } else {
            "directory walk"
        },
        if sys::is_elevated() { "yes" } else { "no" },
        rayon::current_num_threads()
    );
    let ix = match build(&cli) {
        Ok(ix) => ix,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };

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

    if queries.is_empty() {
        eprintln!("\ninteractive mode: type a query, empty line or :q to quit");
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
            run_once(&ix, &cli, line);
        }
        return ExitCode::SUCCESS;
    }

    for q in &queries {
        match cli.bench {
            Some(n) => run_bench(&ix, &cli, q, n.max(1)),
            None => run_once(&ix, &cli, q),
        }
    }
    ExitCode::SUCCESS
}

//! `searchq` – ask the running helper (`searchd`) a question over the pipe.
//! Handy for checking the helper without the popup, and for timing the
//! round trip the popup makes.
//!
//!   searchq notepad            ranked search
//!   searchq --status           what the helper has loaded
//!   searchq --save | --shutdown
//!   searchq --bench 200 notepad    latency over 200 identical queries

#[cfg(not(windows))]
fn main() {
    eprintln!("searchq only runs on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use clap::Parser;
    use search::fmt::dur;
    use search::ipc::{KindFilter, Reply, Request};
    use search::pipe;

    #[derive(Parser)]
    #[command(name = "searchq", version, about = "Query the running searchd helper")]
    struct Cli {
        /// Search terms.
        query: Vec<String>,
        #[arg(long)]
        status: bool,
        #[arg(long)]
        save: bool,
        #[arg(long)]
        shutdown: bool,
        /// Pipe tag the helper was started with (tests only).
        #[arg(long)]
        pipe_tag: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u16,
        /// Repeat the query this many times and report latency percentiles.
        #[arg(long)]
        bench: Option<usize>,
        /// Seconds to wait for the helper to appear and finish loading.
        #[arg(long, default_value_t = 0)]
        wait: u64,
        /// Search file contents (the index `searchi` keeps) instead of names.
        #[arg(long)]
        content: bool,
        /// Content index folder (default: %LOCALAPPDATA%\Search\content).
        #[arg(long)]
        content_dir: Option<std::path::PathBuf>,
    }

    search::sys::quiet_errors();
    let cli = Cli::parse();
    if cli.content {
        return content_query(&cli.query.join(" "), cli.content_dir.clone(), cli.limit as usize, cli.bench);
    }
    let name = match pipe::pipe_name(cli.pipe_tag.as_deref()) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("cannot determine the pipe name: {e}");
            return ExitCode::from(2);
        }
    };
    let mut client = match pipe::Client::connect(&name, Duration::from_secs(cli.wait)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot reach the helper at {name}: {e}");
            return ExitCode::from(3);
        }
    };
    let req = if cli.status {
        Request::Status
    } else if cli.save {
        Request::Save
    } else if cli.shutdown {
        Request::Shutdown
    } else {
        Request::Search {
            query: cli.query.join(" "),
            limit: cli.limit,
            kind: KindFilter::Any,
        }
    };

    // Optionally wait for loading to finish.
    let start = Instant::now();
    let mut said = String::new();
    let reply = loop {
        match client.request(&req) {
            Ok(Reply::Loading(why)) if start.elapsed() < Duration::from_secs(cli.wait) => {
                if why != said {
                    eprintln!("helper is loading: {why}");
                    said = why;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(r) => break r,
            Err(e) => {
                eprintln!("request failed: {e}");
                return ExitCode::from(4);
            }
        }
    };
    match &reply {
        Reply::Hits { total, hits } => {
            for h in hits {
                println!("{}{}", h.path, if h.is_dir { "\\" } else { "" });
            }
            println!("{} shown, {total} total", hits.len());
        }
        Reply::Status(s) => {
            println!(
                "protocol {}, {} entries on {} volume(s), elevated: {}, stale: {}",
                s.version, s.entries, s.volumes, s.elevated, s.stale
            );
            println!("{}", s.detail);
        }
        Reply::Done => println!("ok"),
        Reply::Loading(w) => {
            println!("helper is still loading: {w}");
            return ExitCode::from(5);
        }
        Reply::Error(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(6);
        }
        _ => {
            eprintln!("unexpected reply");
            return ExitCode::from(6);
        }
    }

    if let Some(n) = cli.bench {
        let mut times = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Instant::now();
            if client.request(&req).is_err() {
                eprintln!("request failed during the benchmark");
                return ExitCode::from(4);
            }
            times.push(t.elapsed());
        }
        times.sort();
        let p = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
        println!(
            "round trip over {n} runs: min {}  p50 {}  p95 {}  max {}",
            dur(times[0]),
            dur(p(0.5)),
            dur(p(0.95)),
            dur(*times.last().unwrap())
        );
    }
    ExitCode::SUCCESS
}

/// `searchq --content words`: query the content index directly and time it.
#[cfg(windows)]
fn content_query(
    text: &str,
    dir: Option<std::path::PathBuf>,
    limit: usize,
    bench: Option<usize>,
) -> std::process::ExitCode {
    use search::content::{config, search::ContentReader, status};
    use search::fmt::dur;
    use std::process::ExitCode;
    use std::time::Instant;

    let dir = dir.unwrap_or_else(config::content_dir);
    let t = Instant::now();
    let reader = match ContentReader::open(&dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("no content index in {} ({e}); is searchi running?", dir.display());
            return ExitCode::from(3);
        }
    };
    let opened = t.elapsed();
    if let Some(p) = status::read(&dir) {
        println!("{}", p.summary(status::now_ms()));
    }
    let t = Instant::now();
    let hits = reader.search(text, limit);
    let first = t.elapsed();
    for h in &hits {
        let loc = if h.page > 0 { format!(" (page {})", h.page) } else { String::new() };
        println!("{:7.2}  {}{loc}
         {}", h.score, h.path, h.snippet);
    }
    println!("{} files, first query {} (open {}), {} passages", hits.len(), dur(first), dur(opened), reader.passages());
    if let Some(n) = bench {
        let mut times: Vec<std::time::Duration> = (0..n)
            .map(|_| {
                let t = Instant::now();
                let _ = reader.search(text, limit);
                t.elapsed()
            })
            .collect();
        times.sort();
        let p = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
        println!("over {n} runs: min {}  p50 {}  p95 {}  max {}", dur(times[0]), dur(p(0.5)), dur(p(0.95)), dur(*times.last().unwrap()));
    }
    ExitCode::SUCCESS
}

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
    }

    search::sys::quiet_errors();
    let cli = Cli::parse();
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

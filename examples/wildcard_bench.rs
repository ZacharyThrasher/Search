//! Times the query language on a synthetic index shaped like a big Windows
//! machine (default 15.7M names, ~1.3M of them folders).
//!
//!   cargo run --release --example wildcard_bench [entries]
//!   RAYON_NUM_THREADS=1 cargo run --release --example wildcard_bench   # single-thread view

use std::time::{Duration, Instant};

use search::index::{Builder, Kind};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const WORDS: &[&str] = &[
    "report",
    "invoice",
    "photo",
    "screenshot",
    "notes",
    "readme",
    "config",
    "index",
    "main",
    "test",
    "data",
    "backup",
    "image",
    "video",
    "music",
    "document",
    "project",
    "release",
    "build",
    "cache",
    "temp",
    "log",
    "settings",
    "icon",
    "logo",
    "banner",
    "reaction",
    "meme",
    "holiday",
    "family",
    "budget",
    "resume",
    "draft",
    "final",
    "copy",
    "old",
    "new",
    "setup",
    "driver",
    "update",
    "module",
    "package",
    "plugin",
    "theme",
    "font",
    "style",
    "script",
    "helper",
    "service",
    "client",
    "server",
    "model",
    "view",
    "controller",
    "util",
    "common",
    "core",
    "base",
    "lib",
    "vendor",
];

const EXTS: &[(&str, u32)] = &[
    ("dll", 14),
    ("png", 9),
    ("txt", 6),
    ("js", 9),
    ("json", 7),
    ("h", 5),
    ("cpp", 3),
    ("rs", 2),
    ("md", 3),
    ("jpg", 5),
    ("gif", 1),
    ("pdf", 2),
    ("xml", 4),
    ("cs", 3),
    ("py", 3),
    ("pyc", 4),
    ("mui", 4),
    ("exe", 2),
    ("lib", 2),
    ("log", 2),
    ("", 6),
    ("ini", 1),
    ("html", 2),
    ("css", 2),
    ("svg", 2),
    ("ts", 3),
    ("map", 2),
    ("zip", 1),
];

fn main() {
    let total: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(15_700_000);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let dirs = total / 12;
    let weight_sum: u32 = EXTS.iter().map(|e| e.1).sum();

    let t = Instant::now();
    let mut b = Builder::new();
    b.reserve(total, total * 26);
    b.begin_volume("C:", 5);
    let mut dir_ids: Vec<u64> = vec![5];
    let mut name = String::new();
    for i in 0..dirs {
        let parent = dir_ids[rng.below(dir_ids.len().min(1 + i / 3 + 8))];
        name.clear();
        name.push_str(WORDS[rng.below(WORDS.len())]);
        if rng.below(3) > 0 {
            name.push_str(&format!("{}", rng.below(5000)));
        }
        let id = 10 + i as u64;
        b.push(id, parent, &name, true);
        dir_ids.push(id);
    }
    for i in dirs..total {
        let parent = dir_ids[rng.below(dir_ids.len())];
        name.clear();
        name.push_str(WORDS[rng.below(WORDS.len())]);
        if rng.below(2) == 0 {
            name.push('_');
            name.push_str(WORDS[rng.below(WORDS.len())]);
        }
        if rng.below(4) > 0 {
            name.push_str(&format!("{}", rng.below(100_000)));
        }
        let mut pick = rng.below(weight_sum as usize) as u32;
        for (e, w) in EXTS {
            if pick < *w {
                if !e.is_empty() {
                    name.push('.');
                    name.push_str(e);
                }
                break;
            }
            pick -= w;
        }
        b.push(10 + i as u64, parent, &name, false);
    }
    let ix = b.build();
    println!(
        "{} entries, {:.0} MiB, built in {:.1}s, {} rayon threads\n",
        ix.len(),
        ix.heap_bytes() as f64 / 1048576.0,
        t.elapsed().as_secs_f64(),
        rayon::current_num_threads()
    );

    let queries: &[(&str, &str)] = &[
        ("plain substring (today)", "report"),
        ("plain, 2 terms (today)", "report 2024"),
        ("plain, rare", "reaction_meme4"),
        ("plain, one letter (today)", "e"),
        ("just a star", "*"),
        ("star dot (typing *.png)", "*.p"),
        ("extension glob", "*.png"),
        ("extension glob, rare", "*.gif"),
        ("ext: shorthand", "ext:png;jpg"),
        ("glob suffix", "*_reaction.gif"),
        ("glob prefix", "report*"),
        ("glob middle", "rep*t_inv*.pdf"),
        ("? wildcard", "?og.txt"),
        ("glob + plain", "report *.pdf"),
        ("OR of globs", "*.png|*.gif|*.jpg"),
        ("NOT", "report !*.dll"),
        ("regex", r"regex:^report\d+\.pdf$"),
        ("regex + plain", r"regex:\d{5}\.png$ meme"),
        ("path + glob", r"photo1\*.png"),
        ("path only (substring)", r"photo1\report"),
        ("no literal at all", "???.??"),
        ("path, nothing alive", r"zzzz\"),
        ("negated name", "!zzzz"),
        ("broken regex", "regex:("),
    ];
    println!(
        "{:<26} {:<26} {:>12} {:>10} {:>10} {:>10}",
        "case", "query", "matches", "ranked", "count only", "first 50"
    );
    let runs: usize = std::env::var("RUNS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(5);
    let only = std::env::args().nth(2);
    for (label, q) in queries {
        if only
            .as_ref()
            .is_some_and(|o| !q.contains(o.as_str()) && !label.contains(o.as_str()))
        {
            continue;
        }
        let time = |f: &dyn Fn() -> usize| -> (usize, Duration) {
            let mut best = Duration::MAX;
            let mut n = 0;
            for _ in 0..runs {
                let t = Instant::now();
                n = f();
                best = best.min(t.elapsed());
            }
            (n, best)
        };
        let (matches, ranked) = time(&|| ix.search_ranked(q, Kind::Any, 50).total);
        let (_, counted) = time(&|| {
            ix.search(
                q,
                search::index::QueryOpts {
                    limit: 0,
                    count_total: true,
                    kind: Kind::Any,
                },
            )
            .total
        });
        let (_, first) = time(&|| {
            ix.search(
                q,
                search::index::QueryOpts {
                    limit: 50,
                    count_total: false,
                    kind: Kind::Any,
                },
            )
            .hits
            .len()
        });
        println!(
            "{:<26} {:<26} {:>12} {:>8.1}ms {:>8.1}ms {:>8.1}ms",
            label,
            q,
            matches,
            ranked.as_secs_f64() * 1e3,
            counted.as_secs_f64() * 1e3,
            first.as_secs_f64() * 1e3
        );
    }
}

//! `s --live-test DIR`: proves the live path end to end on a real volume by
//! creating, renaming and deleting files under `DIR` and timing how long the
//! running engine takes to reflect each change in search results.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::index::QueryOpts;
use crate::live::Engine;

/// A change must show up within this long to pass.
pub const BUDGET: Duration = Duration::from_secs(1);
const GIVE_UP: Duration = Duration::from_secs(8);

struct Step {
    what: &'static str,
    took: Option<Duration>,
    /// Informational steps are printed but never fail the run.
    informational: bool,
}

fn paths(engine: &Engine, token: &str) -> Vec<String> {
    engine
        .search(
            token,
            QueryOpts {
                limit: 20,
                count_total: false,
                ..Default::default()
            },
        )
        .hits
        .into_iter()
        .map(|h| h.path)
        .collect()
}

/// Poll until `ok(search(token))`, returning the elapsed time or `None` on timeout.
fn wait_for(engine: &Engine, token: &str, ok: impl Fn(&[String]) -> bool) -> Option<Duration> {
    let t = Instant::now();
    loop {
        if ok(&paths(engine, token)) {
            return Some(t.elapsed());
        }
        if t.elapsed() > GIVE_UP {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Run the checks, printing one line per step through `say`. True if every
/// non-informational step met `BUDGET`.
pub fn run(engine: &Engine, dir: &Path, say: &dyn Fn(&str)) -> std::io::Result<bool> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let u = format!("lvtst{:x}", nanos % 0xFFFF_FFFF_FFFF);
    fs::create_dir_all(dir)?;
    let (a, b) = (dir.join(format!("{u}-a.txt")), dir.join(format!("{u}-b.txt")));
    let (d1, d2) = (dir.join(format!("{u}-dir")), dir.join(format!("{u}-dir2")));
    let inner = format!("{u}-inner.txt");
    let held = dir.join(format!("{u}-held.txt"));

    let mut steps: Vec<Step> = Vec::new();
    let mut step = |what, informational, took| {
        steps.push(Step {
            what,
            took,
            informational,
        })
    };
    let ends = |suffix: &str| {
        let suffix = suffix.to_ascii_lowercase();
        move |p: &[String]| p.len() == 1 && p[0].to_ascii_lowercase().ends_with(&suffix)
    };

    // 1. create (written and closed, like most programs do)
    fs::write(&a, b"x")?;
    step(
        "create file",
        false,
        wait_for(engine, &format!("{u}-a"), ends(&format!("{u}-a.txt"))),
    );

    // 2. rename: new name appears, old name disappears
    fs::rename(&a, &b)?;
    let t = wait_for(engine, &format!("{u}-b"), ends(&format!("{u}-b.txt")));
    let gone = wait_for(engine, &format!("{u}-a"), |p| p.is_empty());
    step("rename file", false, t.zip(gone).map(|(x, y)| x.max(y)));

    // 3. new directory with a file inside
    fs::create_dir(&d1)?;
    fs::write(d1.join(&inner), b"x")?;
    step(
        "create folder + file inside",
        false,
        wait_for(engine, &format!("{u}-inner"), |p| {
            p.len() == 1 && p[0].to_ascii_lowercase().contains(&format!("{u}-dir\\"))
        }),
    );

    // 4. rename the folder: the file inside must follow
    fs::rename(&d1, &d2)?;
    step(
        "rename folder (child path follows)",
        false,
        wait_for(engine, &format!("{u}-inner"), |p| {
            p.len() == 1 && p[0].to_ascii_lowercase().contains(&format!("{u}-dir2\\"))
        }),
    );

    // 5. delete everything
    fs::remove_file(d2.join(&inner))?;
    fs::remove_dir(&d2)?;
    fs::remove_file(&b)?;
    step(
        "delete file, folder, renamed file",
        false,
        wait_for(engine, &u, |p| p.is_empty()),
    );

    // 6. informational: a file created and kept open (journal records for
    // open handles can lag the close).
    let mut f = File::create(&held)?;
    f.write_all(b"x")?;
    step(
        "create file, handle kept open",
        true,
        wait_for(engine, &format!("{u}-held"), ends(&format!("{u}-held.txt"))),
    );
    drop(f);
    fs::remove_file(&held)?;
    step(
        "delete that file",
        true,
        wait_for(engine, &u, |p| p.is_empty()),
    );

    let mut pass = true;
    for s in &steps {
        let verdict = match s.took {
            Some(d) if d <= BUDGET => "ok",
            Some(_) if s.informational => "slow (informational)",
            Some(_) => "TOO SLOW",
            None if s.informational => "never seen (informational)",
            None => "NEVER SEEN",
        };
        let took = s
            .took
            .map_or("-".to_string(), |d| format!("{:.1} ms", d.as_secs_f64() * 1e3));
        say(&format!("  {:<38} {:>12}   {verdict}", s.what, took));
        if !s.informational && !matches!(s.took, Some(d) if d <= BUDGET) {
            pass = false;
        }
    }
    Ok(pass)
}

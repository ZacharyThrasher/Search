//! Research helper for the step 5 embedding study: runs the real step 4
//! extractor and chunker over folders and prints chunks-per-file by extension.
//! With `--sample N --out FILE` it also writes every Nth chunk as JSON lines
//! (`{"ext":..,"text":..}`) to feed the embedding speed benchmark.
//!
//!   cargo run --release --example embed_chunk_stats -- [--sample N --out f.jsonl] DIR...

use search::content::{chunk, config::DEFAULT_EXTENSIONS, extract};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

#[derive(Default)]
struct Acc {
    files: u64,
    chunks: u64,
    text_bytes: u64,
    per_file: Vec<u32>,
    uniq: std::collections::HashSet<u64>,
}

fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            let n = e.file_name().to_string_lossy().to_ascii_lowercase();
            if n == ".git" || n == "target" || n == "node_modules" {
                continue;
            }
            walk(&p, out);
        } else if ft.is_file() {
            out.push(p);
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let (mut sample, mut outp) = (0u64, None);
    let mut dirs = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--sample" => sample = args.next().unwrap().parse().unwrap(),
            "--out" => outp = Some(args.next().unwrap()),
            _ => dirs.push(PathBuf::from(a)),
        }
    }
    let mut files = Vec::new();
    for d in &dirs {
        walk(d, &mut files);
    }
    let mut out = outp.map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    let mut by_ext: BTreeMap<String, Acc> = BTreeMap::new();
    let mut n = 0u64;
    for f in files {
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if !DEFAULT_EXTENSIONS.contains(&ext.as_str()) {
            continue;
        }
        let Ok(segs) = extract::extract(&f, 20 << 20) else { continue };
        let (chunks, _) = chunk::chunk_segments(&segs, 1200);
        let a = by_ext.entry(ext.clone()).or_default();
        a.files += 1;
        a.chunks += chunks.len() as u64;
        a.per_file.push(chunks.len() as u32);
        for c in &chunks {
            a.text_bytes += c.text.len() as u64;
            {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                c.text.hash(&mut h);
                a.uniq.insert(h.finish());
            }
            n += 1;
            if let (Some(o), true) = (out.as_mut(), sample > 0 && n % sample == 0) {
                let line = serde_json::json!({"ext": ext, "text": c.text});
                writeln!(o, "{line}").unwrap();
            }
        }
    }
    println!("{:8} {:>8} {:>9} {:>8} {:>7} {:>7} {:>9} {:>8}", "ext", "files", "chunks", "avg/file", "median", "p90", "avgbytes", "unique%");
    let (mut tf, mut tc) = (0u64, 0u64);
    for (ext, mut a) in by_ext {
        a.per_file.sort();
        let pf = |q: f64| a.per_file[((a.per_file.len() - 1) as f64 * q) as usize];
        println!(
            "{:8} {:>8} {:>9} {:>8.1} {:>7} {:>7} {:>9.0} {:>7.1}%",
            ext,
            a.files,
            a.chunks,
            a.chunks as f64 / a.files as f64,
            pf(0.5),
            pf(0.9),
            a.text_bytes as f64 / a.chunks.max(1) as f64,
            100.0 * a.uniq.len() as f64 / a.chunks.max(1) as f64
        );
        tf += a.files;
        tc += a.chunks;
    }
    println!("TOTAL files={tf} chunks={tc} avg/file={:.1}", tc as f64 / tf.max(1) as f64);
}

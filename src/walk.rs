//! Directory-walking fallback. Needs no elevation and works on any file
//! system (FAT32, ReFS, network shares), at the cost of speed. Also gives the
//! MFT path an independent ground truth to be compared against.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::index::Builder;

#[derive(Debug, Default, Clone)]
pub struct WalkStats {
    pub entries: usize,
    /// Directories or entries that could not be read (permissions etc.).
    pub errors: usize,
}

struct Rec {
    id: u64,
    parent: u64,
    name: String,
    is_dir: bool,
}

struct Ctx {
    next_id: AtomicU64,
    errors: AtomicUsize,
    out: Mutex<Vec<Vec<Rec>>>,
}

fn walk_dir<'s>(dir: PathBuf, id: u64, ctx: &'s Ctx, scope: &rayon::Scope<'s>) {
    let rd = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => {
            ctx.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let mut local = Vec::new();
    for ent in rd {
        let Ok(ent) = ent else {
            ctx.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        // `file_type` does not follow links, so symlinks and junctions are
        // listed but never descended into (no cycles, no double counting).
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let cid = ctx.next_id.fetch_add(1, Ordering::Relaxed);
        local.push(Rec {
            id: cid,
            parent: id,
            name: ent.file_name().to_string_lossy().into_owned(),
            is_dir,
        });
        if is_dir {
            let path = ent.path();
            scope.spawn(move |s| walk_dir(path, cid, ctx, s));
        }
    }
    ctx.out.lock().unwrap().push(local);
}

/// Walk `root` in parallel and add it to `builder` as one volume-like tree.
pub fn index_tree(root: &Path, builder: &mut Builder) -> io::Result<WalkStats> {
    let abs = std::path::absolute(root)?;
    if !abs.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is not a directory", abs.display()),
        ));
    }
    let mut label = abs.to_string_lossy().into_owned();
    while label.len() > 1 && (label.ends_with('\\') || label.ends_with('/')) {
        label.pop();
    }
    const ROOT_ID: u64 = 0;
    let ctx = Ctx {
        next_id: AtomicU64::new(1),
        errors: AtomicUsize::new(0),
        out: Mutex::new(Vec::new()),
    };
    rayon::scope(|s| walk_dir(abs, ROOT_ID, &ctx, s));

    builder.begin_volume(&label, ROOT_ID);
    let mut entries = 0;
    for batch in ctx.out.into_inner().unwrap() {
        for r in batch {
            builder.push(r.id, r.parent, &r.name, r.is_dir);
            entries += 1;
        }
    }
    builder.end_volume();
    Ok(WalkStats {
        entries,
        errors: ctx.errors.load(Ordering::Relaxed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Builder, QueryOpts};

    #[test]
    fn walks_a_small_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/Deep File.TXT"), b"x").unwrap();
        fs::write(root.join("top.txt"), b"x").unwrap();

        let mut b = Builder::new();
        let stats = index_tree(root, &mut b).unwrap();
        let ix = b.build();
        assert_eq!(stats.entries, 4); // a, a/b, deep file, top.txt
        assert_eq!(ix.len(), 5); // + synthesized root

        let r = ix.search("deep file", QueryOpts::default());
        assert_eq!(r.total, 1);
        let p = ix.path(r.hits[0]);
        assert!(
            p.ends_with("\\a\\b\\Deep File.TXT") || p.ends_with("/a/b/Deep File.TXT"),
            "{p}"
        );
        assert_eq!(ix.orphans(), 0);
    }

    #[test]
    fn missing_root_is_an_error() {
        let mut b = Builder::new();
        assert!(index_tree(Path::new("Z:\\definitely\\not\\here"), &mut b).is_err());
    }
}

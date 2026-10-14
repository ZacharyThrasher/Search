//! The tantivy index: schema, the single writer (owned by the indexer) and
//! the manifest of files seen.
//!
//! Two kinds of documents share the index:
//!
//! * a **chunk** per passage of a file: `path`, `plow`, `fid`, `body`,
//!   `start`, `end`, `page`;
//! * a **manifest** entry per file: `path`, `plow`, `mfid`, `msize`, `mtime`,
//!   `mstatus`. It records that the file was looked at (even when it produced
//!   no text), so a restart or a repeated crawl skips unchanged files. It lives
//!   in the same index as the chunks so a commit makes both visible together:
//!   there is no second store that can disagree with the first.
//!
//! Files are keyed by a 64-bit hash of the lower-cased path (`fid_of`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;
use tantivy::query::RegexQuery;
use tantivy::schema::{Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions};
use tantivy::tokenizer::{
    AsciiFoldingFilter, Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, TextAnalyzer,
};
use tantivy::{Index, IndexWriter, TantivyDocument, Term, doc};

use super::chunk::Chunk;
use super::dir::PatientDirectory;
use super::extract::Status;

/// Bumped when the schema or analysis changes; an index with another version
/// is thrown away and rebuilt.
pub const SCHEMA_VERSION: u32 = 1;

/// Tokenizer used for `body`: words, lower case, accents folded, English stems.
pub const TOKENIZER: &str = "ctext";
/// The same without stemming: for prefix matching and highlighting.
pub const TOKENIZER_RAW: &str = "craw";

#[derive(Clone, Copy)]
pub struct Fields {
    pub path: Field,
    pub plow: Field,
    pub fid: Field,
    pub body: Field,
    pub start: Field,
    pub end: Field,
    pub page: Field,
    pub mfid: Field,
    pub msize: Field,
    pub mtime: Field,
    pub mstatus: Field,
}

pub fn build_schema() -> (Schema, Fields) {
    use tantivy::schema::NumericOptions;
    let mut b = Schema::builder();
    let body_opts = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let indexed = NumericOptions::default().set_indexed();
    let fast = NumericOptions::default().set_fast();
    let fast_indexed = NumericOptions::default().set_indexed().set_fast();
    let f = Fields {
        path: b.add_text_field("path", STRING | STORED),
        plow: b.add_text_field("plow", STRING),
        fid: b.add_u64_field("fid", indexed),
        body: b.add_text_field("body", body_opts),
        start: b.add_u64_field("start", STORED),
        end: b.add_u64_field("end", STORED),
        page: b.add_u64_field("page", STORED),
        mfid: b.add_u64_field("mfid", fast_indexed),
        msize: b.add_u64_field("msize", fast.clone()),
        mtime: b.add_u64_field("mtime", fast.clone()),
        mstatus: b.add_u64_field("mstatus", fast),
    };
    (b.build(), f)
}

pub fn register_tokenizers(index: &Index) {
    let raw = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(64))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build();
    let stemmed = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(64))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .filter(Stemmer::new(Language::English))
        .build();
    index.tokenizers().register(TOKENIZER_RAW, raw);
    index.tokenizers().register(TOKENIZER, stemmed);
}

/// Stable 64-bit key for a path (case-insensitive). FNV-1a with a final mix;
/// it must never change between versions or old entries would be orphaned.
pub fn fid_of(path: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for ch in path.chars().flat_map(char::to_lowercase) {
        let mut buf = [0u8; 4];
        for &b in ch.encode_utf8(&mut buf).as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    // splitmix64 finaliser
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

pub fn lower_path(path: &str) -> String {
    path.to_lowercase()
}

fn version_file(dir: &Path) -> PathBuf {
    dir.join("schema.ver")
}

fn index_dir(dir: &Path) -> PathBuf {
    dir.join("index")
}

fn io_err<E: std::fmt::Display>(e: E) -> tantivy::TantivyError {
    tantivy::TantivyError::SystemError(e.to_string())
}

/// Open the index in `dir` for writing, creating it (or recreating it if its
/// schema version differs). Returns whether it was created from scratch.
pub fn open_for_write(dir: &Path) -> tantivy::Result<(Index, Fields, bool)> {
    std::fs::create_dir_all(dir).map_err(io_err)?;
    let idx = index_dir(dir);
    let current = std::fs::read_to_string(version_file(dir))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    let mut fresh = false;
    if current != Some(SCHEMA_VERSION) {
        if idx.exists() {
            std::fs::remove_dir_all(&idx).map_err(io_err)?;
        }
        fresh = true;
    }
    std::fs::create_dir_all(&idx).map_err(io_err)?;
    let (schema, f) = build_schema();
    // Passage text dominates the index; zstd stores it in roughly a third less
    // space than the default lz4, and reading a block for a snippet is as fast.
    let compressor = tantivy::store::Compressor::Zstd(tantivy::store::ZstdCompressor::default());
    let index = Index::builder()
        .schema(schema)
        .settings(tantivy::IndexSettings { docstore_compression: compressor, ..Default::default() })
        .open_or_create(PatientDirectory::open(&idx)?)?;
    std::fs::write(version_file(dir), SCHEMA_VERSION.to_string()).map_err(io_err)?;
    register_tokenizers(&index);
    Ok((index, f, fresh))
}

/// Open an existing index for searching. `Err` if there is none (yet).
pub fn open_for_read(dir: &Path) -> tantivy::Result<(Index, Fields)> {
    let ver = std::fs::read_to_string(version_file(dir)).ok().and_then(|s| s.trim().parse::<u32>().ok());
    if ver != Some(SCHEMA_VERSION) {
        return Err(io_err("no content index of this version"));
    }
    let index = Index::open(PatientDirectory::open(index_dir(dir))?)?;
    register_tokenizers(&index);
    let (_, f) = build_schema();
    Ok((index, f))
}

/// What the manifest remembers about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManEntry {
    pub size: u64,
    pub mtime: u64,
    pub status: Status,
}

pub type Manifest = FxHashMap<u64, ManEntry>;

/// Read every manifest entry from the committed segments.
pub fn load_manifest(index: &Index, f: &Fields) -> tantivy::Result<Manifest> {
    let reader = index.reader_builder().reload_policy(tantivy::ReloadPolicy::Manual).try_into()?;
    let searcher = reader.searcher();
    let mut out = Manifest::default();
    for seg in searcher.segment_readers() {
        let ff = seg.fast_fields();
        let fid = ff.u64("mfid")?;
        let size = ff.u64("msize")?;
        let mtime = ff.u64("mtime")?;
        let status = ff.u64("mstatus")?;
        let _ = f;
        for d in 0..seg.max_doc() {
            if let Some(alive) = seg.alive_bitset()
                && !alive.is_alive(d)
            {
                continue;
            }
            let Some(id) = fid.first(d) else { continue };
            out.insert(
                id,
                ManEntry {
                    size: size.first(d).unwrap_or(0),
                    mtime: mtime.first(d).unwrap_or(0),
                    status: Status::from_u64(status.first(d).unwrap_or(5)),
                },
            );
        }
    }
    Ok(out)
}

/// Escape a string for a tantivy-fst regex.
pub fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Everything known about one file once it has been looked at.
#[derive(Debug, Clone)]
pub struct FileDoc {
    pub path: String,
    pub size: u64,
    pub mtime: u64,
    pub status: Status,
    pub chunks: Vec<Chunk>,
}

pub struct Writer {
    index: Index,
    writer: IndexWriter,
    f: Fields,
    /// Documents added since the last commit.
    pub pending: usize,
}

impl Writer {
    /// `heap` bytes for tantivy's indexing buffer.
    pub fn open(dir: &Path, heap: usize) -> tantivy::Result<(Writer, bool)> {
        let (index, f, fresh) = open_for_write(dir)?;
        let writer = index.writer_with_num_threads(1, heap)?;
        Ok((Writer { index, writer, f, pending: 0 }, fresh))
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn fields(&self) -> &Fields {
        &self.f
    }

    /// Replace everything known about `d.path` with `d`.
    pub fn put(&mut self, d: FileDoc) -> tantivy::Result<()> {
        let f = self.f;
        let fid = fid_of(&d.path);
        self.remove_fid(fid);
        let plow = lower_path(&d.path);
        let mut m = TantivyDocument::default();
        m.add_text(f.path, &d.path);
        m.add_text(f.plow, &plow);
        m.add_u64(f.mfid, fid);
        m.add_u64(f.msize, d.size);
        m.add_u64(f.mtime, d.mtime);
        m.add_u64(f.mstatus, d.status as u64);
        self.writer.add_document(m)?;
        self.pending += 1;
        for c in d.chunks {
            self.writer.add_document(doc!(
                f.path => d.path.as_str(),
                f.plow => plow.as_str(),
                f.fid => fid,
                f.body => c.text,
                f.start => c.start,
                f.end => c.end,
                f.page => c.page as u64,
            ))?;
            self.pending += 1;
        }
        Ok(())
    }

    pub fn remove_fid(&mut self, fid: u64) {
        self.writer.delete_term(Term::from_field_u64(self.f.fid, fid));
        self.writer.delete_term(Term::from_field_u64(self.f.mfid, fid));
        self.pending += 1;
    }

    pub fn remove(&mut self, path: &str) {
        self.remove_fid(fid_of(path));
    }

    /// Remove every file below the folder `dir` (a path without trailing slash).
    pub fn remove_folder(&mut self, dir: &str) -> tantivy::Result<()> {
        let mut prefix = lower_path(dir);
        if !prefix.ends_with('\\') {
            prefix.push('\\');
        }
        let q = RegexQuery::from_pattern(&format!("{}.*", regex_escape(&prefix)), self.f.plow)?;
        self.writer.delete_query(Box::new(q))?;
        self.pending += 1;
        Ok(())
    }

    pub fn commit(&mut self) -> tantivy::Result<()> {
        self.writer.commit()?;
        self.pending = 0;
        Ok(())
    }

    /// Wait for background merges (used at shutdown and in tests).
    pub fn finish(self) -> tantivy::Result<()> {
        self.writer.wait_merging_threads()
    }
}

/// Counts for the status readout.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    pub docs: u64,
    pub bytes_on_disk: u64,
}

pub fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata()
                && m.is_file()
            {
                total += m.len();
            }
        }
    }
    total
}

pub fn index_files_dir(dir: &Path) -> PathBuf {
    index_dir(dir)
}

/// Used by tests and the CLI to summarise a manifest.
pub fn status_counts(m: &Manifest) -> HashMap<u8, u64> {
    let mut out = HashMap::new();
    for e in m.values() {
        *out.entry(e.status as u8).or_insert(0) += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::chunk::Chunk;

    pub fn file(path: &str, texts: &[&str]) -> FileDoc {
        FileDoc {
            path: path.into(),
            size: 10,
            mtime: 20,
            status: Status::Indexed,
            chunks: texts
                .iter()
                .enumerate()
                .map(|(i, t)| Chunk { start: i as u64 * 100, end: i as u64 * 100 + t.len() as u64, page: 0, text: (*t).into() })
                .collect(),
        }
    }

    #[test]
    fn fid_is_stable_and_case_insensitive() {
        assert_eq!(fid_of(r"C:\Users\Zach\A.txt"), fid_of(r"c:\users\zach\a.TXT"));
        assert_ne!(fid_of(r"C:\a.txt"), fid_of(r"C:\b.txt"));
        // Pinned: changing the hash would orphan every stored entry.
        assert_eq!(fid_of("c:\\a.txt"), 839_577_135_239_710_256);
    }

    #[test]
    fn put_replace_remove_and_manifest_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (mut w, fresh) = Writer::open(dir.path(), 15_000_000).unwrap();
        assert!(fresh);
        w.put(file(r"C:\a\one.txt", &["alpha beta", "gamma delta"])).unwrap();
        w.put(file(r"C:\a\sub\two.txt", &["epsilon"])).unwrap();
        w.put(FileDoc { path: r"C:\a\bin.dat".into(), size: 5, mtime: 6, status: Status::Binary, chunks: vec![] }).unwrap();
        w.commit().unwrap();
        let m = load_manifest(w.index(), w.fields()).unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m[&fid_of(r"C:\a\bin.dat")].status, Status::Binary);
        assert_eq!(m[&fid_of(r"c:\A\ONE.txt")], ManEntry { size: 10, mtime: 20, status: Status::Indexed });

        // Replacing a file drops its old chunks and manifest entry.
        w.put(file(r"C:\a\one.txt", &["zeta"])).unwrap();
        w.commit().unwrap();
        let m = load_manifest(w.index(), w.fields()).unwrap();
        assert_eq!(m.len(), 3);

        w.remove(r"C:\a\one.txt");
        w.commit().unwrap();
        assert_eq!(load_manifest(w.index(), w.fields()).unwrap().len(), 2);

        // A folder takes everything below it, and nothing that merely shares a prefix.
        w.put(file(r"C:\a2\three.txt", &["eta"])).unwrap();
        w.commit().unwrap();
        w.remove_folder(r"C:\A").unwrap();
        w.commit().unwrap();
        let m = load_manifest(w.index(), w.fields()).unwrap();
        assert_eq!(m.len(), 1);
        assert!(m.contains_key(&fid_of(r"C:\a2\three.txt")));
        w.finish().unwrap();
    }

    #[test]
    fn manifest_survives_a_reopen_and_a_schema_bump_resets() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut w, _) = Writer::open(dir.path(), 15_000_000).unwrap();
            w.put(file(r"C:\x.txt", &["hello"])).unwrap();
            w.commit().unwrap();
            w.finish().unwrap();
        }
        let (w, fresh) = Writer::open(dir.path(), 15_000_000).unwrap();
        assert!(!fresh);
        assert_eq!(load_manifest(w.index(), w.fields()).unwrap().len(), 1);
        w.finish().unwrap();
        std::fs::write(super::version_file(dir.path()), "0").unwrap();
        let (w, fresh) = Writer::open(dir.path(), 15_000_000).unwrap();
        assert!(fresh);
        assert_eq!(load_manifest(w.index(), w.fields()).unwrap().len(), 0);
    }

    #[test]
    fn regex_escape_covers_path_characters() {
        let p = r"c:\dir (x86)\a+b[1].txt";
        let esc = regex_escape(p);
        assert!(esc.contains(r"\\") && esc.contains(r"\(") && esc.contains(r"\+") && esc.contains(r"\["));
    }

    /// Folder names full of regex syntax still remove exactly their own files.
    #[test]
    fn removing_a_folder_with_special_characters_in_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let (mut w, _) = Writer::open(dir.path(), 15_000_000).unwrap();
        let weird = r"C:\Users\Zach\Docs (x86) [old]+{v2}$^&#~-.a|b\Résumé";
        w.put(file(&format!(r"{weird}\one.txt"), &["alpha"])).unwrap();
        w.put(file(&format!(r"{weird}\sub\two.txt"), &["beta"])).unwrap();
        w.put(file(r"C:\Users\Zach\Docs (x86) [old]+{v2}$^&#~-.a|bX\three.txt", &["gamma"])).unwrap();
        w.put(file(r"C:\Users\Zach\Docs\four.txt", &["delta"])).unwrap();
        w.commit().unwrap();
        assert_eq!(load_manifest(w.index(), w.fields()).unwrap().len(), 4);
        w.remove_folder(weird).unwrap();
        w.commit().unwrap();
        let m = load_manifest(w.index(), w.fields()).unwrap();
        assert_eq!(m.len(), 2, "{} entries left", m.len());
        assert!(m.contains_key(&fid_of(r"C:\Users\Zach\Docs\four.txt")));
        // Case-insensitive, like the file system.
        w.remove_folder(r"c:\USERS\zach\DOCS").unwrap();
        w.commit().unwrap();
        assert_eq!(load_manifest(w.index(), w.fields()).unwrap().len(), 1);
    }
}

/// What the index holds, by file type and by folder: for tuning what to index.
#[derive(Debug, Default)]
pub struct Report {
    pub passages: u64,
    pub files: u64,
    pub text_bytes: u64,
    /// (extension, files, passages, text bytes), biggest first.
    pub by_ext: Vec<(String, u64, u64, u64)>,
    /// (folder prefix, passages, text bytes), biggest first.
    pub by_dir: Vec<(String, u64, u64)>,
}

pub fn report(dir: &Path) -> tantivy::Result<Report> {
    use tantivy::schema::Value;
    let (index, f) = open_for_read(dir)?;
    let reader = index.reader_builder().reload_policy(tantivy::ReloadPolicy::Manual).try_into()?;
    let searcher = reader.searcher();
    let mut by_ext: HashMap<String, (u64, u64, u64)> = HashMap::new();
    let mut by_dir: HashMap<String, (u64, u64)> = HashMap::new();
    let mut r = Report::default();
    for seg in searcher.segment_readers() {
        let store = seg.get_store_reader(16)?;
        for doc in store.iter::<TantivyDocument>(seg.alive_bitset()) {
            let doc = doc?;
            let Some(path) = doc.get_first(f.path).and_then(|v| v.as_str()) else { continue };
            let ext = path.rsplit('.').next().filter(|e| e.len() <= 8 && !e.contains('\\')).unwrap_or("").to_lowercase();
            match doc.get_first(f.body).and_then(|v| v.as_str()) {
                Some(body) => {
                    r.passages += 1;
                    r.text_bytes += body.len() as u64;
                    let e = by_ext.entry(ext).or_default();
                    e.1 += 1;
                    e.2 += body.len() as u64;
                    let key: String = path.split('\\').take(4).collect::<Vec<_>>().join("\\");
                    let d = by_dir.entry(key).or_default();
                    d.0 += 1;
                    d.1 += body.len() as u64;
                }
                None => {
                    r.files += 1;
                    by_ext.entry(ext).or_default().0 += 1;
                }
            }
        }
    }
    r.by_ext = by_ext.into_iter().map(|(k, v)| (k, v.0, v.1, v.2)).collect();
    r.by_ext.sort_by_key(|e| std::cmp::Reverse(e.3));
    r.by_dir = by_dir.into_iter().map(|(k, v)| (k, v.0, v.1)).collect();
    r.by_dir.sort_by_key(|e| std::cmp::Reverse(e.2));
    Ok(r)
}

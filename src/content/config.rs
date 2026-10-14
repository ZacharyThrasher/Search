//! The `[content]` section of `%LOCALAPPDATA%\Search\config.toml`.
//!
//! The popup reads the top-level keys of the same file and ignores this
//! section; the indexer and the popup's content results read this section.
//! A bad value never stops anything: the problem is reported and the default
//! is used.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::events::Filter;

/// Extensions indexed by default: documents, notes and source code, plus the
/// few human-written config formats. Machine-generated data and markup
/// (`json jsonl xml html css csv ...`) are left out: on a developer PC they
/// were over 40% of all text and almost never what a search is after. Add the
/// ones you want to `extensions`.
pub const DEFAULT_EXTENSIONS: &[&str] = &[
    // documents
    "pdf", "docx", "xlsx", "pptx", "odt", "ods", "odp", "txt", "md", "markdown", "rst", "org", "tex", "bib",
    // human-written config
    "yaml", "yml", "toml", "sql", // source
    "rs", "py", "go", "java", "kt", "scala", "c", "h", "cc", "cpp", "hpp", "cs", "swift", "rb", "php", "lua", "js",
    "mjs", "cjs", "ts", "tsx", "jsx", "sh", "bash", "ps1", "psm1", "bat", "cmd", "vb", "fs", "dart", "r", "jl", "ex",
    "exs", "clj", "hs", "ml", "zig",
];

/// Extensions listed first in the initial crawl, so documents become
/// searchable before the (much larger) pile of source files is done.
pub const DOCUMENT_EXTENSIONS: &[&str] =
    &["pdf", "docx", "xlsx", "pptx", "odt", "ods", "odp", "txt", "md", "markdown", "rst", "org", "tex"];

/// Folder names that are never entered.
pub const DEFAULT_EXCLUDE_FOLDERS: &[&str] = &[
    "node_modules",
    ".git",
    ".hg",
    ".svn",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    ".gradle",
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".nuget",
    "site-packages",
    "dist-packages",
    "bower_components",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    ".nuxt",
    "$recycle.bin",
    "system volume information",
    "cache",
    "code cache",
    "gpucache",
    "inetcache",
    "temp",
    "indexeddb",
    "local storage",
    "session storage",
    "service worker",
    "blob_storage",
    "crashpad",
    // Big third-party installs and toolchains (source and docs that ship with
    // them were 70% of all text on the first full crawl of the dev PC).
    "matlab",
    "quartus",
    "vivado",
    "vitis",
    "vitis_hls",
    "modelsim_ase",
    "miniconda",
    "miniconda3",
    "anaconda3",
    "emsdk",
    "pinokio",
    "curseforge",
];

/// Folders (below the drive root, any drive) that are never entered: the
/// system, and the usual homes of installed software and games.
pub const DEFAULT_EXCLUDE_PATHS: &[&str] = &[
    r"\windows\",
    r"\program files\",
    r"\program files (x86)\",
    r"\programdata\",
    r"\windowsapps\",
    r"\msys64\",
    r"\steamlibrary\",
    r"\steam\steamapps\",
    r"\riot games\",
    r"\epic games\",
];

/// Path fragments that exclude a file wherever they occur.
pub const DEFAULT_EXCLUDE_FRAGMENTS: &[&str] = &[r"\appdata\local\", r"\go\pkg\mod\"];

/// Globs (matched against the path with `/` separators) skipped by default.
pub const DEFAULT_EXCLUDE_GLOBS: &[&str] = &["**/*.min.js", "**/*.min.css", "**/package-lock.json", "**/*.d.ts"];

#[derive(Debug, Clone, PartialEq)]
pub struct ContentConfig {
    pub enabled: bool,
    /// Skip system folders, caches, `node_modules`, `.git`, build output...
    /// Turn off to index exactly what the other settings allow.
    pub default_excludes: bool,
    /// Skip folders whose name starts with a dot (`.vscode`, `.codex`, ...).
    pub skip_dot_folders: bool,
    pub extensions: Vec<String>,
    /// Only index below these folders (empty: every drive).
    pub include_paths: Vec<String>,
    pub include_globs: Vec<String>,
    pub exclude_folders: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub max_file_mb: u64,
    pub file_timeout_secs: u64,
    /// Extractor processes running at once.
    pub workers: usize,
    pub pause_when_active: bool,
    /// Seconds without keyboard or mouse before the user counts as away.
    pub idle_secs: u64,
    pub pause_on_battery: bool,
    /// Target size of one passage, in bytes of text.
    pub chunk_bytes: usize,
    /// Content results asked for per query.
    pub max_results: usize,
}

impl Default for ContentConfig {
    fn default() -> Self {
        ContentConfig {
            enabled: true,
            default_excludes: true,
            skip_dot_folders: true,
            extensions: DEFAULT_EXTENSIONS.iter().map(|s| s.to_string()).collect(),
            include_paths: Vec::new(),
            include_globs: Vec::new(),
            exclude_folders: Vec::new(),
            exclude_paths: Vec::new(),
            exclude_globs: Vec::new(),
            max_file_mb: 20,
            file_timeout_secs: 60,
            workers: 2,
            pause_when_active: true,
            idle_secs: 20,
            pause_on_battery: true,
            chunk_bytes: 1200,
            max_results: 20,
        }
    }
}

#[derive(Deserialize, Default)]
struct File {
    content: Option<Section>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Section {
    enabled: Option<bool>,
    default_excludes: Option<bool>,
    skip_dot_folders: Option<bool>,
    extensions: Option<Vec<String>>,
    include_paths: Option<Vec<String>>,
    include_globs: Option<Vec<String>>,
    exclude_folders: Option<Vec<String>>,
    exclude_paths: Option<Vec<String>>,
    exclude_globs: Option<Vec<String>>,
    max_file_mb: Option<i64>,
    file_timeout_secs: Option<i64>,
    workers: Option<i64>,
    pause_when_active: Option<bool>,
    idle_secs: Option<i64>,
    pause_on_battery: Option<bool>,
    chunk_bytes: Option<i64>,
    max_results: Option<i64>,
}

pub const DEFAULT_SECTION: &str = r#"
# ---------------------------------------------------------------------------
# Content search: finds words inside files (PDF, Office, text, code). Run by
# the separate searchi.exe, which indexes in the background at low priority.
# Changes apply the next time searchi starts (right-click the tray icon > Quit
# is for the popup; stop searchi from Task Manager or run install.ps1 again).
[content]
enabled = true
# default_excludes = false   # also index system folders, caches, node_modules, .git, ...
# skip_dot_folders = false   # also index inside .vscode, .config, .codex, ... (skipped by default)

# Only these file types are indexed (without the dot).
# extensions = ["pdf", "docx", "txt", "md", "rs", "py"]

# Index only below these folders (default: every NTFS drive).
# include_paths = ["C:\\Users\\you\\Documents", "D:\\"]

# Extra folders to skip: by name (anywhere), or by path.
# exclude_folders = ["backups"]
# exclude_paths = ["C:\\Users\\you\\Downloads"]

# Globs against the full path (case-insensitive, use / or \\).
# exclude_globs = ["**/*.generated.*"]
# include_globs = ["C:/Users/you/**"]

max_file_mb = 20          # bigger files are skipped
file_timeout_secs = 60    # an extractor stuck this long is killed, the file skipped
workers = 2               # extractor processes at once (keep low)
pause_when_active = true  # crawl only while you are away from the keyboard
idle_secs = 20            # ... for this many seconds
pause_on_battery = true   # no crawling on battery
max_results = 20          # content matches shown below the name matches
"#;

impl ContentConfig {
    /// Parse the whole config file; only `[content]` matters here. The second
    /// value lists problems (the defaults are used for those settings).
    pub fn parse(text: &str) -> (ContentConfig, Vec<String>) {
        let mut c = ContentConfig::default();
        let mut problems = Vec::new();
        let file: File = match toml::from_str(text) {
            Ok(f) => f,
            Err(e) => {
                problems.push(format!("config.toml: {e}"));
                return (c, problems);
            }
        };
        let Some(s) = file.content else { return (c, problems) };
        let mut range = |name: &str, v: Option<i64>, lo: i64, hi: i64, set: &mut dyn FnMut(i64)| {
            if let Some(v) = v {
                if (lo..=hi).contains(&v) {
                    set(v);
                } else {
                    problems.push(format!("[content] {name} must be {lo}-{hi}"));
                }
            }
        };
        range("max_file_mb", s.max_file_mb, 1, 4096, &mut |v| c.max_file_mb = v as u64);
        range("file_timeout_secs", s.file_timeout_secs, 1, 3600, &mut |v| c.file_timeout_secs = v as u64);
        range("workers", s.workers, 1, 16, &mut |v| c.workers = v as usize);
        range("idle_secs", s.idle_secs, 0, 86_400, &mut |v| c.idle_secs = v as u64);
        range("chunk_bytes", s.chunk_bytes, 200, 20_000, &mut |v| c.chunk_bytes = v as usize);
        range("max_results", s.max_results, 1, 200, &mut |v| c.max_results = v as usize);
        if let Some(v) = s.enabled {
            c.enabled = v;
        }
        if let Some(v) = s.default_excludes {
            c.default_excludes = v;
        }
        if let Some(v) = s.skip_dot_folders {
            c.skip_dot_folders = v;
        }
        if let Some(v) = s.pause_when_active {
            c.pause_when_active = v;
        }
        if let Some(v) = s.pause_on_battery {
            c.pause_on_battery = v;
        }
        if let Some(v) = s.extensions {
            c.extensions = v.into_iter().map(|e| e.trim().trim_start_matches('.').to_lowercase()).collect();
        }
        if let Some(v) = s.include_paths {
            c.include_paths = v;
        }
        if let Some(v) = s.include_globs {
            c.include_globs = v;
        }
        if let Some(v) = s.exclude_folders {
            c.exclude_folders = v;
        }
        if let Some(v) = s.exclude_paths {
            c.exclude_paths = v;
        }
        if let Some(v) = s.exclude_globs {
            c.exclude_globs = v;
        }
        (c, problems)
    }

    pub fn load(path: &Path) -> (ContentConfig, Vec<String>) {
        match std::fs::read_to_string(path) {
            Ok(t) => Self::parse(&t),
            Err(_) => (ContentConfig::default(), Vec::new()),
        }
    }

    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_mb * 1024 * 1024
    }

    /// The wire-format rules (everything but globs), lower-cased.
    pub fn filter(&self) -> Filter {
        let lower = |v: &[String]| v.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
        let defaults = self.default_excludes;
        let mut exclude_components: Vec<String> = if defaults {
            DEFAULT_EXCLUDE_FOLDERS.iter().map(|s| s.to_string()).collect()
        } else {
            Vec::new()
        };
        exclude_components.extend(lower(&self.exclude_folders));
        let mut exclude_prefixes: Vec<String> = if defaults {
            DEFAULT_EXCLUDE_PATHS.iter().map(|s| s.to_string()).collect()
        } else {
            Vec::new()
        };
        let mut include_prefixes = Vec::new();
        // A user path with a drive excludes just that folder; without one, on any drive.
        let mut extra_fragments = Vec::new();
        for p in lower(&self.exclude_paths) {
            let p = with_trailing_slash(&p.replace('/', "\\"));
            if p.as_bytes().get(1) == Some(&b':') {
                extra_fragments.push(p);
            } else {
                exclude_prefixes.push(if p.starts_with('\\') { p } else { format!("\\{p}") });
            }
        }
        for p in lower(&self.include_paths) {
            include_prefixes.push(with_trailing_slash(&p.replace('/', "\\")));
        }
        let mut exclude_fragments: Vec<String> = if defaults {
            DEFAULT_EXCLUDE_FRAGMENTS.iter().map(|s| s.to_string()).collect()
        } else {
            Vec::new()
        };
        // A fragment that starts with a drive letter matches anywhere in the path;
        // as a *prefix* it would be exact, which is what the user meant.
        exclude_fragments.extend(extra_fragments);
        Filter {
            exts: self.extensions.clone(),
            exclude_components,
            exclude_prefixes,
            exclude_fragments,
            include_prefixes,
            skip_dot_dirs: self.skip_dot_folders,
        }
    }

    /// The same filter restricted to the document formats (first crawl pass).
    pub fn document_filter(&self) -> Filter {
        let mut f = self.filter();
        f.exts.retain(|e| DOCUMENT_EXTENSIONS.contains(&e.as_str()));
        f
    }
}

fn with_trailing_slash(p: &str) -> String {
    if p.ends_with('\\') { p.to_string() } else { format!("{p}\\") }
}

/// Everything that decides whether a path is indexed: the wire filter plus
/// the user's globs. Used by the indexer for listings, events and walks.
pub struct PathRules {
    pub filter: Filter,
    include: Option<GlobSet>,
    exclude: GlobSet,
}

impl PathRules {
    pub fn new(cfg: &ContentConfig) -> (PathRules, Vec<String>) {
        let mut problems = Vec::new();
        let mut build = |pats: &[String], extra: &[&str]| {
            let mut b = GlobSetBuilder::new();
            let mut any = false;
            for p in pats.iter().map(String::as_str).chain(extra.iter().copied()) {
                let p = p.replace('\\', "/");
                match Glob::new(&p) {
                    Ok(g) => {
                        // case-insensitive: Windows paths
                        let g = globset::GlobBuilder::new(g.glob()).case_insensitive(true).literal_separator(false).build();
                        match g {
                            Ok(g) => {
                                b.add(g);
                                any = true;
                            }
                            Err(e) => problems.push(format!("glob {p:?}: {e}")),
                        }
                    }
                    Err(e) => problems.push(format!("glob {p:?}: {e}")),
                }
            }
            (any, b.build().unwrap_or_else(|_| GlobSet::empty()))
        };
        let (_, exclude) = build(&cfg.exclude_globs, if cfg.default_excludes { DEFAULT_EXCLUDE_GLOBS } else { &[] });
        let (has_include, include) = build(&cfg.include_globs, &[]);
        (
            PathRules { filter: cfg.filter(), include: has_include.then_some(include), exclude },
            problems,
        )
    }

    fn globs_ok(&self, path: &str) -> bool {
        let p = path.replace('\\', "/");
        if self.exclude.is_match(&p) {
            return false;
        }
        self.include.as_ref().is_none_or(|i| i.is_match(&p))
    }

    /// Only `X:\...` paths are ever read. A path the helper hands over that
    /// names a network share (`\\server\share`) or a device would make this
    /// process contact that host or open that device.
    pub fn drive_rooted(path: &str) -> bool {
        let b = path.as_bytes();
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
    }

    pub fn allows_file(&self, path: &str) -> bool {
        Self::drive_rooted(path) && self.filter.allows_file(path) && self.globs_ok(path)
    }

    /// Whether to descend into a folder (globs only exclude here, since an
    /// include glob describes files).
    pub fn allows_dir(&self, path: &str) -> bool {
        Self::drive_rooted(path) && self.filter.allows_dir(path)
    }
}

/// `%LOCALAPPDATA%\Search`.
pub fn app_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("Search"))
        .unwrap_or_else(|| std::env::temp_dir().join("Search"))
}

/// Where the content index, its status file and the indexer's log live. Owned
/// by the unelevated indexer; the elevated helper never writes here.
pub fn content_dir() -> PathBuf {
    app_dir().join("content")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_default_section_give_defaults() {
        let (c, p) = ContentConfig::parse("");
        assert_eq!(c, ContentConfig::default());
        assert!(p.is_empty());
        let (c, p) = ContentConfig::parse(&format!("hotkey = \"Alt+Space\"\n{DEFAULT_SECTION}"));
        assert!(p.is_empty(), "{p:?}");
        assert_eq!(c, ContentConfig::default());
    }

    #[test]
    fn values_override_and_bad_ones_are_reported() {
        let (c, p) = ContentConfig::parse(
            "[content]\nmax_file_mb = 5\nworkers = 99\nextensions = [\".PDF\", \"Txt\"]\nexclude_folders = [\"Backups\"]\n",
        );
        assert_eq!(c.max_file_mb, 5);
        assert_eq!(c.workers, 2); // refused, default kept
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(c.extensions, ["pdf", "txt"]);
        let f = c.filter();
        assert!(f.exclude_components.contains(&"backups".to_string()));
        assert!(f.exclude_components.contains(&"node_modules".to_string()));
    }

    #[test]
    fn syntax_errors_fall_back_to_defaults() {
        let (c, p) = ContentConfig::parse("[content\nmax_file_mb = ");
        assert_eq!(c, ContentConfig::default());
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn rules_combine_filter_and_globs() {
        let (c, _) = ContentConfig::parse(
            "[content]\nexclude_globs = [\"**/drafts/**\"]\nexclude_paths = [\"C:\\\\Users\\\\Zach\\\\Downloads\"]\n",
        );
        let (r, problems) = PathRules::new(&c);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(r.allows_file(r"C:\Users\Zach\Documents\a.pdf"));
        // Never a network share, a device path or a relative one.
        assert!(!r.allows_file(r"\\server\share\a.pdf"));
        assert!(!r.allows_file(r"\\?\C:\Users\Zach\a.pdf"));
        assert!(!r.allows_file(r"Documents\a.pdf"));
        assert!(!r.allows_dir(r"\\server\share"));
        assert!(!r.allows_file(r"C:\Users\Zach\Documents\drafts\a.pdf"));
        assert!(!r.allows_file(r"C:\Users\Zach\Downloads\a.pdf"));
        assert!(r.allows_file(r"D:\Users\Zach\Downloads\a.pdf")); // only that drive's folder
        assert!(!r.allows_file(r"C:\proj\app.min.js"));
        assert!(!r.allows_file(r"C:\Windows\a.txt"));
        assert!(!r.allows_file(r"C:\Users\Zach\a.exe"));
    }

    #[test]
    fn include_paths_and_globs_restrict() {
        let (c, _) = ContentConfig::parse("[content]\ninclude_paths = [\"C:\\\\Users\\\\Zach\"]\n");
        let (r, _) = PathRules::new(&c);
        assert!(r.allows_file(r"C:\Users\Zach\a.txt"));
        assert!(!r.allows_file(r"C:\Users\Zachary\a.txt"));
        assert!(!r.allows_file(r"D:\a.txt"));
        let (c, _) = ContentConfig::parse("[content]\ninclude_globs = [\"C:/work/**\"]\n");
        let (r, _) = PathRules::new(&c);
        assert!(r.allows_file(r"C:\work\a\b.txt"));
        assert!(!r.allows_file(r"C:\other\b.txt"));
    }

    #[test]
    fn document_pass_is_a_subset() {
        let c = ContentConfig::default();
        let d = c.document_filter();
        assert!(d.exts.contains(&"pdf".to_string()));
        assert!(!d.exts.contains(&"rs".to_string()));
        assert!(d.exts.len() < c.extensions.len());
    }
}

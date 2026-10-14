//! Content search: find words inside files.
//!
//! * `config`   the `[content]` settings and the path rules
//! * `extract`  text out of files (runs only in extractor child processes)
//! * `chunk`    passages with byte offsets
//! * `store`    the tantivy index and its manifest (written by the indexer)
//! * `search`   queries and snippets (read by the popup and the CLI)

pub mod chunk;
pub mod config;
pub mod dir;
pub mod extract;
#[cfg(windows)]
pub mod indexer;
pub mod search;
pub mod status;
pub mod store;
pub mod sys;
#[cfg(windows)]
pub mod worker;

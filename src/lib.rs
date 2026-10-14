//! Search core: an in-RAM filename index built from the NTFS master file
//! table (or, as a fallback, a directory walk) and a fast substring scan.

pub mod fmt;
pub mod index;
pub mod ipc;
pub mod live;
#[cfg(windows)]
pub mod mft;
#[cfg(windows)]
pub mod pipe;
pub mod popup;
pub mod selftest;
pub mod snapshot;
pub mod sys;
pub mod usn;
pub mod walk;

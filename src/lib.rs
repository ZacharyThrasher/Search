//! Search core: an in-RAM filename index built from the NTFS master file
//! table (or, as a fallback, a directory walk) and a fast substring scan.

pub mod index;
#[cfg(windows)]
pub mod mft;
pub mod sys;
pub mod usn;
pub mod walk;

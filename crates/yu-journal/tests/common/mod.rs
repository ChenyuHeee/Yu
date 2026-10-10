//! Helpers shared by the journal's integration tests.
//!
//! The constraint tests all ask a version of the same question — "how many times
//! does this string appear on disk?" — so the counting lives here, once, and it
//! always scans the *whole* journal directory (log plus blobs). A payload that
//! leaked anywhere would show up.

#![allow(dead_code)] // each test binary uses a subset of these

use std::fs;
use std::path::{Path, PathBuf};

use yu_journal::{Journal, SessionId};
use yu_observe::AgentId;

pub fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

pub fn agent() -> AgentId {
    AgentId::new("exec")
}

pub async fn journal_in(dir: &Path, session: &str) -> Journal {
    Journal::open(dir, SessionId::new(session)).await.unwrap()
}

/// Every file under `root`, recursively. `root` itself may not exist.
pub fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The contents of every file under `root`, concatenated.
pub fn text_under(root: &Path) -> String {
    let mut out = String::new();
    for path in files_under(root) {
        out.push_str(&fs::read_to_string(&path).unwrap_or_default());
    }
    out
}

/// How many times `needle` occurs in file contents under `root`.
///
/// Paths are not searched: a digest used as a file name is a reference to the
/// payload, not the payload.
pub fn occurrences_under(root: &Path, needle: &str) -> usize {
    text_under(root).matches(needle).count()
}

/// The files in the content-addressed store (everything under `blobs/`), ignoring
/// temp files — a leftover temp file would mean a write that never finished.
pub fn blob_files(dir: &Path) -> Vec<PathBuf> {
    files_under(&dir.join("blobs"))
        .into_iter()
        .filter(|p| !p.to_string_lossy().ends_with(".tmp"))
        .collect()
}

/// The journal's log file as text, for tests that count lines or look for a
/// value that must *not* be in the envelope.
pub fn log_text(journal: &Journal) -> String {
    fs::read_to_string(journal.path()).unwrap()
}

/// Open a journal forked from `parent` at `at_seq`.
pub async fn journal_fork(
    dir: &std::path::Path,
    session: &str,
    parent: std::sync::Arc<Journal>,
    at_seq: u64,
) -> Journal {
    Journal::open_fork(dir, SessionId::new(session), parent, at_seq)
        .await
        .expect("open a fork")
}

//! Content-addressed payload store.
//!
//! Three of DESIGN's four L2 constraints end here, and all three are the same
//! move — keep the bytes in one place and reference them:
//!
//! * (1) a payload stored as a blob exists exactly once on disk, no matter how
//!   many events, forks, or replays point at it;
//! * (3) re-sent material (a system prompt, a `task_reminder`) is written once
//!   and referenced by digest thereafter, so re-injection costs a 64-byte
//!   reference per turn instead of a re-copy of the whole body;
//! * (4) thinking text goes in a blob while its signature stays a scalar field
//!   on the event, so the two can never be concatenated by accident.
//!
//! Layout is `root/ab/cdef…`: the first two hex characters of the sha256 digest
//! name a directory, the remaining 62 name the file. Sharding keeps one
//! directory from holding every blob a long run writes, and the digest *is* the
//! name — which is what makes [`BlobStore::put`] idempotent by construction
//! rather than by bookkeeping.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::JournalError;

/// Length of a sha256 digest in hex characters. Every blob reference in the
/// journal is expected to have exactly this length.
pub const DIGEST_LEN: usize = 64;

/// Hex characters spent on the directory level of the shard path.
const SHARD_LEN: usize = 2;

/// The sha256 of `bytes`, lowercase hex.
///
/// Public because callers have to answer "have I stored this already?" *before*
/// storing it — constraints 1 and 3 are both decisions made on this value.
pub fn digest_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Distinguishes temp files written by concurrent callers in one process; the
/// pid distinguishes processes.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A content-addressed store rooted at one directory.
///
/// Cloning is cheap and the clone refers to the same store; the store holds no
/// open handles, so every method is usable from any thread.
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Open (creating if needed) the store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, JournalError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The directory this store writes into.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Store `bytes`, returning their digest.
    ///
    /// Idempotent: identical bytes return the identical digest and do not write
    /// a second file. A re-sent system prompt therefore costs nothing to record
    /// again (constraint 3).
    pub fn put(&self, bytes: &[u8]) -> Result<String, JournalError> {
        let digest = digest_of(bytes);
        let dir = self.root.join(&digest[..SHARD_LEN]);
        let path = dir.join(&digest[SHARD_LEN..]);

        // The name is the content, so a file that is already there already holds
        // these bytes. Writing again could only ever rewrite them.
        if path.is_file() {
            return Ok(digest);
        }

        fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!(
            ".{}.{}.{}.tmp",
            &digest[SHARD_LEN..],
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));

        // Write to a temp file, then rename into place. A reader must never see
        // a half-written blob, and a crash must never leave behind a file whose
        // name claims a digest its contents do not have.
        let attempt = (|| -> Result<(), JournalError> {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &path)?;
            Ok(())
        })();

        if let Err(err) = attempt {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        tracing::trace!(digest = %digest, bytes = bytes.len(), "blob stored");
        Ok(digest)
    }

    /// Read a blob back. A digest with no file behind it is [`BlobMissing`],
    /// never an empty payload — a reference that points at nothing is exactly
    /// the silent loss DESIGN forbids.
    ///
    /// [`BlobMissing`]: JournalError::BlobMissing
    pub fn get(&self, digest: &str) -> Result<Vec<u8>, JournalError> {
        let path = self.resolve(digest)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(JournalError::BlobMissing {
                    digest: digest.to_owned(),
                })
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Whether a blob is present.
    pub fn has(&self, digest: &str) -> bool {
        self.resolve(digest).map(|p| p.is_file()).unwrap_or(false)
    }

    /// Size in bytes, or `None` when the blob is absent.
    pub fn size(&self, digest: &str) -> Option<u64> {
        let path = self.resolve(digest).ok()?;
        match fs::metadata(path) {
            Ok(meta) if meta.is_file() => Some(meta.len()),
            _ => None,
        }
    }

    /// Store a string payload.
    pub fn put_str(&self, s: &str) -> Result<String, JournalError> {
        self.put(s.as_bytes())
    }

    /// Read a blob back as a string. The digest travels with the error, since
    /// the caller's next question is always "which reference was that".
    pub fn get_str(&self, digest: &str) -> Result<String, JournalError> {
        let bytes = self.get(digest)?;
        String::from_utf8(bytes).map_err(|_| JournalError::NotUtf8 {
            digest: digest.to_owned(),
        })
    }

    /// Whether the bytes on disk still hash to the name they are filed under.
    ///
    /// The store can only promise name-equals-content at the moment of writing;
    /// L3's claim that any historical point is reconstructible rests on that
    /// promise, so it has to be checkable later.
    pub fn verify(&self, digest: &str) -> Result<bool, JournalError> {
        let bytes = self.get(digest)?;
        Ok(digest_of(&bytes).eq_ignore_ascii_case(digest))
    }

    /// Map a reference to a path, rejecting anything that is not a digest.
    ///
    /// The validation is what keeps a reference from addressing a path outside
    /// the store: only hex characters survive it, so `../` cannot be spelled.
    fn resolve(&self, digest: &str) -> Result<PathBuf, JournalError> {
        if !is_digest(digest) {
            return Err(JournalError::InvalidDigest {
                digest: digest.to_owned(),
            });
        }
        let lower = digest.to_ascii_lowercase();
        Ok(self.root.join(&lower[..SHARD_LEN]).join(&lower[SHARD_LEN..]))
    }
}

/// Whether `s` is a sha256 digest written in hex.
pub fn is_digest(s: &str) -> bool {
    s.len() == DIGEST_LEN && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path().join("blobs")).unwrap();
        (dir, store)
    }

    #[test]
    fn digest_is_sha256_hex() {
        // The empty string's sha256, so the function is pinned to a value that
        // does not depend on our own round-trip.
        assert_eq!(
            digest_of(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn put_is_idempotent() {
        let (dir, store) = store();
        let first = store.put(b"same bytes").unwrap();
        let second = store.put(b"same bytes").unwrap();
        assert_eq!(first, second);
        assert_eq!(store.get(&first).unwrap(), b"same bytes");
        // One blob, not two: that is what "stored once" means on disk.
        assert_eq!(files_under(dir.path()).len(), 1);
    }

    #[test]
    fn blob_is_filed_under_its_first_two_hex_characters() {
        let (dir, store) = store();
        let digest = store.put(b"sharded").unwrap();
        let expected = dir
            .path()
            .join("blobs")
            .join(&digest[..2])
            .join(&digest[2..]);
        assert!(expected.is_file(), "expected {}", expected.display());
    }

    #[test]
    fn missing_blob_names_the_digest() {
        let (_dir, store) = store();
        let digest = digest_of(b"never stored");
        match store.get(&digest) {
            Err(JournalError::BlobMissing { digest: d }) => assert_eq!(d, digest),
            other => panic!("expected BlobMissing, got {other:?}"),
        }
        assert!(!store.has(&digest));
        assert_eq!(store.size(&digest), None);
    }

    #[test]
    fn a_reference_cannot_escape_the_store() {
        let (_dir, store) = store();
        for bad in ["../../../etc/passwd", "..", "abc", "", &"z".repeat(64)] {
            match store.get(bad) {
                Err(JournalError::InvalidDigest { digest }) => assert_eq!(digest, bad),
                other => panic!("`{bad}` should not resolve, got {other:?}"),
            }
            assert!(!store.has(bad));
        }
    }

    #[test]
    fn strings_round_trip() {
        let (_dir, store) = store();
        let digest = store.put_str("hello ☕").unwrap();
        assert_eq!(store.get_str(&digest).unwrap(), "hello ☕");
    }

    #[test]
    fn non_utf8_payload_is_reported_not_replaced() {
        let (_dir, store) = store();
        let digest = store.put(&[0xff, 0xfe, 0x00]).unwrap();
        assert!(matches!(
            store.get_str(&digest),
            Err(JournalError::NotUtf8 { .. })
        ));
        assert_eq!(store.get(&digest).unwrap(), vec![0xff, 0xfe, 0x00]);
    }

    #[test]
    fn verify_detects_content_that_no_longer_matches_its_name() {
        let (dir, store) = store();
        let digest = store.put(b"original").unwrap();
        assert!(store.verify(&digest).unwrap());
        let path = dir
            .path()
            .join("blobs")
            .join(&digest[..2])
            .join(&digest[2..]);
        fs::write(&path, b"tampered").unwrap();
        assert!(!store.verify(&digest).unwrap());
    }

    #[test]
    fn failed_writes_leave_no_temp_files() {
        let (dir, store) = store();
        store.put(b"clean").unwrap();
        for entry in files_under(dir.path()) {
            assert!(
                !entry.to_string_lossy().ends_with(".tmp"),
                "left behind {}",
                entry.display()
            );
        }
    }

    fn files_under(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }
}

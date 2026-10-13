//! A tool-shape profile: somebody else's tool surface, snapshotted.
//!
//! DESIGN's 「工具面:与 Claude Code 对齐」 makes two claims this module exists to
//! keep separable:
//!
//! * **The shape is data.** A profile holds the `name`, `description` and
//!   `input_schema` of each tool exactly as the aligned client sent them, so
//!   adopting a new one is a file, not a rebuild. 约束 3: "它必须是 profile,不是
//!   焊死的选择 … profile 是一份可刷新的快照."
//! * **What comes back is not part of it.** A profile is a *snapshot of a wire
//!   request*, which is why nothing here interprets the entries: an entry is
//!   copied, and the decisions about which of them this harness can honour live
//!   beside the implementations that have to honour them (see
//!   [`crate::native`]'s face table). A loader that pruned, reordered or
//!   "tidied" the snapshot would destroy the only property that makes it worth
//!   having — that it is what was sent rather than what we remember.
//!
//! The first capture lives in `captures/claude-code/`: Claude Code 2.1.248,
//! recorded before forwarding, with the body hashes on both sides of the
//! forward. `captures/verify.py` is its criterion; this module's is that the
//! bytes reaching the wire are the ones in the file.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// One tool's face, as the profile records it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProfileEntry {
    pub name: String,
    /// Absent rather than empty when the snapshot carried no description: the
    /// two are different faces, and only one of them was measured.
    #[serde(default)]
    pub description: Option<String>,
    /// Kept as raw JSON. A typed schema would drop the keys it does not know
    /// (`$schema`), reorder the ones it keeps, and quietly become a *different*
    /// schema — the capture's README says exactly this is the thing to expect.
    pub input_schema: Value,
}

/// The file as it is on disk: `{"tools": [...], "system": [...]}`.
///
/// `system` is deliberately not read. It is in the same package as the tools
/// because that is how the capture was taken, but the decision layer's prompt is
/// original (DESIGN:「prompt 必须原创」) and an executor prompt borrowed wholesale
/// is a different decision from a tool face — it would arrive here as a field
/// nobody asked for.
#[derive(Deserialize)]
struct Snapshot {
    tools: Vec<ProfileEntry>,
}

/// A snapshot of a tool surface, and the name it is known by.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolProfile {
    name: String,
    entries: Vec<ProfileEntry>,
}

/// Why a profile could not be turned into a registry.
///
/// The load errors and the binding errors are one type on purpose: both are
/// *data* errors — a refreshed snapshot can rename a parameter as easily as it
/// can lose a file, and a caller that handled the second but not the first would
/// be the caller this enum exists to make impossible.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a tool-shape profile: {problem}")]
    Shape { path: PathBuf, problem: String },
    #[error(
        "the profile `{profile}` lists no tools, so it would describe a surface with nothing on \
         it; a profile that failed to load must not quietly become the unmeasured fallback"
    )]
    Empty { profile: String },
    #[error(
        "the profile `{profile}` has no tool named `{wire}`, and `{implementation}` is built to \
         wear that face; a snapshot that does not carry it cannot be honoured by pretending"
    )]
    NoSuchTool {
        profile: String,
        wire: &'static str,
        implementation: String,
    },
    #[error(
        "the profile `{profile}`'s `{wire}` has no `{parameter}` parameter, so `{implementation}` \
         cannot read what that face says the model will send"
    )]
    NoSuchParameter {
        profile: String,
        wire: &'static str,
        parameter: &'static str,
        implementation: String,
    },
    #[error(
        "the profile `{profile}`'s `{wire}` description does not contain `{text}`, which this \
         harness refuses to adopt ({why}); a re-worded snapshot must be read again rather than \
         assumed"
    )]
    Unrefused {
        profile: String,
        wire: &'static str,
        text: String,
        why: String,
    },
}

impl ToolProfile {
    /// A profile from the bytes of a snapshot, named by the caller.
    pub fn from_json(name: impl Into<String>, text: &str) -> Result<Self, ProfileError> {
        let name = name.into();
        let path = PathBuf::from(&name);
        let snapshot: Snapshot = serde_json::from_str(text).map_err(|problem| {
            ProfileError::Shape {
                path: path.clone(),
                problem: problem.to_string(),
            }
        })?;
        Self::from_snapshot(name, snapshot)
    }

    /// A profile from a file, named by the file's stem.
    ///
    /// The stem is the identity: `claude-code-2.1.248.json` is the surface of
    /// one measured client version, and that is the whole of what the name
    /// claims.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ProfileError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ProfileError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self::from_json(name, &text)
    }

    /// A profile assembled in memory. For tests, and for a source that is not a
    /// file — the type does not care where the bytes came from, only that they
    /// are a snapshot rather than something we made up.
    pub fn new(name: impl Into<String>, entries: Vec<ProfileEntry>) -> Result<Self, ProfileError> {
        Self::from_snapshot(
            name.into(),
            Snapshot {
                tools: entries,
            },
        )
    }

    fn from_snapshot(name: String, snapshot: Snapshot) -> Result<Self, ProfileError> {
        if snapshot.tools.is_empty() {
            return Err(ProfileError::Empty { profile: name });
        }
        Ok(Self {
            name,
            entries: snapshot.tools,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Every entry, in the order the snapshot listed them. The order is kept
    /// rather than sorted: it is the order the tools render in on the wire, and
    /// DESIGN's cache rule is that the request's byte prefix is stable.
    pub fn entries(&self) -> &[ProfileEntry] {
        &self.entries
    }

    pub fn entry(&self, name: &str) -> Option<&ProfileEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|entry| entry.name.as_str()).collect()
    }
}

impl ProfileEntry {
    pub fn new(name: impl Into<String>, description: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: Some(description.into()),
            input_schema,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SNAPSHOT: &str = r#"{
      "tools": [
        {"name": "Read", "description": "Reads a file.", "input_schema": {"$schema": "x", "type": "object"}},
        {"name": "Edit", "input_schema": {"type": "object"}}
      ],
      "system": [{"type": "text", "text": "not ours to read"}]
    }"#;

    #[test]
    fn a_snapshot_loads_with_its_order_and_its_schemas_intact() {
        let profile = ToolProfile::from_json("test", SNAPSHOT).unwrap();
        assert_eq!(profile.names(), vec!["Read", "Edit"]);
        assert_eq!(profile.entry("Read").unwrap().description.as_deref(), Some("Reads a file."));
        // `$schema` is part of the captured schema and must survive the round
        // trip; a loader that dropped it would send a different tool.
        assert_eq!(
            profile.entry("Read").unwrap().input_schema.get("$schema"),
            Some(&json!("x"))
        );
        // An entry with no description is `None`, not an empty string: only one
        // of those two was measured.
        assert_eq!(profile.entry("Edit").unwrap().description, None);
    }

    #[test]
    fn the_system_field_is_not_read_into_the_profile() {
        // The profile type has no `system` at all — this pins that the field is
        // ignored rather than quietly becoming the executor's prompt.
        let profile = ToolProfile::from_json("test", SNAPSHOT).unwrap();
        assert_eq!(profile.entries().len(), 2);
    }

    #[test]
    fn a_profile_with_no_tools_is_an_error_not_a_default() {
        let problem = ToolProfile::from_json("empty", r#"{"tools": []}"#).unwrap_err();
        assert!(problem.to_string().contains("lists no tools"), "{problem}");
    }

    #[test]
    fn a_file_that_is_not_a_profile_says_so() {
        let problem = ToolProfile::from_json("junk", "{\"not_tools\": 1}").unwrap_err();
        assert!(problem.to_string().contains("is not a tool-shape profile"), "{problem}");
    }

    #[test]
    fn loading_a_missing_file_names_the_path() {
        let problem = ToolProfile::from_path("/nonexistent/profile.json").unwrap_err();
        assert!(problem.to_string().contains("/nonexistent/profile.json"), "{problem}");
    }

    /// The committed capture loads, under the name of the client version it was
    /// taken from. If this fails the profile directory moved, and everything
    /// downstream of it is testing something else.
    #[test]
    fn the_committed_capture_loads_under_its_version_name() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../captures/claude-code/profile/claude-code-2.1.248.json"
        );
        let profile = ToolProfile::from_path(path).expect("the committed capture loads");
        assert_eq!(profile.name(), "claude-code-2.1.248");
        assert_eq!(profile.entries().len(), 26);
        assert!(profile.entry("Read").is_some());
        assert!(profile.entry("Bash").is_some());
        assert!(profile.entry("Edit").is_some());
        assert!(profile.entry("Write").is_some());
        // And the tool DESIGN keeps asserting does not exist in this surface,
        // still does not: a profile that carried it would be a different client.
        assert!(profile.entry("Grep").is_none());
    }
}

//! The typed tool result.
//!
//! DESIGN: **返回值不是一个字符串,是一个带生命周期的类型化对象。** Every other
//! harness returns a string and, when it is too large, silently picks one of the
//! downgrades it happens to implement. Here the value is a structure that says
//! what it is (`kind`), how big it was (`size`), whether it can be re-obtained
//! (`reproducible`), whether it can be written to disk and addressed
//! (`addressable`), and where it came from (`provenance` — the key that makes a
//! re-run possible).
//!
//! Two consequences are visible in this module's types rather than in prose:
//!
//! * **A manifest is a pointer, not a claim of completeness.** [`Manifest`]
//!   carries the omitted byte count and, when one exists, the digest of the blob
//!   that holds the rest. [`ToolResult::render`] turns any of those into an
//!   explicit statement of what is present.
//! * **Read-back is closed against re-spilling.** [`Provenance::from_spill`]
//!   marks a payload that was produced by reading a previously spilled blob, so
//!   the reducer can refuse to spill it a second time (see `reduce`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use yu_wire::types::{ContentBlock, ToolResultBlock};

/// What shape the result is.
///
/// `kind` is not decoration: it selects the reduction path and, for unbounded
/// shapes, the specialised reader that gets dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    File,
    Search,
    Log,
    Diff,
    Structured,
}

impl ResultKind {
    /// Every kind, in declaration order. Policy tables and prompt-sizing tests
    /// iterate this rather than listing the variants again.
    pub const ALL: [ResultKind; 5] = [
        ResultKind::File,
        ResultKind::Search,
        ResultKind::Log,
        ResultKind::Diff,
        ResultKind::Structured,
    ];

    /// Whether this shape is unbounded in principle (a search can return any
    /// number of hits; a file has a definite size).
    ///
    /// Unbounded shapes are not *reduced* by looking at their size — their size
    /// is a symptom. They are handled by a reader specialised for the shape.
    pub fn is_unbounded(self) -> bool {
        matches!(self, ResultKind::Search | ResultKind::Log)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ResultKind::File => "file",
            ResultKind::Search => "search",
            ResultKind::Log => "log",
            ResultKind::Diff => "diff",
            ResultKind::Structured => "structured",
        }
    }
}

/// Which tool, with what arguments — the key to re-running.
///
/// DESIGN: "provenance — 哪个工具、什么参数 —— 重跑的钥匙". Without this a
/// manifest can say what is missing but not how to get it back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub tool: String,
    pub input: Value,
    /// Set when this payload was produced by reading a previously spilled blob.
    /// DESIGN's claim 3 depends on this: read-back must be closed against
    /// re-spilling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_spill: Option<String>,
}

impl Provenance {
    pub fn new(tool: impl Into<String>, input: Value) -> Self {
        Self {
            tool: tool.into(),
            input,
            from_spill: None,
        }
    }
}

/// What is *not* in the payload.
///
/// DESIGN: "摘要谎称自己是内容。清单不谎称。" The note is prose and may be read by
/// a model, so it must not overstate what is present; the byte count and the
/// blob digest are the machine-checkable parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// How many bytes are NOT in the payload.
    pub omitted_bytes: u64,
    /// The command that would re-obtain the full result. DESIGN: "重跑的命令是什么".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerun: Option<String>,
    /// Human-readable, must not overstate what is present.
    pub note: String,
    /// If a spilled blob exists, its digest — the manifest is a pointer, not a
    /// claim of completeness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// What the model actually receives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Payload {
    /// Everything is present.
    Inline(String),
    /// Large, reproducible: intentionally dropped. `manifest.rerun` says how to
    /// get it back.
    Dropped(Manifest),
    /// Large, not reproducible, addressable: on disk, with an
    /// instruction-carrying pointer.
    Spilled {
        digest: String,
        pointer: String,
        manifest: Manifest,
    },
    /// A shape-specialised reader already summarised it; the manifest records
    /// what was read.
    Reduced { text: String, manifest: Manifest },
}

impl Payload {
    /// The bytes this payload carries in context, if any.
    ///
    /// `Dropped` carries none (that is the point) and `Spilled` carries a
    /// pointer rather than content.
    pub fn text(&self) -> Option<&str> {
        match self {
            Payload::Inline(text) => Some(text),
            Payload::Reduced { text, .. } => Some(text),
            Payload::Dropped(_) | Payload::Spilled { .. } => None,
        }
    }

    pub fn manifest(&self) -> Option<&Manifest> {
        match self {
            Payload::Inline(_) => None,
            Payload::Dropped(m) => Some(m),
            Payload::Spilled { manifest, .. } => Some(manifest),
            Payload::Reduced { manifest, .. } => Some(manifest),
        }
    }

    /// Whether the payload's bytes are on disk and addressable.
    pub fn is_on_disk(&self) -> bool {
        matches!(self, Payload::Spilled { .. })
    }
}

/// A tool call's result, as a value with a life cycle rather than a string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub payload: Payload,
    pub kind: ResultKind,
    /// The size of the payload the tool produced — not of whatever text the
    /// reduction chose to keep. This is the number a manifest's
    /// `omitted_bytes` is measured against, and it survives reduction
    /// unchanged so that the account stays honest after the fact.
    pub size: u64,
    /// Can re-running the same call produce the same result?
    pub reproducible: bool,
    /// Can the full result be written to disk and addressed?
    pub addressable: bool,
    pub provenance: Provenance,
}

impl ToolResult {
    /// Small result, all present.
    ///
    /// `reproducible` and `addressable` default to `false`: neither is claimed
    /// until a tool says so. A tool that can be re-run should set them.
    pub fn inline(
        tool: impl Into<String>,
        input: Value,
        kind: ResultKind,
        text: impl Into<String>,
    ) -> Self {
        let text = text.into();
        Self {
            size: text.len() as u64,
            payload: Payload::Inline(text),
            kind,
            reproducible: false,
            addressable: false,
            provenance: Provenance::new(tool, input),
        }
    }

    /// What the model actually sees. Every variant must render as an honest
    /// statement of what is and is not present.
    ///
    /// Most of what is here goes in front of a model, and a message in a shape
    /// no model was trained on spends the post-training it has for harness
    /// output. Two of the three shapes below are therefore Claude Code's, and
    /// each arm says which claim it is making, because "copied from Claude
    /// Code" and "ours" are different claims and only the first can be checked
    /// against a source. The Claude Code wording cited here is from the 2.1.248
    /// CLI bundle, the version installed on this machine.
    ///
    /// * `Dropped` — **Claude Code's truncation marker**, `... [N <unit>
    ///   truncated] ...` (`h8t`/`Gut` in that bundle emit `... [N characters
    ///   truncated] ...` and `... [N lines truncated] ...`). The scaffold is
    ///   Claude Code's; **the unit is ours** and it is bytes, because the
    ///   manifest counts bytes and a dropped payload keeps no text to count
    ///   lines or characters in.
    /// * `Spilled` — **Claude Code's `<persisted-output>` block**: `Output too
    ///   large (<size>). Full output saved to: <path>` between
    ///   `<persisted-output>` and `</persisted-output>` (`Lae`, with its own
    ///   size formatter `Ht`; the same block appears in this machine's own
    ///   transcripts as `Output too large (38.5KB). Full output saved to: …`).
    ///   Two differences, both **ours**: the `Preview (first 2KB):` section is
    ///   absent because a `Payload::Spilled` holds no bytes to preview and may
    ///   not claim one, and the instruction after the block is ours because in
    ///   Claude Code that path is readable with Read and Grep — the block
    ///   stands alone there — while this address is not readable by the
    ///   caller's tools, so the delegation is the way back in (DESIGN:
    ///   "隔离必须带指令").
    /// * `Reduced` — **ours**. Claude Code has no form for a tool result
    ///   replaced by a summary of it, because it never replaces one; there was
    ///   nothing to copy. The note is bracketed because Claude Code's
    ///   annotations are (`[Truncated. Full output: <path>]`); the words are
    ///   this project's and they say what the text is not.
    pub fn render(&self) -> String {
        match &self.payload {
            // Everything is present, so there is nothing to disclose and no
            // wrapper to pay for.
            Payload::Inline(text) => text.clone(),

            // Claude Code's marker, used the way it is used there: where the
            // cut would have been, carrying the count of what is gone. The
            // manifest's note follows, because a marker alone would be less
            // than full disclosure here — the whole payload was cut, not a
            // stretch of it. The way back, when the manifest holds one, is
            // printed from the manifest itself rather than left to the note:
            // `rerun` and `blob` are the fields other code reads, and a message
            // that named neither would be quieter than the structure is.
            Payload::Dropped(manifest) => {
                let mut out = format!(
                    "... [{} bytes truncated] ...\n\n{}",
                    manifest.omitted_bytes, manifest.note
                );
                if let Some(rerun) = &manifest.rerun {
                    out.push_str(&format!("\nRe-run `{rerun}` to obtain it."));
                } else if let Some(blob) = &manifest.blob {
                    out.push_str(&format!("\nThe full bytes are on disk at blob {blob}."));
                }
                out
            }

            // Claude Code's block for an output too large to keep, addressed by
            // the blob's digest. The instruction that follows is ours.
            Payload::Spilled {
                digest,
                pointer,
                manifest,
            } => format!(
                "<persisted-output>\nOutput too large ({size}). Full output saved to: {digest}\n</persisted-output>\n\n{pointer}\n\n{note}",
                size = human_bytes(manifest.omitted_bytes),
                note = manifest.note,
            ),

            // Ours: Claude Code has no form for a result that was replaced by a
            // summary of it.
            Payload::Reduced { text, manifest } => format!(
                "{text}\n\n[not the raw {kind} result: {omitted} of {size} bytes are not shown — {note}]",
                kind = self.kind.as_str(),
                omitted = manifest.omitted_bytes,
                size = self.size,
                note = manifest.note,
            ),
        }
    }

    /// Whether the payload is fully present.
    ///
    /// Only [`Payload::Inline`] is complete. A spilled payload is *recoverable*
    /// but not present; calling it complete would be exactly the kind of claim
    /// DESIGN forbids.
    pub fn is_complete(&self) -> bool {
        matches!(self.payload, Payload::Inline(_))
    }

    /// Bytes omitted, if any.
    pub fn omitted_bytes(&self) -> u64 {
        self.payload
            .manifest()
            .map(|m| m.omitted_bytes)
            .unwrap_or_default()
    }

    /// Bytes that are actually present in context.
    pub fn present_bytes(&self) -> u64 {
        match &self.payload {
            Payload::Inline(text) => text.len() as u64,
            Payload::Reduced { text, .. } => text.len() as u64,
            Payload::Dropped(_) | Payload::Spilled { .. } => 0,
        }
    }

    /// Build the `ContentBlock::ToolResult` to send back.
    ///
    /// The `tool_use_id` is replayed verbatim — `yu-wire` never parses,
    /// validates or synthesises ids (see `docs/WIRE-FACTS.md` Quirk 2), and
    /// neither does this.
    pub fn to_block(&self, tool_use_id: &str) -> ContentBlock {
        ContentBlock::ToolResult(ToolResultBlock::text(tool_use_id, self.render()))
    }

    /// Build the `ContentBlock::ToolResult` to send back, as an error.
    ///
    /// `message` is the caller's account of the failure; it is passed through
    /// unchanged so that the harness — not this crate — decides what a failure
    /// says.
    pub fn to_error_block(&self, tool_use_id: &str, message: &str) -> ContentBlock {
        ContentBlock::ToolResult(ToolResultBlock::error(tool_use_id, message))
    }
}

/// Bytes the way Claude Code's own formatter renders them.
///
/// `Ht` in the 2.1.248 bundle: below 1KB the count in bytes, otherwise one
/// decimal with a trailing `.0` dropped (`2048` is `2KB`, `9000` is `8.8KB`),
/// then MB, then GB. The number inside the `<persisted-output>` block is meant
/// to read the way the same number reads in Claude Code, so this is not "close
/// enough": `Ht` goes through JavaScript's `toFixed(1)`, which rounds a tie
/// away from zero — `(1.25).toFixed(1)` is `1.3` — where Rust's `{:.1}` rounds
/// it to even, `1.2`. The value is exactly `bytes / 2^n`, so the rounding is
/// done in integers and lands where `toFixed` lands; see [`one_decimal`].
fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    match bytes {
        b if b < KB => format!("{b} bytes"),
        b if b < MB => format!("{}KB", one_decimal(b as u128, KB as u128)),
        b if b < GB => format!("{}MB", one_decimal(b as u128, MB as u128)),
        b => format!("{}GB", one_decimal(b as u128, GB as u128)),
    }
}

/// `numerator / denominator` at one decimal, rounded half away from zero — the
/// rule JavaScript's `toFixed(1)` uses, computed in integers so no float
/// rounding can disagree at a tie. `denominator` is a power of two, so
/// `numerator / denominator` is exact and the integer result is the decimal
/// `toFixed` would produce.
fn one_decimal(numerator: u128, denominator: u128) -> String {
    let tenths = (numerator * 20 + denominator) / (2 * denominator);
    if tenths % 10 == 0 {
        format!("{}", tenths / 10)
    } else {
        format!("{}.{}", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dropped(omitted: u64, rerun: Option<&str>) -> ToolResult {
        ToolResult {
            payload: Payload::Dropped(Manifest {
                omitted_bytes: omitted,
                rerun: rerun.map(str::to_owned),
                note: "the command output was dropped".to_owned(),
                blob: None,
            }),
            kind: ResultKind::Log,
            size: omitted,
            reproducible: true,
            addressable: true,
            provenance: Provenance::new("exec", json!({"command": "cargo test"})),
        }
    }

    #[test]
    fn unbounded_shapes_are_search_and_log_only() {
        assert!(ResultKind::Search.is_unbounded());
        assert!(ResultKind::Log.is_unbounded());
        assert!(!ResultKind::File.is_unbounded());
        assert!(!ResultKind::Diff.is_unbounded());
        assert!(!ResultKind::Structured.is_unbounded());
    }

    #[test]
    fn as_str_is_the_wire_spelling_for_every_kind() {
        for kind in ResultKind::ALL {
            let value = serde_json::to_value(kind).unwrap();
            assert_eq!(value, Value::String(kind.as_str().to_owned()));
        }
    }

    #[test]
    fn inline_result_is_complete_and_renders_verbatim() {
        let r = ToolResult::inline("read_file", json!({"path": "a.rs"}), ResultKind::File, "hi");
        assert!(r.is_complete());
        assert_eq!(r.omitted_bytes(), 0);
        assert_eq!(r.present_bytes(), 2);
        assert_eq!(r.size, 2);
        assert_eq!(r.render(), "hi", "an inline payload needs no wrapper");
    }

    #[test]
    fn dropped_render_names_the_byte_count_and_the_rerun_command() {
        let r = dropped(4096, Some("cargo test"));
        let text = r.render();
        assert!(text.contains("4096"), "{text}");
        assert!(text.contains("cargo test"), "{text}");
        assert!(!r.is_complete());
        assert_eq!(r.omitted_bytes(), 4096);
        assert_eq!(r.present_bytes(), 0);
    }

    #[test]
    fn dropped_render_carries_claude_codes_truncation_marker() {
        let text = dropped(4096, Some("cargo test")).render();
        assert!(text.contains("... [4096 bytes truncated] ..."), "{text}");
        assert!(
            !text.contains("[reduction]"),
            "a self-invented prefix is a shape no model has seen: {text}"
        );
    }

    #[test]
    fn dropped_render_without_a_rerun_does_not_invent_one() {
        let r = dropped(10, None);
        let text = r.render();
        assert!(text.contains("10"), "{text}");
        assert!(
            !text.contains("Re-running"),
            "no re-run key was recorded, so none may be named: {text}"
        );
    }

    #[test]
    fn spilled_render_carries_the_pointer_and_the_instructed_digest() {
        let r = ToolResult {
            payload: Payload::Spilled {
                digest: "fnv1a64:0123456789abcdef".to_owned(),
                pointer: "Complete output saved as blob `fnv1a64:0123456789abcdef`.".to_owned(),
                manifest: Manifest {
                    omitted_bytes: 99,
                    rerun: None,
                    note: "spilled".to_owned(),
                    blob: Some("fnv1a64:0123456789abcdef".to_owned()),
                },
            },
            kind: ResultKind::Structured,
            size: 99,
            reproducible: false,
            addressable: true,
            provenance: Provenance::new("mcp__db__query", json!({})),
        };
        let text = r.render();
        assert!(text.contains("fnv1a64:0123456789abcdef"), "{text}");
        assert!(text.contains("99"), "{text}");
        assert!(text.contains("blob"), "{text}");
        assert!(!r.is_complete());
    }

    #[test]
    fn reduced_render_says_it_is_not_the_raw_result() {
        let r = ToolResult {
            payload: Payload::Reduced {
                text: "three errors, all in parser.rs".to_owned(),
                manifest: Manifest {
                    omitted_bytes: 5000,
                    rerun: None,
                    note: "read by the log_reader reader".to_owned(),
                    blob: None,
                },
            },
            kind: ResultKind::Log,
            size: 5030,
            reproducible: true,
            addressable: true,
            provenance: Provenance::new("exec", json!({"command": "cargo test"})),
        };
        let text = r.render();
        assert!(text.starts_with("three errors"), "{text}");
        assert!(text.contains("5000 of 5030 bytes are not shown"), "{text}");
        assert!(text.contains("not the raw log result"), "{text}");
        assert!(
            !text.contains("[reduction]"),
            "the note is ours; it must not wear an invented marker: {text}"
        );
        assert!(!r.is_complete());
    }

    #[test]
    fn human_bytes_matches_claude_codes_formatter() {
        // `Ht` in the 2.1.248 bundle: bytes below 1KB, one decimal above it
        // with a trailing `.0` dropped, then MB and GB.
        assert_eq!(human_bytes(0), "0 bytes");
        assert_eq!(human_bytes(1023), "1023 bytes");
        assert_eq!(human_bytes(1024), "1KB");
        assert_eq!(human_bytes(2048), "2KB");
        assert_eq!(human_bytes(9000), "8.8KB");
        assert_eq!(human_bytes(39_480), "38.6KB");
        assert_eq!(human_bytes(1_258_291), "1.2MB");
        // 1280 bytes is 1.25KB exactly — a tie. `(1.25).toFixed(1)` is `1.3`
        // and `{:.1}` on the same `f64` is `1.2`, so this pins the rounding
        // rule rather than the arithmetic: a float implementation fails here.
        assert_eq!(human_bytes(1280), "1.3KB");
    }

    #[test]
    fn tool_use_id_is_replayed_verbatim() {
        let r = ToolResult::inline("read_file", json!({}), ResultKind::File, "x");
        let block = r.to_block("call_00_4Ykgo3t3AxTqrrfx12mk4590");
        match block {
            ContentBlock::ToolResult(b) => {
                assert_eq!(b.tool_use_id, "call_00_4Ykgo3t3AxTqrrfx12mk4590");
                assert_eq!(b.is_error, None);
            }
            other => panic!("expected a tool_result, got {other:?}"),
        }
    }

    #[test]
    fn error_block_is_marked_as_an_error() {
        let r = ToolResult::inline("read_file", json!({}), ResultKind::File, "x");
        match r.to_error_block("toolu_missing", "no such tool: nope") {
            ContentBlock::ToolResult(b) => {
                assert_eq!(b.tool_use_id, "toolu_missing");
                assert_eq!(b.is_error, Some(true));
            }
            other => panic!("expected a tool_result, got {other:?}"),
        }
    }

    #[test]
    fn from_spill_is_absent_from_the_wire_when_unset() {
        let p = Provenance::new("read_file", json!({"path": "a"}));
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("from_spill").is_none());

        let mut p2 = p.clone();
        p2.from_spill = Some("fnv1a64:deadbeef".to_owned());
        let v2 = serde_json::to_value(&p2).unwrap();
        assert_eq!(v2["from_spill"], json!("fnv1a64:deadbeef"));
    }

    #[test]
    fn every_payload_round_trips_through_serde() {
        let cases = vec![
            ToolResult::inline("read_file", json!({"path": "a"}), ResultKind::File, "body"),
            dropped(7, Some("cargo test")),
            ToolResult {
                payload: Payload::Spilled {
                    digest: "d".to_owned(),
                    pointer: "p".to_owned(),
                    manifest: Manifest {
                        omitted_bytes: 3,
                        rerun: None,
                        note: "n".to_owned(),
                        blob: Some("d".to_owned()),
                    },
                },
                kind: ResultKind::Diff,
                size: 3,
                reproducible: false,
                addressable: true,
                provenance: Provenance::new("git_diff", json!({})),
            },
            ToolResult {
                payload: Payload::Reduced {
                    text: "t".to_owned(),
                    manifest: Manifest {
                        omitted_bytes: 1,
                        rerun: None,
                        note: "n".to_owned(),
                        blob: None,
                    },
                },
                kind: ResultKind::Search,
                size: 2,
                reproducible: true,
                addressable: true,
                provenance: Provenance::new("grep", json!({})),
            },
        ];
        for case in cases {
            let json = serde_json::to_string(&case).unwrap();
            let back: ToolResult = serde_json::from_str(&json).unwrap();
            assert_eq!(back, case);
        }
    }
}

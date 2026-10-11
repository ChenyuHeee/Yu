//! `spot_check` — the one deliberate channel for raw material.
//!
//! DESIGN's decision layer "**不读原始证据**": whether an assertion matches the
//! tool result is checked by the harness, deterministically and for free, and a
//! contradiction is reported as an event rather than by asking the layer to go
//! read things. The one exception is this tool, and DESIGN says why it can
//! afford one: "想看原始证据只有一条通道:`spot_check`,而它记在决策层自己的上下文账上"
//! — being billed to the scarcest context in the system is what keeps it a
//! scarce action rather than a second way to read everything.
//!
//! The module is deliberately thin: it holds the tool definition and the
//! resolver, and the loop owns the dispatch. `Ok` is answered to the model with
//! [`Spotted::text`]; `Err` with the error's `Display`, which is written to be
//! read by the model rather than by a log.
//!
//! The resolver's contract is DESIGN's manifest rule: **an address must
//! resolve**. An address that cannot be followed is a promise the harness does
//! not keep — `crate::render::NotLoadedEntry`'s own doc says an entry names how
//! to reach what it left out. Three forms are produced today (see
//! `crate::delegate::outcome_from_run`, the producer), and they are matched
//! here rather than replaced by a scheme of this module's invention:
//!
//! * `blob:<digest>` — a payload in the record's content-addressed store: a
//!   spilled tool result, or a recorded reply.
//! * `journal:seq=<n>` — a tool output kept inline on the log line.
//! * `journal:seq=<n> (dropped by reduction)` — an output a reduction
//!   discarded. The suffix is written precisely because there is nothing to
//!   fetch, and the answer for it is a **report, never an error**: a caller
//!   that cannot tell "the record says this was dropped" from a transport
//!   failure will retry forever, and DESIGN's rule is that the model sees the
//!   truth about what happened.
//!
//! Every distinct failure gets its own answer, because the model acts on the
//! text it gets back: a malformed address, an unknown scheme, a digest or seq
//! that is not there, and a payload that is not text are different facts about
//! the world, and each is reported as itself rather than collapsed into one
//! "could not read" that leaves the next move unguessable.

use serde_json::json;

use yu_journal::blob::is_digest;
use yu_journal::{Event, EventBody, Journal, JournalError};
use yu_wire::types::{CustomTool, ToolDefinition};

/// The tool's name, written once: the definition and any dispatch that looks
/// the call up by name must read the same constant, or "advertised" and
/// "accepted" drift apart in a way neither side can see.
pub const SPOT_CHECK: &str = "spot_check";

/// The most raw material one spot check loads into the decision layer.
///
/// The scarcity is the design, so it has to be a property of the mechanism and
/// not a hope: a spot check that could pull the whole record would make the
/// accounting a formality. The budget is smaller than the view it lands in (a
/// rendered view has measured around 10k characters), because the point of a
/// spot check is to see the piece of evidence behind a claim — not to become
/// the view's content.
pub const SPOT_BUDGET_BYTES: usize = 4_000;

/// The annotation the producer appends when a reduction discarded the payload.
///
/// Kept identical to the producer's spelling (`crate::delegate`) character for
/// character: this is a parsed suffix, not a description.
const DROPPED: &str = " (dropped by reduction)";

/// What an address resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spotted {
    /// The address as it was asked for, after trimming.
    pub address: String,
    /// What the model reads: the raw material, possibly cut, or — when the
    /// record holds nothing — a statement of that fact and of what the record
    /// does say. There is no other field the model needs to see, so the whole
    /// tool result is here.
    pub text: String,
    /// Bytes of raw material this call loaded into the decision layer's
    /// context: what the spot check is billed for. Zero when nothing was
    /// loaded — a dropped output has no size to bill, and saying so is the
    /// point of reporting it.
    pub bytes: u64,
    /// The size the record reports for the payload behind the address, when
    /// more of it exists than was loaded: a cut payload's whole size, or the
    /// size a dropped output was recorded at. `None` when everything behind
    /// the address was loaded, or when the record does not say.
    pub total_bytes: Option<u64>,
    /// Whether the payload outran [`SPOT_BUDGET_BYTES`] and `text` carries only
    /// its beginning. A cut is stated in `text` as well; a boolean alone would
    /// only be visible to a caller that already knew to look.
    pub cut: bool,
}

/// Why an address did not resolve.
///
/// The variants stay distinct because DESIGN's rule for the decision layer is
/// that it acts on the text it gets back: "there is no event there", "the bytes
/// are gone", "that is not an address", and "the record could not be read" call
/// for four different next moves, and one shared message would make them
/// indistinguishable.
#[derive(Debug, thiserror::Error)]
pub enum SpotError {
    /// The address does not parse against the scheme's own grammar. Reported
    /// with the reason and, in `Display`, with the grammar — a refused call is
    /// otherwise a dead end.
    ///
    /// The grammar is spelled out here rather than interpolated from `DROPPED`
    /// because an attribute cannot read it; the test that checks a refusal names
    /// every form is what keeps the two spellings from drifting.
    #[error(
        "`{address}` is not an address spot_check can follow: {reason}. A record address is one \
         of `blob:<64 hex characters>`, `journal:seq=<n>`, or `journal:seq=<n> (dropped by \
         reduction)`."
    )]
    Malformed { address: String, reason: String },

    /// A well-formed address in a scheme the record does not use.
    #[error(
        "`{scheme}:` is not a scheme the record uses, so nothing exists behind `{address}`. \
         Record addresses start with `blob:` (a payload in the store) or `journal:` (a position \
         in the log)."
    )]
    UnknownScheme { address: String, scheme: String },

    /// The digest is well-formed and the store has no payload under it: a
    /// reference the record makes that the disk cannot honour.
    #[error(
        "`{address}` is a well-formed address, but no payload is stored under that digest. The \
         record points at bytes that are not on disk — this is a loss, not an empty result, and \
         retrying the same address will say the same thing."
    )]
    BlobMissing { address: String, digest: String },

    /// The payload exists but is not valid UTF-8, so it cannot be handed over
    /// as text. Reported rather than lossily replaced: a caller shown mangled
    /// bytes would judge the material by something the record does not contain.
    #[error(
        "`{address}` resolves, but the payload behind it is not text (it is not valid UTF-8), \
         and spot_check can only hand over text. The bytes are still in the store at that \
         address."
    )]
    NotText { address: String, digest: String },

    /// The seq is well-formed and the record's history does not contain it.
    #[error(
        "`{address}` is a well-formed address, but seq {seq} is not part of this record's \
         history. Nothing is wrong with the address; there is simply no event with that seq — \
         check it against the manifest, which does not invent one."
    )]
    NoSuchEvent { address: String, seq: u64 },

    /// Reading the record itself failed. The one genuinely retryable failure,
    /// and it says so by being the only one that is about the read rather than
    /// about the material.
    #[error(
        "spot_check could not read the record: {0}. This is a read failure, not an answer about \
         the material — the address itself may be fine."
    )]
    Journal(#[from] JournalError),
}

/// The `spot_check` tool as the decision layer sees it.
///
/// The description is prompt surface, so it carries the two facts the layer
/// must know before calling: what an address is, and that the cost lands in its
/// own context — the accounting is the entire reason this tool is scarce.
pub fn spot_check_tool() -> ToolDefinition {
    ToolDefinition::Custom(CustomTool::new(
        SPOT_CHECK,
        "Look at raw material itself — the bytes behind an address from your manifest — rather \
         than at a report about it. This is the only channel through which evidence that was \
         reduced away can reach you, and it is charged to your own context: what you pull lands \
         here in full. Use it when judging a claim depends on the material itself, not on \
         someone's account of it. A payload larger than the tool's budget comes back cut, and \
         the result says where the rest is; an output a reduction dropped comes back saying \
         exactly that, because there is truly nothing to fetch.",
        json!({
            "type": "object",
            "properties": {
                "address": {
                    "type": "string",
                    "description": "An address from the manifest: `blob:<digest>`, \
                                    `journal:seq=<n>`, or `journal:seq=<n> (dropped by \
                                    reduction)`. Anything else is a refused call, not a silent \
                                    success."
                }
            },
            "required": ["address"],
            "additionalProperties": false
        }),
    ))
}

/// Resolve one address against the record.
///
/// `Ok` means the record answered — including the answers "the reduction
/// dropped this, so there is nothing to read" and "that event is not a tool
/// output", which are answers about the material rather than failures of the
/// read. `Err` means one of the specific failures [`SpotError`] names.
pub async fn resolve(address: &str, journal: &Journal) -> Result<Spotted, SpotError> {
    // Addresses travel as text out of a rendered view, and a copied one can
    // carry the whitespace around it. Refusing that would spend one of the
    // scarcest actions in the system on a formatting artifact.
    let address = address.trim();

    let Some((scheme, rest)) = address.split_once(':') else {
        return Err(SpotError::Malformed {
            address: address.to_owned(),
            reason: "it has no `scheme:` prefix".to_owned(),
        });
    };

    match scheme {
        "blob" => resolve_blob(address, rest, journal),
        "journal" => resolve_journal(address, rest, journal).await,
        other => Err(SpotError::UnknownScheme {
            address: address.to_owned(),
            scheme: other.to_owned(),
        }),
    }
}

/// `blob:<digest>` — a payload in the store.
fn resolve_blob(address: &str, digest: &str, journal: &Journal) -> Result<Spotted, SpotError> {
    // Validate before reading. `BlobStore::has` answers `false` for a malformed
    // digest too, and `false` there reads as "the bytes are gone" — a different
    // fact from "this is not an address", and the model's next move differs.
    if !is_digest(digest) {
        return Err(SpotError::Malformed {
            address: address.to_owned(),
            reason: format!(
                "`blob:` must be followed by a sha256 digest in hex, and `{digest}` is not one"
            ),
        });
    }
    match journal.blobs().get_str(digest) {
        Ok(text) => Ok(loaded(address, &text, &[])),
        Err(JournalError::BlobMissing { .. }) => Err(SpotError::BlobMissing {
            address: address.to_owned(),
            digest: digest.to_owned(),
        }),
        Err(JournalError::NotUtf8 { .. }) => Err(SpotError::NotText {
            address: address.to_owned(),
            digest: digest.to_owned(),
        }),
        Err(other) => Err(SpotError::Journal(other)),
    }
}

/// `journal:seq=<n>` and its dropped annotation.
///
/// The record is the authority, not the annotation: the suffix says what the
/// producer saw when it wrote the address, and the event says what is actually
/// there. They agree in every address the producer emits today; where they do
/// not, the record wins and the disagreement is stated, because a caller that
/// trusts a stale annotation either misses bytes that exist or waits for bytes
/// that never will.
async fn resolve_journal(
    address: &str,
    rest: &str,
    journal: &Journal,
) -> Result<Spotted, SpotError> {
    let (seq, claimed_dropped) = parse_seq(address, rest)?;
    let Some(event) = event_at(journal, seq).await? else {
        return Err(SpotError::NoSuchEvent {
            address: address.to_owned(),
            seq,
        });
    };

    let EventBody::ToolResult {
        name,
        bytes,
        inline,
        blob,
        reduction,
        reproducible,
        ..
    } = &event.body
    else {
        return Ok(nothing(
            address,
            None,
            &format!(
                "seq {seq} holds a `{}` event, not a tool output. `journal:seq=<n>` names a tool \
                 result kept inline in the log, and there is none behind this address.",
                event.body.kind()
            ),
        ));
    };

    let mismatch = claimed_dropped.then(|| {
        "the address claims this output was dropped by reduction, but the record holds its \
         payload — the record is what is shown above"
            .to_owned()
    });

    match (inline, blob) {
        (Some(text), _) => match mismatch {
            Some(note) => Ok(loaded(address, text, &[note])),
            None => Ok(loaded(address, text, &[])),
        },
        (None, Some(digest)) => {
            let text = journal.blobs().get_str(digest).map_err(|err| match err {
                JournalError::BlobMissing { .. } => SpotError::BlobMissing {
                    address: address.to_owned(),
                    digest: digest.clone(),
                },
                JournalError::NotUtf8 { .. } => SpotError::NotText {
                    address: address.to_owned(),
                    digest: digest.clone(),
                },
                other => SpotError::Journal(other),
            })?;
            let mut notes = vec![format!(
                "this event keeps its payload out of line at `blob:{digest}`"
            )];
            notes.extend(mismatch);
            Ok(loaded(address, &text, &notes))
        }
        (None, None) => Ok(nothing(
            address,
            Some(*bytes),
            &dropped_account(name, *bytes, *reproducible, reduction.as_deref()),
        )),
    }
}

/// Parse `seq=<n>`, optionally followed by the dropped annotation.
fn parse_seq(address: &str, rest: &str) -> Result<(u64, bool), SpotError> {
    let Some(number) = rest.strip_prefix("seq=") else {
        return Err(SpotError::Malformed {
            address: address.to_owned(),
            reason: format!("`journal:` names an event as `seq=<n>`, and `{rest}` is not that"),
        });
    };
    let (digits, dropped) = match number.strip_suffix(DROPPED) {
        Some(digits) => (digits, true),
        None => (number, false),
    };
    let seq = digits.parse::<u64>().map_err(|_| SpotError::Malformed {
        address: address.to_owned(),
        reason: format!(
            "`{digits}` is not an event number; the address reads `journal:seq=<n>`, optionally \
             ending in `{DROPPED}`"
        ),
    })?;
    Ok((seq, dropped))
}

/// The event with this seq, or `None` when the record's history has none.
///
/// The branch's own index holds only what the branch recorded itself, and
/// `read_range` is a bounded read into that index — which is what makes
/// resolving one seq cost one line rather than the file. A branch numbers
/// continuously with its parent (`Journal::open_fork`), so an address made
/// before the fork names an event the branch *inherited* rather than recorded;
/// the lineage read is what finds those, and answering "no such event" for one
/// would be false.
async fn event_at(journal: &Journal, seq: u64) -> Result<Option<Event>, SpotError> {
    let Some(upper) = seq.checked_add(1) else {
        return Ok(None);
    };
    if let Some(event) = journal.read_range(seq, upper).await?.into_iter().next()
        && event.seq == seq
    {
        return Ok(Some(event));
    }
    if journal.lineage().is_empty() {
        return Ok(None);
    }
    Ok(journal
        .history_prefix(upper)
        .await?
        .into_iter()
        .find(|event| event.seq == seq))
}

/// Render one payload the record holds, cut to the budget.
fn loaded(address: &str, payload: &str, notes: &[String]) -> Spotted {
    let total = payload.len();
    let (body, cut) = cut_to_budget(payload);

    let mut text = if cut {
        format!("SPOT CHECK — {address} — {} of {total} bytes\n", body.len())
    } else {
        format!("SPOT CHECK — {address} — {total} bytes\n")
    };
    text.push_str(body);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    for note in notes {
        text.push_str(&format!("[{note}]\n"));
    }
    if cut {
        // DESIGN's rule for a truncated result: state the cut and say where the
        // rest is. Delegation is the channel, because a second spot check of
        // the same address would pull the same prefix.
        text.push_str(&format!(
            "[cut after {} of {total} bytes; the full payload is still in the record at \
             {address} — delegate a reader for the rest rather than pulling it in here]\n",
            body.len()
        ));
    }

    Spotted {
        address: address.to_owned(),
        text,
        bytes: body.len() as u64,
        total_bytes: cut.then_some(total as u64),
        cut,
    }
}

/// Render an address that resolves to nothing to read.
///
/// `Ok`, not `Err`: the record answered. The account says what it does know, so
/// the model is not left treating "there is nothing here" as a mystery to be
/// retried.
fn nothing(address: &str, total_bytes: Option<u64>, account: &str) -> Spotted {
    Spotted {
        address: address.to_owned(),
        text: format!("SPOT CHECK — {address} — nothing to read\n{account}\n"),
        bytes: 0,
        total_bytes,
        cut: false,
    }
}

/// What the record says about an output a reduction discarded.
///
/// The event keeps the shape of what was dropped even though it kept none of
/// the bytes, and that shape is the only honest thing to report: the payload is
/// not here, here is what it was, and here is the one route back to it (or the
/// statement that there is none).
fn dropped_account(name: &str, bytes: u64, reproducible: bool, reduction: Option<&str>) -> String {
    let mut out = match reduction {
        Some(note) => format!(
            "The record says this output was dropped by its reduction (`{note}`), so nothing is \
             stored behind this address: a `{name}` result of {bytes} bytes, "
        ),
        None => format!(
            "The record holds no payload for this output and no note saying why, so nothing is \
             stored behind this address: a `{name}` result of {bytes} bytes, "
        ),
    };
    if reproducible {
        out.push_str("reproducible.\n");
        out.push_str(&format!(
            "Re-running `{name}` through a delegation is how to get those bytes again; this \
             address will keep answering the same way."
        ));
    } else {
        out.push_str("not reproducible.\n");
        out.push_str(
            "Re-running it is not a way back to those bytes, and the record kept their size and \
             shape but not their content; this address will keep answering the same way.",
        );
    }
    out
}

/// Cut `text` to [`SPOT_BUDGET_BYTES`], on a character boundary.
fn cut_to_budget(text: &str) -> (&str, bool) {
    if text.len() <= SPOT_BUDGET_BYTES {
        return (text, false);
    }
    let mut end = SPOT_BUDGET_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use yu_journal::SessionId;
    use yu_observe::{AgentId, Timeline};
    use yu_tools::Tool;

    use super::*;

    use crate::agent::test_support::{FakeEndpoint, final_reply, scratch_dir};
    use crate::delegate::{DelegateArgs, DelegateOutcome, run_delegation};

    /// One real delegation against a real journal.
    ///
    /// Every test here takes its addresses from what this produced rather than
    /// writing them out, because a resolver tested against the author's belief
    /// about the format is testing the belief.
    async fn run(
        name: &str,
        replies: Vec<String>,
        files: &[(&str, &str)],
    ) -> (PathBuf, Arc<Journal>, DelegateOutcome) {
        let dir = scratch_dir(name);
        let journal = Arc::new(Journal::open(&dir, SessionId::new("s")).await.unwrap());
        for (path, content) in files {
            std::fs::write(dir.join(path), content).unwrap();
        }
        let fake = FakeEndpoint::scripted(replies);
        let tools = Arc::new(yu_tools::ToolRegistry::native(&dir));
        let outcome = run_delegation(
            &DelegateArgs::new("do the work"),
            fake.client(),
            journal.clone(),
            tools,
            Timeline::new(),
            dir.clone(),
        )
        .await
        .expect("the executor finishes on its second turn");
        (dir, journal, outcome)
    }

    /// A turn that asks for one tool call.
    ///
    /// The thinking block is not decoration: the strict history rule requires
    /// one with every `tool_use`.
    fn tool_turn(name: &str, input: serde_json::Value) -> String {
        serde_json::json!({
            "id": "msg-fake",
            "model": "fake-model",
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "working", "signature": "sig"},
                {"type": "tool_use", "id": "call_00_fake", "name": name, "input": input}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
        .to_string()
    }

    /// The address of a small `exec` result, which is the producer's inline
    /// form, plus the journal that holds it.
    async fn inline_address(name: &str) -> (PathBuf, Arc<Journal>, String, String) {
        let command = "printf 'six bytes'; exit 3";
        let (dir, journal, outcome) = run(
            name,
            vec![
                tool_turn("exec", serde_json::json!({"command": command})),
                final_reply("done"),
            ],
            &[],
        )
        .await;
        let address = outcome.evidence[0]
            .address
            .clone()
            .expect("the producer addresses its evidence");
        (dir, journal, address, command.to_owned())
    }

    #[tokio::test]
    async fn an_inline_result_resolves_to_the_bytes_the_tool_printed() {
        // The address comes from `outcome_from_run`; the expected text comes
        // from running the same command through the same tool. Neither is a
        // transcription of the format by this test.
        let (_dir, journal, address, command) = inline_address("spot-inline").await;

        let spotted = resolve(&address, &journal)
            .await
            .expect("an inline result must resolve");
        assert!(!spotted.cut);

        let ctx = yu_tools::native::ToolContext::with_default_budget(std::env::temp_dir());
        let printed = yu_tools::native::Exec
            .execute(&serde_json::json!({"command": command}), &ctx)
            .payload
            .text()
            .expect("exec returns text")
            .to_owned();

        assert!(spotted.text.contains(&printed), "{}", spotted.text);
        assert_eq!(
            spotted.bytes,
            printed.len() as u64,
            "the size loaded is what the decision layer is billed for"
        );
        assert_eq!(spotted.total_bytes, None, "nothing was left behind");
    }

    #[tokio::test]
    async fn a_recorded_reply_resolves_from_the_address_the_producer_gave_it() {
        let reply = "everything passed; 12 tests, 0 failures";
        let (_dir, journal, outcome) = run("spot-reply", vec![final_reply(reply)], &[]).await;
        let address = outcome
            .report_address
            .clone()
            .expect("the recorded reply has an address");
        assert!(address.starts_with("blob:"), "{address}");

        let spotted = resolve(&address, &journal)
            .await
            .expect("a recorded reply must resolve");
        assert!(spotted.text.contains(reply), "{}", spotted.text);
        assert_eq!(spotted.bytes, reply.len() as u64);
        assert!(!spotted.cut);
    }

    #[tokio::test]
    async fn an_oversized_payload_comes_back_cut_and_says_where_the_rest_is() {
        // The interesting failure is the silent one: a cut that does not say it
        // cut, which reads exactly like a whole payload — the loss DESIGN
        // forbids, in the tool that exists to show what a reduction threw away.
        let reply = format!("{}\nTAIL-MARKER", "x".repeat(SPOT_BUDGET_BYTES * 2));
        let (_dir, journal, outcome) = run("spot-cut", vec![final_reply(&reply)], &[]).await;
        let address = outcome.report_address.clone().unwrap();

        let spotted = resolve(&address, &journal).await.unwrap();
        assert!(spotted.cut);
        assert_eq!(spotted.bytes, SPOT_BUDGET_BYTES as u64);
        assert_eq!(spotted.total_bytes, Some(reply.len() as u64));
        assert!(
            !spotted.text.contains("TAIL-MARKER"),
            "the tail must not have been loaded: {}",
            spotted.text
        );
        assert!(
            spotted.text.contains(&format!("of {} bytes", reply.len())),
            "the cut must state both sizes: {}",
            spotted.text
        );
        assert!(
            spotted.text.contains(&address) && spotted.text.contains("delegate a reader"),
            "a cut must say where the rest is: {}",
            spotted.text
        );
    }

    #[tokio::test]
    async fn a_dropped_output_resolves_to_the_records_own_account_not_an_error() {
        // A large reproducible read is the drop path. The producer emits the
        // dropped annotation for it, and the answer must be an account — a
        // caller that cannot tell it from a transport failure retries forever.
        let content = "l".repeat(64 * 1024);
        let (_dir, journal, outcome) = run(
            "spot-dropped",
            vec![
                tool_turn("read_file", serde_json::json!({"path": "big.txt"})),
                final_reply("done"),
            ],
            &[("big.txt", &content)],
        )
        .await;
        let entry = outcome
            .omitted
            .iter()
            .find(|e| e.address.ends_with(DROPPED))
            .unwrap_or_else(|| {
                panic!(
                    "the producer must have emitted a dropped address: {:#?}",
                    outcome.omitted
                )
            });
        let address = entry.address.clone();

        let spotted = resolve(&address, &journal)
            .await
            .expect("a dropped output is an answer, not an error");
        assert!(!spotted.cut);
        assert_eq!(
            spotted.bytes, 0,
            "nothing was loaded, so nothing may be billed as loaded raw material"
        );
        assert_eq!(
            spotted.total_bytes,
            Some(content.len() as u64),
            "the record kept the size of what it dropped"
        );
        assert!(
            spotted.text.contains("dropped") && spotted.text.contains("read_file"),
            "the account must say what was dropped: {}",
            spotted.text
        );
        assert!(
            !spotted.text.contains(&content),
            "the payload was dropped; showing it would be fabrication"
        );
    }

    #[tokio::test]
    async fn an_address_that_says_dropped_over_a_payload_that_exists_reports_the_record() {
        // The suffix is the producer's claim; the event is the fact. A stale
        // annotation must not hide bytes that exist, and the disagreement is
        // itself worth stating.
        let (_dir, journal, address, _command) = inline_address("spot-claimed-dropped").await;
        let claimed = format!("{address}{DROPPED}");

        let spotted = resolve(&claimed, &journal)
            .await
            .expect("the record answers, regardless of what the address claims");
        assert!(
            spotted.text.contains("six bytes"),
            "the record holds the payload; the claim must not hide it: {}",
            spotted.text
        );
        assert!(
            spotted.text.contains("claims this output was dropped"),
            "the disagreement must be stated: {}",
            spotted.text
        );
    }

    #[tokio::test]
    async fn a_plain_seq_address_over_a_dropped_output_still_says_nothing_is_there() {
        // The other direction: an address without the suffix over an event that
        // was dropped. Trusting the annotation would render this as an empty
        // success, which is the silent loss the suffix exists to prevent.
        let content = "k".repeat(64 * 1024);
        let (_dir, journal, outcome) = run(
            "spot-plain-dropped",
            vec![
                tool_turn("read_file", serde_json::json!({"path": "big.txt"})),
                final_reply("done"),
            ],
            &[("big.txt", &content)],
        )
        .await;
        let address = outcome
            .omitted
            .iter()
            .find(|e| e.address.ends_with(DROPPED))
            .expect("a dropped address was produced")
            .address
            .strip_suffix(DROPPED)
            .expect("the suffix is what we strip")
            .to_owned();

        let spotted = resolve(&address, &journal).await.unwrap();
        assert_eq!(spotted.bytes, 0);
        assert!(
            spotted.text.contains("dropped") && spotted.text.contains("nothing to read"),
            "{}",
            spotted.text
        );
    }

    #[tokio::test]
    async fn a_blob_that_is_gone_is_reported_as_a_loss_not_as_emptiness() {
        // The producer's own address, made unreachable the way references
        // actually become unreachable: the file is removed.
        let (_dir, journal, outcome) = run("spot-missing", vec![final_reply("recorded")], &[]).await;
        let address = outcome.report_address.clone().unwrap();
        let digest = address.strip_prefix("blob:").unwrap().to_owned();
        let path = journal
            .blobs()
            .root()
            .join(&digest[..2])
            .join(&digest[2..]);
        std::fs::remove_file(path).unwrap();
        assert!(!journal.blobs().has(&digest));

        let err = resolve(&address, &journal).await.unwrap_err();
        match &err {
            SpotError::BlobMissing { digest: d, .. } => assert_eq!(d, &digest),
            other => panic!("a reference with nothing behind it must be reported, got {other:?}"),
        }
        let text = err.to_string();
        assert!(text.contains(&digest), "{text}");
        assert!(text.contains("not on disk"), "{text}");
    }

    #[tokio::test]
    async fn a_seq_that_is_not_in_the_record_says_so_rather_than_emptying() {
        // "Not there" is a fact about the record, not a read failure and not an
        // empty payload: an empty success would read as evidence that the record
        // held nothing, which is the opposite of what it says.
        let (_dir, journal, address, _command) = inline_address("spot-no-seq").await;
        let seq: u64 = address
            .strip_prefix("journal:seq=")
            .expect("the producer's inline form")
            .parse()
            .unwrap();

        let err = resolve(&format!("journal:seq={}", seq + 1_000), &journal)
            .await
            .unwrap_err();
        match &err {
            SpotError::NoSuchEvent { seq: s, .. } => assert_eq!(*s, seq + 1_000),
            other => panic!("expected NoSuchEvent, got {other:?}"),
        }
        assert!(
            err.to_string().contains("not part of this record"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_malformed_address_is_refused_with_the_grammar_it_broke() {
        let (_dir, journal, _outcome) = run("spot-malformed", vec![final_reply("done")], &[]).await;

        for bad in [
            "",
            "notes.txt",
            "blob:",
            "blob:not-hex",
            "journal:",
            "journal:5",
            "journal:seq=",
            "journal:seq=abc",
            "journal:seq=5 (dropped)",
        ] {
            match resolve(bad, &journal).await {
                Err(SpotError::Malformed { address, .. }) => assert_eq!(address, bad),
                other => panic!("`{bad}` must be refused as malformed, got {other:?}"),
            }
        }

        // A refusal is only useful if it teaches the grammar it wanted.
        let text = resolve("journal:5", &journal).await.unwrap_err().to_string();
        for form in ["blob:", "journal:seq=<n>", DROPPED] {
            assert!(text.contains(form), "the refusal must name `{form}`: {text}");
        }
    }

    #[tokio::test]
    async fn an_unknown_scheme_is_not_confused_with_a_missing_event() {
        let (_dir, journal, _outcome) = run("spot-scheme", vec![final_reply("done")], &[]).await;

        let err = resolve("file:notes.txt", &journal).await.unwrap_err();
        match &err {
            SpotError::UnknownScheme { scheme, .. } => assert_eq!(scheme, "file"),
            other => panic!("expected UnknownScheme, got {other:?}"),
        }
        let text = err.to_string();
        assert!(text.contains("blob:") && text.contains("journal:"), "{text}");

        // The four failure texts must not be interchangeable, or the model
        // cannot choose a next move from them.
        let malformed = resolve("notes.txt", &journal).await.unwrap_err().to_string();
        let missing = resolve("journal:seq=999999", &journal)
            .await
            .unwrap_err()
            .to_string();
        assert_ne!(text, malformed);
        assert_ne!(text, missing);
        assert_ne!(malformed, missing);
    }

    #[tokio::test]
    async fn a_binary_payload_is_reported_rather_than_replaced() {
        let (_dir, journal, _outcome) = run("spot-binary", vec![final_reply("done")], &[]).await;
        let digest = journal.blobs().put(&[0xff, 0xfe, 0x00]).unwrap();

        let err = resolve(&format!("blob:{digest}"), &journal)
            .await
            .unwrap_err();
        match &err {
            SpotError::NotText { digest: d, .. } => assert_eq!(d, &digest),
            other => panic!("expected NotText, got {other:?}"),
        }
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    #[tokio::test]
    async fn an_address_copied_with_whitespace_around_it_still_resolves() {
        let (_dir, journal, address, _command) = inline_address("spot-whitespace").await;
        let padded = format!("  {address}\n");
        let spotted = resolve(&padded, &journal)
            .await
            .expect("whitespace around an address is not a wrong address");
        assert_eq!(spotted.address, address);
    }

    #[tokio::test]
    async fn a_seq_that_names_a_non_tool_event_says_what_it_is() {
        let (_dir, journal, _outcome) = run("spot-non-tool", vec![final_reply("done")], &[]).await;
        let first = journal
            .read_range(0, 1)
            .await
            .unwrap()
            .into_iter()
            .next()
            .expect("the run recorded something");

        let spotted = resolve(&format!("journal:seq={}", first.seq), &journal)
            .await
            .expect("the record answers with what the event is");
        assert_eq!(spotted.bytes, 0);
        assert!(
            spotted.text.contains(first.body.kind()) && spotted.text.contains("not a tool output"),
            "{}",
            spotted.text
        );
    }

    #[tokio::test]
    async fn a_seq_whose_payload_is_stored_out_of_line_resolves_too() {
        // The seq address names the event; where the payload is kept is the
        // record's business. Following it is one call instead of two, and the
        // result names the store address it went through.
        let (_dir, journal, _outcome) = run("spot-seq-blob", vec![final_reply("done")], &[]).await;
        let payload = "payload kept out of line";
        let digest = journal.blobs().put_str(payload).unwrap();
        let event = journal
            .append(
                AgentId::new("exec"),
                EventBody::ToolResult {
                    tool_use_id: "call_x".into(),
                    name: "read_file".into(),
                    kind: "file".into(),
                    bytes: payload.len() as u64,
                    reproducible: true,
                    addressable: true,
                    inline: None,
                    blob: Some(digest.clone()),
                    reduction: Some("spill_with_pointer".into()),
                },
            )
            .await
            .unwrap();

        let spotted = resolve(&format!("journal:seq={}", event.seq), &journal)
            .await
            .expect("the event resolved, so its payload must too");
        assert!(spotted.text.contains(payload), "{}", spotted.text);
        assert!(
            spotted.text.contains(&digest),
            "the result must say where the payload actually lives: {}",
            spotted.text
        );
    }

    #[tokio::test]
    async fn an_address_from_before_a_fork_resolves_through_the_lineage() {
        // A branch numbers continuously with its parent and copies nothing, so
        // an address made before the fork names an event the branch inherited.
        // Its own log cannot answer for that event, and "no such event" would
        // be false — the lineage read is what keeps the address a promise.
        let (_dir, parent, outcome) = run(
            "spot-fork",
            vec![
                tool_turn("exec", serde_json::json!({"command": "printf 'before the fork'"})),
                final_reply("done"),
            ],
            &[],
        )
        .await;
        let address = outcome.evidence[0]
            .address
            .clone()
            .expect("the producer addresses its evidence");
        let seq: u64 = address
            .strip_prefix("journal:seq=")
            .expect("the producer's inline form")
            .parse()
            .unwrap();

        let branch_dir = scratch_dir("spot-fork-branch");
        let child = Journal::open_fork(
            &branch_dir,
            SessionId::new("branch"),
            parent.clone(),
            parent.len() as u64,
        )
        .await
        .unwrap();

        // The address the parent's run produced, asked of the branch.
        assert!(
            child.read_range(seq, seq + 1).await.unwrap().is_empty(),
            "the branch's own log must not hold a pre-fork event, or this test proves nothing"
        );

        let spotted = resolve(&format!("journal:seq={seq}"), &child)
            .await
            .expect("an inherited address must resolve in the branch that inherited it");
        assert!(spotted.text.contains("before the fork"), "{}", spotted.text);
    }

    #[test]
    fn the_tool_says_where_its_addresses_come_from_and_that_this_layer_pays() {
        let ToolDefinition::Custom(tool) = spot_check_tool() else {
            panic!("spot_check must be a client-defined tool");
        };
        assert_eq!(tool.name, SPOT_CHECK);
        let desc = tool.description.clone().unwrap_or_default();
        // The accounting is the reason the tool is scarce, so the description
        // has to carry it — a caller that does not know it is paying will treat
        // this as an ordinary read.
        assert!(desc.contains("charged to your own context"), "{desc}");
        assert!(desc.contains("reduced away"), "{desc}");

        assert_eq!(tool.input_schema["required"], json!(["address"]));
        assert_eq!(tool.input_schema["additionalProperties"], json!(false));
        assert_eq!(tool.input_schema["properties"]["address"]["type"], json!("string"));
        let address_desc = tool.input_schema["properties"]["address"]["description"]
            .as_str()
            .unwrap();
        for form in ["blob:<digest>", "journal:seq=<n>", "dropped by reduction"] {
            assert!(address_desc.contains(form), "{address_desc}");
        }
    }
}

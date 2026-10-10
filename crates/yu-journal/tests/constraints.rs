//! DESIGN's four L2 engineering constraints, each demonstrated against real files.
//!
//! These are the point of the crate: every one of them is something existing
//! harnesses get wrong, so each test asserts the *shape of the storage*, not just
//! that a round-trip works.

mod common;

use common::{agent, blob_files, journal_in, log_text, occurrences_under, tempdir};
use yu_journal::{EventBody, JournalError};
use yu_observe::TokenUsage;

/// Constraint 1: a tool result payload is stored exactly once — inline (small) or
/// as a blob reference (large), never in both places.
#[tokio::test]
async fn a_large_tool_result_is_stored_once() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;

    // A payload big enough that inlining it would be the reduction failure the
    // design is about, with a marker so its copies can be counted.
    let marker = "TOOL-RESULT-MARKER-3f9c";
    let payload = format!("{}{marker}{}", "x".repeat(64 * 1024), "y".repeat(64 * 1024));
    let digest = journal.blobs().put_str(&payload).unwrap();

    let event = journal
        .append(
            agent(),
            EventBody::ToolResult {
                tool_use_id: "call_01".into(),
                name: "read".into(),
                kind: "file".into(),
                bytes: payload.len() as u64,
                reproducible: true,
                addressable: true,
                inline: None,
                blob: Some(digest.clone()),
                reduction: None,
            },
        )
        .await
        .unwrap();

    // The event holds a reference, not the payload.
    let EventBody::ToolResult { inline, blob, .. } = &event.body else {
        panic!("expected a tool result, got {:?}", event.body);
    };
    assert_eq!(inline.as_deref(), None);
    assert_eq!(blob.as_deref(), Some(digest.as_str()));

    // Once across the whole journal: the line has the digest, the store has the
    // bytes, and nothing has both.
    assert_eq!(occurrences_under(journal.dir(), marker), 1);
    assert_eq!(blob_files(journal.dir()).len(), 1);
    assert!(!log_text(&journal).contains(marker));
    assert_eq!(journal.blobs().get_str(&digest).unwrap(), payload);
    assert_eq!(
        journal.blobs().size(&digest),
        Some(payload.len() as u64),
        "the recorded size is the blob's size"
    );
}

/// The other half of constraint 1: a small payload is inlined and is *also*
/// stored exactly once — inline is a storage decision, not an excuse to keep a
/// second copy in the blob store.
#[tokio::test]
async fn a_small_tool_result_is_inlined_and_not_stored_twice() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;
    let payload = "SMALL-RESULT-MARKER-11b7";

    let event = journal
        .append(
            agent(),
            EventBody::ToolResult {
                tool_use_id: "call_02".into(),
                name: "bash".into(),
                kind: "structured".into(),
                bytes: payload.len() as u64,
                reproducible: false,
                addressable: false,
                inline: Some(payload.to_owned()),
                blob: None,
                reduction: None,
            },
        )
        .await
        .unwrap();

    let EventBody::ToolResult { inline, blob, .. } = &event.body else {
        panic!("expected a tool result");
    };
    assert_eq!(inline.as_deref(), Some(payload));
    assert_eq!(blob.as_deref(), None);

    assert_eq!(occurrences_under(journal.dir(), payload), 1);
    assert!(blob_files(journal.dir()).is_empty());
}

/// And the case that is neither: a payload a reduction dropped on purpose. It is
/// absent from disk, and the reduction note is what says so — a drop is an event,
/// not a silence.
#[tokio::test]
async fn a_dropped_payload_is_absent_and_says_why() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;

    journal
        .append(
            agent(),
            EventBody::ToolResult {
                tool_use_id: "call_03".into(),
                name: "bash".into(),
                kind: "log".into(),
                bytes: 4 * 1024 * 1024,
                reproducible: true,
                addressable: false,
                inline: None,
                blob: None,
                reduction: Some("dropped 4194304 bytes; re-run `cargo test`".into()),
            },
        )
        .await
        .unwrap();

    assert!(blob_files(journal.dir()).is_empty());
    assert!(log_text(&journal).contains("dropped 4194304 bytes"));
}

/// Constraint 1 as a refusal: both fields set is exactly the duplication Claude
/// Code's transcript has, and it must not be storable.
#[tokio::test]
async fn a_payload_in_two_places_is_refused() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;
    let digest = journal.blobs().put_str("hello").unwrap();

    let err = journal
        .append(
            agent(),
            EventBody::ToolResult {
                tool_use_id: "call_04".into(),
                name: "read".into(),
                kind: "file".into(),
                bytes: 5,
                reproducible: true,
                addressable: true,
                inline: Some("hello".into()),
                blob: Some(digest),
                reduction: None,
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, JournalError::PayloadStoredTwice));
    assert!(journal.is_empty());
    assert_eq!(log_text(&journal), "");
}

/// Constraint 2: one JSONL line per event, however many content blocks it covers.
#[tokio::test]
async fn one_line_per_event_even_with_many_content_blocks() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;

    // A turn of the shape WIRE-FACTS describes: many blocks in one response.
    let blocks: Vec<String> = (0..64).map(|i| format!("block-{i}")).collect();
    journal
        .append(
            agent(),
            EventBody::ModelResponse {
                id: "4aeac4b3-5f44-4b94-bef3-cb721ea7bef0".into(),
                stop_reason: "tool_use".into(),
                usage: TokenUsage {
                    input_tokens: 256,
                    output_tokens: 128,
                    cache_read_input_tokens: 2299,
                    cache_creation_input_tokens: 0,
                },
                blocks: blocks.clone(),
                thinking_blob: None,
                thinking_signature: None,
            },
        )
        .await
        .unwrap();

    let text = log_text(&journal);
    assert_eq!(text.lines().count(), 1, "64 blocks, still one line");
    assert_eq!(journal.len(), 1);
    // The envelope is written once — this is the counter that a per-block line
    // would multiply by 64.
    assert_eq!(text.matches("\"seq\":").count(), 1);
    assert_eq!(text.matches("\"session\":").count(), 1);
    assert_eq!(text.matches("\"blocks\":").count(), 1);

    let events = journal.events().await.unwrap();
    let EventBody::ModelResponse { blocks: got, .. } = &events[0].body else {
        panic!("expected a model response");
    };
    assert_eq!(got, &blocks);
}

/// The same constraint against the thing that would actually break it: a payload
/// containing newlines. The log is line-oriented, so a payload that is not
/// escaped would split one event across many lines — which is how a transcript
/// ends up with one line per fragment.
#[tokio::test]
async fn a_payload_full_of_newlines_still_occupies_one_line() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;
    let payload = "line one\nline two\r\nline three\n";

    journal
        .append(
            agent(),
            EventBody::ToolResult {
                tool_use_id: "call_05".into(),
                name: "bash".into(),
                kind: "log".into(),
                bytes: payload.len() as u64,
                reproducible: false,
                addressable: false,
                inline: Some(payload.to_owned()),
                blob: None,
                reduction: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(log_text(&journal).lines().count(), 1);
    let events = journal.events().await.unwrap();
    let EventBody::ToolResult { inline, .. } = &events[0].body else {
        panic!("expected a tool result");
    };
    assert_eq!(inline.as_deref(), Some(payload), "escaped, not mangled");
}

/// Constraint 3: material re-injected every turn is stored once and referenced by
/// digest afterwards.
#[tokio::test]
async fn repeated_injections_deduplicate_to_one_blob_and_one_digest() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;

    let reminder = "TASK-REMINDER-MARKER-77ab: stay inside the declared file set";
    let mut digests = Vec::new();
    for turn in 0..5 {
        let body = EventBody::injected("task_reminder", reminder, journal.blobs()).unwrap();
        digests.push(match &body {
            EventBody::Injected { blob, .. } => blob.clone(),
            other => panic!("expected an injection, got {other:?}"),
        });
        let event = journal
            .append(
                agent(),
                EventBody::Note {
                    text: format!("turn {turn}"),
                },
            )
            .await
            .unwrap();
        // Two events per turn: the turn itself, then the reminder that is
        // re-injected into it.
        assert_eq!(event.seq, turn * 2);
        journal.append(agent(), body).await.unwrap();
    }

    // Same material, one digest, one file — and the text itself appears once.
    assert!(digests.windows(2).all(|w| w[0] == w[1]));
    assert_eq!(blob_files(journal.dir()).len(), 1);
    assert_eq!(occurrences_under(journal.dir(), reminder), 1);
    assert_eq!(journal.blobs().get_str(&digests[0]).unwrap(), reminder);

    // Five injections, ten events, and each injection costs a 64-character
    // reference in the log rather than a re-copy of the reminder.
    let text = log_text(&journal);
    assert_eq!(text.lines().count(), 10);
    assert_eq!(text.matches(&digests[0]).count(), 5);
    assert!(!text.contains(reminder));
}

/// Constraint 4: thinking text and signature are stored separately, and neither
/// can swallow the other.
#[tokio::test]
async fn thinking_text_and_signature_round_trip_separately() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;

    // The measured case DESIGN calls out: the signature is *longer* than the
    // thinking text, and a design that concatenates them cannot tell them apart.
    let thinking = "ok";
    let signature = "69164b36-fbed-4709-adfd-d6871900f038";
    assert!(signature.len() > thinking.len());

    let digest = journal.blobs().put_str(thinking).unwrap();
    journal
        .append(
            agent(),
            EventBody::ModelResponse {
                id: "id-1".into(),
                stop_reason: "end_turn".into(),
                usage: TokenUsage::default(),
                blocks: vec![],
                thinking_blob: Some(digest.clone()),
                thinking_signature: Some(signature.to_owned()),
            },
        )
        .await
        .unwrap();

    let line = log_text(&journal);
    assert!(line.contains("thinking_blob"), "the text is referenced");
    assert!(line.contains("thinking_signature"), "the signature is a field");
    // The signature is on the event (it must survive replay verbatim, WIRE-FACTS
    // Quirk 1); the text is not, and neither field contains the other.
    assert!(line.contains(signature));
    assert!(!line.contains(&format!("\"thinking\":\"{thinking}\"")));
    assert_eq!(journal.blobs().get_str(&digest).unwrap(), thinking);
    assert!(!digest.contains(signature));
    assert!(!signature.contains(&digest));

    let EventBody::ModelResponse {
        thinking_blob,
        thinking_signature,
        ..
    } = &journal.events().await.unwrap()[0].body
    else {
        panic!("expected a model response");
    };
    assert_eq!(thinking_blob.as_deref(), Some(digest.as_str()));
    assert_eq!(thinking_signature.as_deref(), Some(signature));
}

/// The signature-only response: DESIGN notes thinking text can be empty with the
/// signature all that is left. Nothing may be lost.
#[tokio::test]
async fn a_signature_without_thinking_text_is_still_recorded() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;
    let signature = "0".repeat(36);

    journal
        .append(
            agent(),
            EventBody::ModelResponse {
                id: "id-2".into(),
                stop_reason: "tool_use".into(),
                usage: TokenUsage::default(),
                blocks: vec![],
                thinking_blob: None,
                thinking_signature: Some(signature.clone()),
            },
        )
        .await
        .unwrap();

    let events = journal.events().await.unwrap();
    let EventBody::ModelResponse {
        thinking_blob,
        thinking_signature,
        ..
    } = &events[0].body
    else {
        panic!("expected a model response");
    };
    assert_eq!(thinking_blob.as_deref(), None);
    assert_eq!(thinking_signature.as_deref(), Some(signature.as_str()));
    assert!(blob_files(journal.dir()).is_empty());
}

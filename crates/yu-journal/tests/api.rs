//! API conformance.
//!
//! Other crates (`yu-core`, `yu-server`, `yu-workflow`) are written against these
//! signatures, so the test spells every one of them out with explicit types. A
//! signature that drifts — an argument becoming `&str`, a return becoming
//! `Option` — fails to compile *here*, at the boundary, instead of breaking the
//! crates downstream.

mod common;

use std::path::{Path, PathBuf};

use common::{agent, tempdir};
use yu_journal::{
    BlobStore, EffectGuard, EffectLog, Event, EventBody, Journal, JournalError, JournalIndex,
    SessionId,
};
use yu_observe::{AgentId, TokenUsage};

#[tokio::test]
async fn events_and_the_journal_have_the_documented_shape() {
    let dir = tempdir();
    let root: PathBuf = dir.path().to_path_buf();

    let session: SessionId = SessionId::new("s");
    let session: SessionId = SessionId::new(session.as_str().to_owned());
    let _: &str = session.as_str();
    let _: String = session.to_string();
    assert_eq!(session, SessionId("s".to_owned()));

    let journal: Journal = Journal::open(root, session).await.unwrap();
    let session: &SessionId = journal.session();
    assert_eq!(session.to_string(), "s");
    let _: &Path = journal.dir();
    let _: &BlobStore = journal.blobs();
    let _: usize = journal.len();
    let _: bool = journal.is_empty();

    // Every body, constructed with the documented field types.
    let bodies = vec![
        EventBody::SessionStarted {
            charter: "charter".to_owned(),
            model: "claude-opus-5".to_owned(),
            quirks: "think-before-tool".to_owned(),
        },
        EventBody::UserMessage {
            text: "do the thing".to_owned(),
        },
        EventBody::ModelRequest {
            model: "claude-opus-5".to_owned(),
            messages: 12usize,
            digest: "0".repeat(64),
            est_input_tokens: Some(4096u64),
        },
        EventBody::ModelResponse {
            id: "4aeac4b3-5f44-4b94-bef3-cb721ea7bef0".to_owned(),
            stop_reason: "tool_use".to_owned(),
            // The accounting type crossed over from yu-observe, exactly as the
            // wire layer's `Usage::to_observed()` produces it.
            usage: yyusage(),
            blocks: vec!["text-block".to_owned()],
            thinking_blob: None,
            thinking_signature: Some("69164b36-fbed-4709-adfd-d6871900f038".to_owned()),
        },
        EventBody::ToolCall {
            tool_use_id: "call_00_4Ykgo3t3AxTqrrfx12mk4590".to_owned(),
            name: "read".to_owned(),
            input: serde_json::json!({"path": "src/lib.rs"}),
        },
        EventBody::ToolResult {
            tool_use_id: "call_00_4Ykgo3t3AxTqrrfx12mk4590".to_owned(),
            name: "read".to_owned(),
            kind: "file".to_owned(),
            bytes: 5u64,
            reproducible: true,
            addressable: true,
            inline: Some("hello".to_owned()),
            blob: None,
            reduction: None,
        },
        EventBody::Injected {
            label: "system_prompt".to_owned(),
            blob: journal.blobs().put_str("system").unwrap(),
        },
        EventBody::Note {
            text: "note".to_owned(),
        },
        EventBody::Effect {
            key: "charge:invoice-1".to_owned(),
            description: "charge the card".to_owned(),
        },
    ];
    for body in &bodies {
        let kind: &'static str = body.kind();
        assert!(!kind.is_empty());
    }

    for (i, body) in bodies.into_iter().enumerate() {
        let event: Event = journal.append(agent(), body).await.unwrap();
        let _: u64 = event.seq;
        let _: i64 = event.ts_ms;
        let _: SessionId = event.session.clone();
        let _: AgentId = event.agent.clone();
        let _: EventBody = event.body.clone();
        assert_eq!(event.seq, i as u64);
        assert!(!format!("{event:?}").is_empty(), "Event: Debug");
    }

    let events: Vec<Event> = journal.events().await.unwrap();
    let tail: Vec<Event> = journal.read_from(4).await.unwrap();
    assert_eq!(tail.len(), events.len() - 4);
    // PartialEq is part of the contract: callers compare what they re-render
    // against what was recorded.
    assert_eq!(journal.read_from(0).await.unwrap(), events);
    assert_ne!(tail, events);
}

#[tokio::test]
async fn the_blob_store_has_the_documented_shape() {
    let dir = tempdir();
    let blobs: BlobStore = BlobStore::open(dir.path().join("blobs")).unwrap();

    let digest: String = blobs.put(b"payload").unwrap();
    let bytes: Vec<u8> = blobs.get(&digest).unwrap();
    assert_eq!(bytes, b"payload");
    let present: bool = blobs.has(&digest);
    assert!(present);
    let size: Option<u64> = blobs.size(&digest);
    assert_eq!(size, Some(7));

    let text: String = blobs.put_str("payload").unwrap();
    assert_eq!(text, digest, "same bytes, same digest");
    let back: String = blobs.get_str(&text).unwrap();
    assert_eq!(back, "payload");

    let missing: String = "a".repeat(64);
    assert!(!blobs.has(&missing));
    assert_eq!(blobs.size(&missing), None);
    match blobs.get(&missing) {
        Err(JournalError::BlobMissing { digest }) => assert_eq!(digest, missing),
        other => panic!("expected BlobMissing, got {other:?}"),
    }
}

#[tokio::test]
async fn the_index_and_effect_api_have_the_documented_shape() {
    let dir = tempdir();
    let journal: Journal = Journal::open(dir.path(), SessionId::new("s"))
        .await
        .unwrap();
    journal
        .append(agent(), EventBody::Note { text: "x".into() })
        .await
        .unwrap();

    // `Journal::open` rebuilt this from the file; `build` is the same view for
    // anything that only has the path.
    let index: JournalIndex = JournalIndex::build(journal.path()).unwrap();
    let offset: Option<u64> = index.offset_of(0);
    assert_eq!(offset, Some(0));
    let count: usize = index.len();
    assert_eq!(count, 1);
    assert_eq!(index.offset_of(1), None);

    let mut log: EffectLog = EffectLog::new();
    let committed: bool = log.is_committed("key");
    assert!(!committed);
    log.commit("key");
    let keys: Vec<String> = log.committed();
    assert_eq!(keys, vec!["key".to_owned()]);

    // `begin` takes `&mut EffectLog` and an `impl Into<String>`, and the guard
    // borrows the log until `mark_done` consumes it.
    let guard: Option<EffectGuard<'_>> = EffectGuard::begin(&mut log, "fresh");
    let guard: EffectGuard<'_> = guard.expect("a fresh key must be beginnable");
    let key: &str = guard.key();
    assert_eq!(key, "fresh");
    guard.mark_done();

    // Committed above: the second attempt is refused, which is the whole point.
    assert!(EffectGuard::begin(&mut log, "key").is_none());
}

#[test]
fn errors_are_std_errors() {
    let err: JournalError = JournalError::PayloadStoredTwice;
    let _: &dyn std::error::Error = &err;
    assert!(!err.to_string().is_empty());
    let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(err);
    assert!(boxed.to_string().contains("inline"));
}

/// The accounting type as the wire layer hands it over.
fn yyusage() -> TokenUsage {
    yu_wire::Usage {
        input_tokens: 256,
        output_tokens: 128,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 2299,
        service_tier: None,
    }
    .to_observed()
}

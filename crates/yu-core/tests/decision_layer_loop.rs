//! The loops, driven end to end against a scripted endpoint.
//!
//! Three mechanisms here are load-bearing and had no test that could fail:
//! the nine span kinds (all emitted from live call sites, none asserted), the
//! assistant's own words reaching the record (`journal_response` writing them
//! to a blob, driven by nothing), and `DecisionLayer::run` itself — the loop the
//! acceptance criterion rests on.
//!
//! All of it rests on one piece of furniture: [`FakeEndpoint`], a loopback HTTP
//! server that answers a scripted sequence of replies. `WireClient` can be
//! pointed anywhere, and `stream()` accepts a complete non-streaming message
//! body as a fallback — which is what keeps the scripts small. A run with a
//! delegation asks in a fixed order (the decision layer, then the executor it
//! derived, then the decision layer again), so the script doubles as a readable
//! statement of the run being driven, and every request is recorded verbatim so
//! a test can assert what the loop actually sent.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use yu_core::agent::{Agent, AgentConfig};
use yu_core::decision::{DecisionConfig, DecisionLayer, DecisionOutcome, DECISION_PROMPT};
use yu_core::delegate::delegate_tool;
use yu_core::render::{render, Charter, Ledger, RenderInput};
use yu_journal::{EventBody, Journal, SessionId};
use yu_observe::{AgentId, SpanKind, Timeline};
use yu_tools::{ResultKind, Tool, ToolContext, ToolRegistry, ToolResult};
use yu_wire::client::{Auth, RetryPolicy, WireClient};
use yu_wire::types::{CustomTool, ToolDefinition};
use yu_wire::Quirks;

const TEST_MODEL: &str = "claude-opus-5";

/// A dummy credential: `WireClientBuilder` refuses to build without one, and
/// construction is not a network call, so nothing else about this value matters.
const TEST_KEY: &str = "sk-test-not-a-real-key";

// ---------------------------------------------------------------------------
// The scripted endpoint
// ---------------------------------------------------------------------------

/// One scripted answer.
enum Reply {
    /// A complete, non-streaming message body. `stream()` accepts one as a
    /// fallback, so a reply that only has to carry a `tool_use` needs no SSE
    /// framing at all.
    Json(Value),
    /// Ready-made SSE frames, for the one thing a non-streaming body cannot do:
    /// deliver a delta. `first_token` is measured on the delta path, so a run
    /// that has to emit it needs at least one genuinely streamed reply.
    Sse(String),
    /// A non-2xx answer, for the tests that script a failure.
    Status(u16, Value),
}

impl Reply {
    fn render(&self) -> Vec<u8> {
        let (status, content_type, body) = match self {
            Reply::Json(value) => (200, "application/json", value.to_string()),
            Reply::Sse(frames) => (200, "text/event-stream", frames.clone()),
            Reply::Status(code, value) => (*code, "application/json", value.to_string()),
        };
        let reason = if status == 200 { "OK" } else { "Error" };
        format!(
            "HTTP/1.1 {status} {reason}\r\n\
             Content-Type: {content_type}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
}

/// A loopback endpoint that answers each request with the next scripted reply.
///
/// The script is consumed in order, one reply per request, which is exactly what
/// a run with a delegation needs. A request beyond the script is answered with a
/// 500 rather than with silence: an implementation that asks more often than the
/// script says it should has to fail its assertions, not hang the suite.
struct FakeEndpoint {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    beyond_the_script: Arc<AtomicUsize>,
}

impl FakeEndpoint {
    fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let addr = listener.local_addr().expect("listener address");
        // Non-blocking accept with an idle deadline, so the thread ends on its
        // own even when the run under test stops early.
        listener
            .set_nonblocking(true)
            .expect("a non-blocking listener");

        let requests = Arc::new(Mutex::new(Vec::new()));
        let beyond_the_script = Arc::new(AtomicUsize::new(0));
        {
            let requests = Arc::clone(&requests);
            let beyond_the_script = Arc::clone(&beyond_the_script);
            let mut script: VecDeque<Reply> = script.into();
            std::thread::spawn(move || {
                const IDLE: Duration = Duration::from_secs(5);
                let mut last_request = Instant::now();
                while last_request.elapsed() < IDLE {
                    let (mut sock, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => return,
                    };
                    last_request = Instant::now();
                    // Blocking and bounded: data may not have arrived yet when
                    // the connection is accepted, and a read that gave up then
                    // would answer nothing. The timeout is what keeps a peer
                    // that connects without writing from holding this thread.
                    let _ = sock.set_nonblocking(false);
                    let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
                    let body = read_request_body(&mut sock);
                    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
                        continue;
                    };
                    requests.lock().expect("no panic in the endpoint").push(request);
                    let reply = script.pop_front().unwrap_or_else(|| {
                        beyond_the_script.fetch_add(1, Ordering::SeqCst);
                        Reply::Status(
                            500,
                            json!({"type": "error", "error": {
                                "type": "api_error",
                                "message": "the scripted endpoint has no reply left",
                            }}),
                        )
                    });
                    let _ = sock.write_all(&reply.render());
                }
            });
        }
        Self {
            addr,
            requests,
            beyond_the_script,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .expect("no panic in the endpoint")
            .clone()
    }

    /// How many requests arrived with no reply left to give them. A test that
    /// asserts this is zero is asserting the run asked exactly as often as the
    /// script says it should.
    fn beyond_the_script(&self) -> usize {
        self.beyond_the_script.load(Ordering::SeqCst)
    }

    fn to_the_decision_layer(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| system_text(request).contains("You are the decision layer"))
            .collect()
    }

    fn to_an_executor(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| system_text(request).contains("You are an executor"))
            .collect()
    }
}

/// Read the body of one complete HTTP/1.1 request: the head, then
/// `content-length` bytes.
fn read_request_body(sock: &mut TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(head_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
            let body_len = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if raw.len() >= head_end + 4 + body_len {
                return raw[head_end + 4..head_end + 4 + body_len].to_vec();
            }
        }
        match sock.read(&mut buf) {
            Ok(0) | Err(_) => return Vec::new(),
            Ok(read) => raw.extend_from_slice(&buf[..read]),
        }
    }
}

/// The text of a request's system blocks, joined.
fn system_text(request: &Value) -> String {
    request["system"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn message_count(request: &Value) -> usize {
    request["messages"].as_array().map_or(0, Vec::len)
}

/// Every text block carried in a request's messages, joined.
fn message_texts(request: &Value) -> Vec<String> {
    let Some(messages) = request["messages"].as_array() else {
        return Vec::new();
    };
    messages
        .iter()
        .map(|message| match &message["content"] {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => blocks.iter().map(block_text).collect::<Vec<_>>().join(""),
            _ => String::new(),
        })
        .collect()
}

/// The text a content block carries, whichever field its type keeps it in:
/// prose and thinking in `text`, a tool result in `content`.
fn block_text(block: &Value) -> String {
    if let Some(text) = block["text"].as_str() {
        return text.to_owned();
    }
    match &block["content"] {
        Value::String(text) => text.clone(),
        Value::Array(inner) => inner.iter().map(block_text).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

/// How many copies of the rendered view a request carries.
///
/// `run` builds each request from a fresh view, so this is exactly one — and the
/// count is the whole point of the fresh-view construction, because a layer that
/// appended this round's view to a running transcript would show more.
fn view_copies(request: &Value) -> usize {
    message_texts(request)
        .iter()
        .filter(|text| text.contains("## This round"))
        .count()
}

// ---------------------------------------------------------------------------
// Scripted replies, in the endpoint's own shapes
// ---------------------------------------------------------------------------

/// A turn that calls a tool.
///
/// The thinking block is not decoration. This endpoint refuses to have a
/// `tool_use` replayed without its thinking block — Quirk 1, measured — and the
/// client checks that before sending, so a scripted tool call without one fails
/// the run rather than the test's assertions.
fn tool_use_reply(id: &str, thinking: &str, call_id: &str, name: &str, input: Value) -> Value {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": TEST_MODEL,
        "content": [
            {"type": "thinking", "thinking": thinking, "signature": format!("sig-{id}")},
            {"type": "tool_use", "id": call_id, "name": name, "input": input},
        ],
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": usage(120, 30),
    })
}

/// A turn that answers in prose and ends.
fn text_reply(id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": TEST_MODEL,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": usage(120, 30),
    })
}

fn usage(input_tokens: u64, output_tokens: u64) -> Value {
    json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    })
}

/// A streamed turn: thinking, then text, then the end of the message.
fn sse_text(id: &str, thinking: &str, text: &str) -> String {
    let frames = [
        json!({"type": "message_start", "message": {"id": id, "type": "message",
            "role": "assistant", "model": TEST_MODEL, "content": [], "stop_reason": null,
            "usage": usage(120, 0)}}),
        json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "thinking_delta", "thinking": thinking}}),
        json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "signature_delta", "signature": format!("sig-{id}")}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1,
            "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 30}}),
        json!({"type": "message_stop"}),
    ];
    let mut out = String::new();
    for frame in &frames {
        let event = frame["type"].as_str().unwrap_or("");
        // The `event:` line is load-bearing: the stream sink's dispatch matches
        // on it, and a data-only frame would deliver its deltas to nothing.
        out.push_str(&format!("event: {event}\ndata: {frame}\n\n"));
    }
    out
}

// ---------------------------------------------------------------------------
// The run under test
// ---------------------------------------------------------------------------

/// A tool with no side effects and no dependence on the machine.
///
/// The run has to execute a real tool — that is where `tool_exec` and `reduce`
/// are emitted — and nothing about the test should depend on the host it runs
/// on.
struct Probe;

impl Tool for Probe {
    fn name(&self) -> &str {
        "probe"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::Custom(CustomTool::new(
            self.name(),
            "Return a fixed payload.",
            json!({"type": "object", "properties": {"what": {"type": "string"}}}),
        ))
    }

    fn execute(&self, input: &Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::inline(self.name(), input.clone(), ResultKind::Structured, "probe: ok")
    }
}

fn registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(Probe));
    Arc::new(registry)
}

fn charter() -> Charter {
    Charter::from_the_humans_own_words("make the probe answer", 1_700_000_000_000, "test")
}

/// A scratch directory this test owns, re-created empty so that a leftover from
/// a failed run cannot be read as part of a passing one. `tempfile` is not a
/// dev-dependency of this crate, and this is the pattern its unit tests use.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yu-core-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A client pointed at the scripted endpoint, sharing `timeline` so the wire
/// layer records its own spans.
fn client(endpoint: &FakeEndpoint, timeline: &Arc<Timeline>) -> Arc<WireClient> {
    Arc::new(
        WireClient::builder()
            .base_url(endpoint.url())
            .auth(Auth::ApiKey(TEST_KEY.to_owned()))
            // The strict, unmeasured profile: a `tool_use` turn has to replay its
            // thinking block. That is what the real endpoint requires, and it is
            // why the tool-calling replies above carry one.
            .quirks(Quirks::unknown())
            // One attempt: a script that runs out is a failure to report, not a
            // reason to sleep through a backoff.
            .retry(RetryPolicy {
                max_attempts: 1,
                base_delay_ms: 1,
                max_delay_ms: 1,
            })
            .timeline(timeline.clone())
            .default_model(TEST_MODEL)
            .build()
            .expect("building the client"),
    )
}

/// One decision-layer run, and what it recorded.
struct DecisionRun {
    outcome: DecisionOutcome,
    journal: Arc<Journal>,
    timeline: Arc<Timeline>,
    dir: PathBuf,
}

impl Drop for DecisionRun {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn decide(name: &str, endpoint: &FakeEndpoint, max_rounds: u32) -> DecisionRun {
    let dir = scratch(name);
    let timeline = Timeline::new();
    let journal = Arc::new(
        Journal::open(&dir, SessionId::new(name))
            .await
            .expect("opening the journal")
            // Without this the journal records no spans of its own, and `journal`
            // is one of the kinds the full run has to cover.
            .with_timeline(timeline.clone()),
    );
    let mut config = DecisionConfig::new(TEST_MODEL);
    config.max_rounds = max_rounds;
    let mut layer = DecisionLayer::new(
        client(endpoint, &timeline),
        journal.clone(),
        registry(),
        timeline.clone(),
        config,
        charter(),
        dir.clone(),
    );
    let outcome = layer.run().await.expect("the scripted run completes");
    DecisionRun {
        outcome,
        journal,
        timeline,
        dir,
    }
}

/// A run that delegates once, asks for a second delegation in the same round —
/// the ceiling answers it — and then decides.
///
/// The second attempt is deliberate: it is what makes the request shape visible,
/// because it produces a decision-layer request carrying one more pair of turns
/// with nothing executed in between.
fn two_delegation_round() -> Vec<Reply> {
    vec![
        Reply::Json(tool_use_reply(
            "msg-d1",
            "the probe is what should run",
            "call-d1",
            "delegate",
            json!({"task": "run the probe and report what it says"}),
        )),
        Reply::Json(text_reply("msg-x1", "REPORT: probe: ok")),
        Reply::Json(tool_use_reply(
            "msg-d2",
            "one more delegation before deciding",
            "call-d2",
            "delegate",
            json!({"task": "run the probe again"}),
        )),
        Reply::Json(text_reply("msg-d3", "decided: the probe answered ok")),
    ]
}

// ---------------------------------------------------------------------------
// Guard 1 — every span kind is emitted by a real run
// ---------------------------------------------------------------------------

/// **M1: "删掉任意一种 span 的 `timeline.start(...)` → 有测试失败".**
///
/// One run does everything at once — the decision layer renders and calls the
/// wire, delegates, the executor executes a tool and reduces its result, the
/// journal records it — and every kind in [`SpanKind::ALL`] has to appear in the
/// timeline. Delete any single call site and this test names what went missing.
#[tokio::test]
async fn a_full_run_records_every_span_kind_the_timeline_declares() {
    let endpoint = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-d1",
            "ask the executor to run the probe",
            "call-d1",
            "delegate",
            json!({"task": "run the probe and report what it says"}),
        )),
        Reply::Json(tool_use_reply(
            "msg-x1",
            "the probe is the only thing to run",
            "call-x1",
            "probe",
            json!({"what": "everything"}),
        )),
        // Streamed, so `first_token` is measurable: it is recorded from the
        // first content delta, and a non-streaming body has no deltas.
        Reply::Sse(sse_text("msg-x2", "the probe answered; report it", "REPORT: probe: ok")),
        Reply::Json(text_reply("msg-d2", "decided: the probe answered ok")),
    ]);

    let run = decide("spans", &endpoint, 4).await;

    let mut seen: Vec<SpanKind> = Vec::new();
    for span in run.timeline.spans() {
        if !seen.contains(&span.kind) {
            seen.push(span.kind);
        }
    }

    let mut missing = Vec::new();
    for kind in SpanKind::ALL {
        if seen.contains(&kind) || covered_outside_this_run(kind).is_some() {
            continue;
        }
        missing.push(format!("{} — expected from {}", kind.as_str(), emitted_by(kind)));
    }
    assert!(
        missing.is_empty(),
        "no span was recorded for:\n  {}",
        missing.join("\n  ")
    );
    // A kind claimed as covered by another test has to be one this run cannot
    // produce: if it starts appearing here, the claim is stale and the coverage
    // is being asserted twice by accident.
    for kind in SpanKind::ALL {
        if let Some(where_) = covered_outside_this_run(kind) {
            assert!(
                !seen.contains(&kind),
                "`{}` is emitted by this run, so the claim that it is covered by {where_} is stale",
                kind.as_str()
            );
        }
    }

    // The script is the run: four requests in the order the run makes them, and
    // nothing asked beyond it.
    assert_eq!(
        endpoint.beyond_the_script(),
        0,
        "the run asked for more replies than the script has"
    );
    assert_eq!(endpoint.to_the_decision_layer().len(), 2);
    assert_eq!(endpoint.to_an_executor().len(), 2);

    // And the spans are attributed and labelled, not merely present: a `spawn`
    // that was never attached to a delegation, or a `tool_exec` that named no
    // tool, would be a number nobody can act on.
    let spans = run.timeline.spans();
    // `assemble` is emitted from two call sites — the decision layer rendering
    // its view and the executor building a step's request — and a check that
    // only asked whether the kind appeared would not notice one of them going
    // away. Both layers have to be seen doing it.
    for agent in [AgentId::decision(), AgentId::new("executor")] {
        assert!(
            spans
                .iter()
                .any(|span| span.kind == SpanKind::Assemble && span.agent == agent),
            "no `assemble` span is attributed to `{agent}`"
        );
    }
    let spawn = spans
        .iter()
        .find(|span| span.kind == SpanKind::Spawn)
        .expect("the run delegated");
    assert_eq!(
        spawn.label.as_deref(),
        Some("delegate:false"),
        "the spawn span must name what was derived"
    );
    let tool_exec = spans
        .iter()
        .find(|span| span.kind == SpanKind::ToolExec)
        .expect("the executor ran a tool");
    assert_eq!(tool_exec.label.as_deref(), Some("probe"));
    assert_eq!(tool_exec.agent, AgentId::new("executor"));
    let reduce = spans
        .iter()
        .find(|span| span.kind == SpanKind::Reduce)
        .expect("the tool result was reduced");
    assert!(
        reduce.attrs.contains_key("path"),
        "the reduction span must record which path the result took: {:?}",
        reduce.attrs
    );
}

/// Where each kind must come from in the run above.
///
/// Written with no wildcard arm on purpose: a tenth `SpanKind` breaks the build
/// here until someone says which call site produces it, which is what stops a
/// kind from being added to the list, emitted by nobody, and asserted by nobody.
fn emitted_by(kind: SpanKind) -> &'static str {
    match kind {
        SpanKind::Init => "the CLI's boot sequence (yu-cli, guarded there)",
        SpanKind::Assemble => "decision.rs per round, agent.rs per step",
        SpanKind::WireWait => "client.rs, around each send",
        SpanKind::FirstToken => "client.rs, measured at the first content delta",
        SpanKind::Stream => "client.rs, around the stream loop",
        SpanKind::ToolExec => "agent.rs run_one",
        SpanKind::Spawn => "delegate.rs run_delegation",
        SpanKind::Reduce => "agent.rs run_one",
        SpanKind::Journal => "yu-journal log.rs append, with a timeline attached",
    }
}

/// The kinds a test that is *already running* cannot produce, and which other
/// tests therefore have to guard.
fn covered_outside_this_run(kind: SpanKind) -> Option<&'static str> {
    match kind {
        // `init` measures process start-up; a running test is past it. It is
        // emitted by the CLI's boot sequence, whose own test asserts it.
        SpanKind::Init => Some("yu-cli's boot sequence (`the_boot_sequence_records_an_init_span`)"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Guard 2 — the assistant's reply reaches the record
// ---------------------------------------------------------------------------

/// **M1: "把 `journal_response` 改回永远写 `None` → 有测试失败".**
///
/// The turn's actual product is its prose, and it is recorded as a blob
/// reference rather than inline. This drives the real loop once and reads the
/// words back out of the store through the digest on the event — which is the
/// only way to check that the reference resolves, and the property the decision
/// layer's "每轮最终答复" and the next generation's verifier both stand on.
#[tokio::test]
async fn the_assistants_reply_is_recoverable_from_the_record() {
    const REPLY: &str = "the parser fails because the config is read twice";
    let endpoint = FakeEndpoint::start(vec![Reply::Json(text_reply("msg-1", REPLY))]);

    let dir = scratch("reply");
    let timeline = Timeline::new();
    let journal = Arc::new(
        Journal::open(&dir, SessionId::new("reply"))
            .await
            .expect("opening the journal"),
    );
    let mut agent = Agent::new(
        client(&endpoint, &timeline),
        journal.clone(),
        registry(),
        timeline,
        AgentConfig::new(AgentId::new("executor"), TEST_MODEL),
        dir.clone(),
    );
    let outcome = agent.run("say what you found").await.expect("the scripted run completes");
    assert_eq!(outcome.text, REPLY, "the loop must keep the turn's own words");

    let events = journal.events().await.expect("reading the journal");
    let recorded: Vec<Option<String>> = events
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ModelResponse { text_blob, .. } => Some(text_blob.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(recorded.len(), 1, "one response is recorded");

    let digest = recorded[0]
        .as_deref()
        .expect("the reply must be recorded as a blob reference, not only its block names");
    assert!(
        journal.blobs().has(digest),
        "the recorded digest `{digest}` resolves to nothing"
    );
    assert_eq!(
        journal.blobs().get_str(digest).expect("reading the blob"),
        REPLY,
        "the blob must hold the turn's prose"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Guard 3 — `DecisionLayer::run` is a loop of freshly rendered views
// ---------------------------------------------------------------------------

/// **M1: "`DecisionLayer::run` 有测试".**
///
/// Two things the run has to get right, both visible from outside:
///
/// * the view is **rebuilt** for each request rather than appended to a running
///   transcript, so a request carries exactly one copy of it plus the turns that
///   round has taken — and nothing else;
/// * the measurement is **per request**: one composition per request, each the
///   size of that request's own context, not a running total and not the union
///   of the run. The union is what the number used to be, and it charged the
///   system prompt and the tool definitions once per round in the direction that
///   flattered the design.
///
/// The second delegation in the script is what makes the shape observable: the
/// ceiling answers it without running anything, so the request after it carries
/// one more pair of turns and no more.
///
/// Note what this test cannot reach: `run`'s outer round loop never iterates —
/// the inner loop has no `break`, so a run ends from inside its first round —
/// and therefore no test can yet observe a *second* round's view. See the
/// M1 report for that; the guard here is on the fresh view each request is built
/// from.
#[tokio::test]
async fn every_request_carries_a_freshly_rendered_view_and_its_own_measurement() {
    let endpoint = FakeEndpoint::start(two_delegation_round());
    let run = decide("rebuild", &endpoint, 4).await;

    let requests = endpoint.to_the_decision_layer();
    let counts: Vec<usize> = requests.iter().map(message_count).collect();
    assert_eq!(
        counts,
        vec![1, 3, 5],
        "each request is the freshly rendered view plus the turns taken since — \
         one view, then one assistant/user pair per delegation attempt"
    );
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(
            view_copies(request),
            1,
            "request {index} carries {} copies of the view: the view is being \
             appended to a transcript rather than rebuilt",
            view_copies(request)
        );
    }

    // The measurement is per request. The first context is the rendered view and
    // nothing else — which is also what says the meter is measuring the request
    // rather than a sum of everything sent so far.
    let fresh_view = {
        let charter = charter();
        let ledger = Ledger::default();
        render(RenderInput {
            prompt: DECISION_PROMPT,
            charter: &charter,
            ledger: &ledger,
            tools: &[delegate_tool()],
            report: None,
            evidence: &[],
            shape: &[],
            omitted: &[],
        })
    };
    assert_eq!(
        run.outcome.contexts.len(),
        requests.len(),
        "one composition per request: the criterion names a context, and a run has many"
    );
    assert_eq!(
        run.outcome.contexts[0].total_chars, fresh_view.composition.total_chars,
        "the first request carries the rendered view and nothing else"
    );
    assert!(
        run.outcome.contexts[0].total_chars > 0
            && run.outcome.contexts[0].total_chars < run.outcome.contexts[1].total_chars
            && run.outcome.contexts[1].total_chars < run.outcome.contexts[2].total_chars,
        "the series must be the run's growth, one request at a time: {:?}",
        run.outcome.contexts
    );
    // A running total would count the tool definitions once per request.
    assert!(
        run.outcome.contexts.iter().all(|context| {
            context.tool_definition_chars == fresh_view.composition.tool_definition_chars
        }),
        "the tool definitions occupy every request exactly once, so a series that \
         grew them is a running total rather than a per-request measurement: {:?}",
        run.outcome.contexts
    );
    assert_eq!(
        run.outcome.composition,
        run.outcome.contexts[2],
        "the run's headline number is the context it ended on"
    );

    // DESIGN: the decision layer's context holds no raw material — a delegation
    // returns a reduction, not the tool stream.
    assert_eq!(
        run.outcome.peak_raw_material_ratio(),
        Some(0.0),
        "nothing in this layer's contexts came straight off a tool: {:?}",
        run.outcome.contexts
    );

    // Every request the wire saw is in the record. A request that reached the
    // model without reaching L2 is a hole in the one artefact the design says is
    // complete.
    let recorded = run
        .journal
        .events()
        .await
        .expect("reading the journal")
        .iter()
        .filter(|event| matches!(event.body, EventBody::ModelRequest { .. }))
        .count();
    assert_eq!(recorded, endpoint.requests().len());

    // One delegation ran; the second attempt was answered by the ceiling.
    assert_eq!(run.outcome.delegations, 1);
    assert_eq!(
        endpoint.to_an_executor().len(),
        1,
        "the ceiling must answer a second delegation rather than run it"
    );
    assert_eq!(endpoint.beyond_the_script(), 0);
    assert!(
        message_texts(&requests[2])
            .iter()
            .any(|text| text.contains("delegation ceiling of 1 reached")),
        "the model has to be told why its second delegation did not run: {:?}",
        message_texts(&requests[2])
    );
}

// ---------------------------------------------------------------------------
// Guard 3, continued — dispatch is by name, and delegation is bounded
// ---------------------------------------------------------------------------

/// The ceiling is what keeps a round's context from growing with how much the
/// layer chose to read. Remove the check and the second delegation runs, which
/// shows up as a second executor request the script does not have.
#[tokio::test]
async fn a_second_delegation_in_a_round_is_answered_rather_than_run() {
    let endpoint = FakeEndpoint::start(two_delegation_round());
    let run = decide("ceiling", &endpoint, 4).await;

    assert_eq!(run.outcome.delegations, 1);
    assert_eq!(
        endpoint.to_an_executor().len(),
        1,
        "one executor, however many times the layer asks"
    );
    assert_eq!(endpoint.beyond_the_script(), 0);
}

/// DESIGN: "工具面:`delegate` … —— **不是** Read/Write/Bash". The name check is
/// what makes the surface real: the prompt names the tools this layer is meant
/// to grow (`decide`, `retire`), so a call to one of them whose JSON happens to
/// carry a `task` field used to be executed as a delegation.
#[tokio::test]
async fn a_tool_use_naming_another_tool_is_answered_rather_than_run_as_a_delegation() {
    let endpoint = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-d1",
            "retire the finished item",
            "call-d1",
            "retire",
            // The field `DelegateArgs` deserialises: a harness without the name
            // check would take this for a delegation and run it.
            json!({"task": "read the whole repository"}),
        )),
        Reply::Json(text_reply("msg-d2", "understood")),
    ]);

    let run = decide("wrong-tool", &endpoint, 4).await;

    assert_eq!(run.outcome.delegations, 0, "nothing was delegated");
    assert_eq!(
        endpoint.to_an_executor().len(),
        0,
        "a call to a tool this layer does not have must not spawn an executor"
    );
    assert_eq!(endpoint.beyond_the_script(), 0);
    let requests = endpoint.to_the_decision_layer();
    assert_eq!(requests.len(), 2, "the run still reaches a decision");
    assert!(
        message_texts(&requests[1])
            .iter()
            .any(|text| text.contains("there is no tool named `retire`")),
        "the model has to be told the call was not a delegation: {:?}",
        message_texts(&requests[1])
    );
}

// ---------------------------------------------------------------------------
// What the round ceiling does not do
// ---------------------------------------------------------------------------

/// **`max_rounds` bounds nothing, and this writes that fact down.**
///
/// A round converses until the model stops asking — the inner loop in
/// `DecisionLayer::run` has no `break` — so the outer `for` never advances past
/// its first iteration, and a completed run always reports `1 round(s)`.
/// Measured here: with `max_rounds = 1` the layer still makes four requests,
/// because nothing in the loop counts them.
///
/// This is a *characterization* test, not a guard on correct behaviour. It pins
/// the state this milestone inherits so that making rounds real has to be a
/// deliberate act: it fails the moment a round can end, and whoever makes that
/// happen must also decide what `max_rounds` means, because today
/// `yu decide --max-rounds` is a knob that does nothing.
///
/// It is in this file because of what the tail of `run` looks like without it:
/// `Err(LoopError::StepCeiling)` reads like a live guard and is reachable only
/// for `max_rounds == 0`. An arm that looks alive and is not is the exact class
/// of thing this milestone exists to find, and a comment saying so is not a test.
///
/// Rounds only differ once L1 is written between them, and nothing writes the
/// ledger during a run yet — which is why this is not fixed here.
#[tokio::test]
async fn the_round_ceiling_does_not_bound_a_run_that_keeps_asking() {
    let endpoint = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-r1",
            "first",
            "call-r1",
            "delegate",
            json!({"task": "one"}),
        )),
        // Consumed by the executor the first attempt derives, not by the layer:
        // the two converse over the same socket.
        Reply::Json(text_reply("msg-e1", "REPORT: one done")),
        Reply::Json(tool_use_reply(
            "msg-r2",
            "again",
            "call-r2",
            "delegate",
            json!({"task": "two"}),
        )),
        Reply::Json(tool_use_reply(
            "msg-r3",
            "again",
            "call-r3",
            "delegate",
            json!({"task": "three"}),
        )),
        Reply::Json(text_reply("msg-r4", "decided with what I have")),
    ]);
    let run = decide("round-ceiling", &endpoint, 1).await;

    assert_eq!(
        run.outcome.rounds, 1,
        "a completed run always reports the first round"
    );
    assert_eq!(
        run.outcome.delegations, 1,
        "the per-round delegation ceiling does hold — only the first ran"
    );
    assert_eq!(
        endpoint.to_an_executor().len(),
        1,
        "and the other two attempts never reached an executor"
    );
    assert_eq!(
        endpoint.to_the_decision_layer().len(),
        4,
        "`max_rounds = 1` permitted four requests, so it bounds nothing. If this \
         assertion now fails, a round can end — `max_rounds` has become real, and \
         this test plus the unreachable comment in `run` both need revisiting."
    );
}

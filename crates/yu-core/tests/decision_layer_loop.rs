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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use yu_core::agent::{Agent, AgentConfig};
use yu_core::decision::{
    DECISION_PROMPT, DecisionConfig, DecisionLayer, DecisionOutcome, Resumed, StoppedEarly,
};
use yu_core::delegate::{decision_surface, spawn_edges, SpawnEdge};
use yu_core::render::{render, Charter, Ledger, RenderInput};
use yu_core::topology::{TopologyKind, TopologySpec};
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
    decide_with(name, endpoint, max_rounds, charter()).await
}

/// The same run, on a charter the caller states.
///
/// M4 is about what a *declaration* does to a run, so the tests below have to be
/// able to change one and change nothing else.
async fn decide_with(
    name: &str,
    endpoint: &FakeEndpoint,
    max_rounds: u32,
    charter: Charter,
) -> DecisionRun {
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
        charter,
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

/// The names of the tools a request carries.
fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
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
    //
    // The executor's name is its address now: this run's only delegation is the
    // first agent the decision layer derived, so `executor#1`. Every executor
    // being called `executor` was correct while only one could exist, and became
    // a defect the moment a run could hold several — two runs under one name,
    // and every reader of the record scoping to a name that names them all.
    for agent in [AgentId::decision(), AgentId::new("executor#1")] {
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
    assert_eq!(tool_exec.agent, AgentId::new("executor#1"));
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
            tools: &decision_surface(&Default::default()),
            report: None,
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
///
/// The name used is one the layer famously does **not** have: DESIGN's tool
/// surface is "`delegate` / `decide` / `retire` / ... — **not** Read/Write/Bash",
/// and a layer that quietly acquired a file reader would be a different layer.
#[tokio::test]
async fn a_tool_use_naming_another_tool_is_answered_rather_than_run_as_a_delegation() {
    let endpoint = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-d1",
            "read the file myself",
            "call-d1",
            "read_file",
            // The field `DelegateArgs` deserialises: a harness without the name
            // check would take this for a delegation and run it. (And `decide`
            // and `retire` are real tools now, so the name has to be one that
            // genuinely is not on the surface.)
            json!({"task": "read the whole repository", "path": "check.py"}),
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
            .any(|text| text.contains("there is no tool named `read_file`")),
        "the model has to be told the call was not a delegation: {:?}",
        message_texts(&requests[1])
    );
}

// ---------------------------------------------------------------------------
// The round ceiling, which now bounds
// ---------------------------------------------------------------------------

/// A model that keeps asking is stopped, told why, and still hands back a number.
///
/// This replaces a characterization test. Until this milestone the run could not
/// leave its first round — the inner loop had no `break` — so `max_rounds` and
/// `yu decide --max-rounds` bounded nothing, and a layer that kept asking made as
/// many requests as it liked. A round now ends when the layer has failed to move
/// three turns running, which is DESIGN's own trigger ("The same failure twice.
/// Stop. Do not ask for a third correction").
///
/// The two halves both matter. Ending the run at the ceiling is *not* enough on
/// its own: `run` used to return `Err(LoopError::StepCeiling)` there, so the
/// caller lost the final text, the composition and the acceptance number — from
/// one of the two ways a long task actually ends.
#[tokio::test]
async fn a_loop_that_keeps_asking_is_stopped_at_the_round_ceiling_and_says_so() {
    let endpoint = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-a1",
            "first",
            "call-a1",
            "delegate",
            json!({"task": "one"}),
        )),
        Reply::Json(text_reply("msg-ae", "REPORT: one done")),
        // Three turns that cannot move. The first two are the mistake and its
        // correction; the third is the loop.
        Reply::Json(tool_use_reply(
            "msg-a2",
            "again",
            "call-a2",
            "delegate",
            json!({"task": "two"}),
        )),
        Reply::Json(tool_use_reply(
            "msg-a3",
            "again",
            "call-a3",
            "delegate",
            json!({"task": "three"}),
        )),
        Reply::Json(tool_use_reply(
            "msg-a4",
            "again",
            "call-a4",
            "delegate",
            json!({"task": "four"}),
        )),
    ]);
    let run = decide("round-ceiling", &endpoint, 1).await;

    let stopped = run
        .outcome
        .stopped_early
        .as_ref()
        .expect("a run that never decided must say so rather than return an error");
    // Typed, so a caller can act on the reason rather than read it; `describe`
    // is the sentence the same fact renders as for a human.
    assert_eq!(
        stopped,
        &StoppedEarly::RoundCeiling { ceiling: 1 },
        "the reason has to name what ran out"
    );
    assert!(
        stopped.describe().contains("round ceiling of 1"),
        "the sentence has to name it too: {}",
        stopped.describe()
    );
    assert_eq!(run.outcome.rounds, 1);
    assert_eq!(
        run.outcome.delegations, 1,
        "the per-round delegation ceiling held: only the first ran"
    );
    assert_eq!(endpoint.to_an_executor().len(), 1);
    assert_eq!(
        endpoint.to_the_decision_layer().len(),
        4,
        "one turn to delegate, then the three that could not move"
    );
    assert_eq!(
        endpoint.beyond_the_script(),
        0,
        "the ceiling stopped the run before the script ran dry — if this fails, \
         the loop is asking for more turns than a round is allowed"
    );
    // The floor of the report: a stopped run still measured itself. Before this,
    // `Err` meant no composition was returned at all.
    assert!(
        run.outcome.composition.total_chars > 0,
        "a run that ends at the ceiling still has a context worth reporting"
    );
    assert_eq!(run.outcome.contexts.len(), 4);
}

// ---------------------------------------------------------------------------
// The advertised surface is the offered surface
// ---------------------------------------------------------------------------

/// **M2: "prompt 广告的工具面 == 实际发出的工具面,且有测试断言这件事".**
///
/// A prompt that names a tool the layer does not have costs a turn every time
/// the model believes it — and the belief is reasonable, because the prompt is
/// the only description of itself the layer gets. The other direction is worse
/// and quieter: a tool sent in the request that the prompt never mentions is one
/// the model has to discover by accident.
///
/// Read out of the prompt rather than restated here, because a copy of the list
/// in the test would drift from the copy in the prompt exactly as the prompt
/// drifted from the request.
#[test]
fn the_prompt_advertises_exactly_the_tools_the_layer_is_offered() {
    let advertised = tools_named_in_the_prompt();
    let offered: std::collections::BTreeSet<String> = decision_surface(&Default::default())
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect();

    let advertised_set: std::collections::BTreeSet<String> = advertised.iter().cloned().collect();
    assert_eq!(
        advertised_set, offered,
        "the prompt and the request disagree about the tool surface.\n\
         advertised and not offered: {:?}\n\
         offered and not advertised: {:?}",
        advertised_set.difference(&offered).collect::<Vec<_>>(),
        offered.difference(&advertised_set).collect::<Vec<_>>(),
    );
    assert!(
        !offered.is_empty(),
        "a layer offered nothing cannot do anything"
    );
}

/// The tool names the prompt's own tool section lists.
///
/// Only the bulleted entries are read. The prompt also names tools it does *not*
/// offer — in the paragraph that says which specified tools are not built yet —
/// and treating those as advertised would make the check assert the opposite of
/// what it is for.
fn tools_named_in_the_prompt() -> Vec<String> {
    let section = DECISION_PROMPT
        .split("# Your tools")
        .nth(1)
        .expect("the prompt must have a `# Your tools` section, or nothing describes the surface");
    let mut names = Vec::new();
    for line in section.lines() {
        // The section ends at the next heading.
        if line.starts_with('#') {
            break;
        }
        if let Some(rest) = line.trim().strip_prefix("- `")
            && let Some((name, _)) = rest.split_once('`')
        {
            names.push(name.to_owned());
        }
    }
    names
}

#[test]
fn the_prompts_tool_section_is_readable_in_the_shape_this_guard_expects() {
    // The guard above parses prose. If the section is reformatted, this says so
    // instead of the guard silently finding nothing and passing on an empty set.
    let named = tools_named_in_the_prompt();
    assert!(
        named.contains(&"delegate".to_owned()),
        "the parser found {named:?} in the prompt's tool section — it says the \
         section is no longer shaped the way the guard reads it"
    );
}

// ---------------------------------------------------------------------------
// The executor's number is its context, not its context plus its answer
// ---------------------------------------------------------------------------

/// Two runs that differ only in the length of the final reply.
///
/// They sent byte-identical requests, so they carried byte-identical contexts,
/// and the reported composition has to say so. Measuring at the *end* of the run
/// instead — which is what it did — charges the agent for the assistant turn
/// that answered the last request: that turn is the model's output, it is never
/// sent, and in an executor it is a large share of the bytes, because the report
/// is the whole point of the turn.
///
/// A differential test rather than an absolute one because the absolute number
/// depends on how every block is counted, and re-deriving that here would be a
/// second implementation of the metric — free to drift, and drifting invisibly.
#[tokio::test]
async fn the_executors_reported_context_does_not_include_the_answer_it_never_sent() {
    async fn run_with_final_reply(reply: &str, name: &str) -> yu_observe::ContextComposition {
        let endpoint = FakeEndpoint::start(vec![Reply::Json(text_reply("msg-1", reply))]);
        let dir = scratch(name);
        let timeline = Timeline::new();
        let journal = Arc::new(
            Journal::open(&dir, SessionId::new(name))
                .await
                .expect("opening the journal"),
        );
        let mut agent = Agent::new(
            client(&endpoint, &timeline),
            journal,
            registry(),
            timeline,
            AgentConfig::new(AgentId::new("executor"), TEST_MODEL),
            dir.clone(),
        );
        let outcome = agent
            .run("say what you found")
            .await
            .expect("the scripted run completes");
        assert_eq!(outcome.text, reply, "the turn's words are the run's words");
        outcome.composition
    }

    let brief = run_with_final_reply("ok", "ctx-brief").await;
    let wordy = run_with_final_reply(&"x".repeat(4000), "ctx-wordy").await;

    assert_eq!(
        brief, wordy,
        "the same request reported two different contexts, so the number is \
         counting the answer rather than the context: brief {brief:?}, wordy {wordy:?}"
    );
    assert!(
        brief.total_chars > 0,
        "and the number is real — an empty context and an unmeasured one are \
         different findings, and this run measured something"
    );
}

// ---------------------------------------------------------------------------
// M2 — L1 is written, and what was ruled out stays ruled out
// ---------------------------------------------------------------------------

/// **M2: "跑一个真实任务,结束后 L1 里有这次运行的决策与理由 —— 是决策,不是报告."**
///
/// The distinction is the whole point. A report is what the executor found; a
/// decision is what the layer concluded and why, and it is the only kind of
/// thing that can be carried into the next round without being re-derived. Until
/// this milestone the layer had no way to write one, so L1 stayed empty and the
/// layer began every round with no memory of its own conclusions.
#[tokio::test]
async fn the_decisions_the_layer_makes_are_in_l1_when_the_run_ends() {
    let endpoint = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-c1",
            "the probe answered, so I can decide",
            "call-c1",
            "decide",
            json!({
                "what": "the config is read twice",
                "why": "the probe showed both reads in the trace",
                "verdict": "accepted",
            }),
        )),
        Reply::Json(text_reply("msg-c2", "cause identified")),
    ]);
    let run = decide("l1-written", &endpoint, 4).await;

    let decisions: Vec<(&str, &str)> = run
        .outcome
        .ledger
        .decisions
        .iter()
        .map(|d| (d.what.as_str(), d.why.as_str()))
        .collect();
    assert_eq!(
        decisions,
        vec![(
            "the config is read twice",
            "the probe showed both reads in the trace"
        )],
        "L1 must hold what the layer decided and why it decided it"
    );

    // And the decision is in the record, not only in memory: L1 lives in the
    // head of a process that ends.
    let notes: Vec<String> = run
        .journal
        .events()
        .await
        .expect("reading the journal")
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Note { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        notes.iter().any(|text| text.contains("the config is read twice")
            && text.contains("the probe showed both reads")),
        "the decision has to reach L2 as well, or a resumed run cannot rebuild \
         the state it was working from: {notes:?}"
    );
}

/// **M2: "否决过的方案不会在后续轮次被重新提出."**
///
/// The mechanism is not a filter on what the model may say — it is that the
/// rejection is *in front of* the layer on every subsequent turn, under a
/// heading that says what it is for. This drives a deliberately constructed
/// re-proposal: the layer rules something out, and the very next round's
/// executor proposes exactly that thing again. The next view has to carry the
/// rejection, or the layer has nothing to refuse it with but its own memory —
/// which DESIGN says it does not have, because the view is re-rendered.
#[tokio::test]
async fn a_ruled_out_proposal_is_in_front_of_the_layer_when_it_comes_back() {
    let endpoint = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-x1",
            "a regex would be unreadable here",
            "call-x1",
            "decide",
            json!({
                "what": "parse the config with a regex",
                "why": "unreadable, and the format is nested",
                "verdict": "rejected",
            }),
        )),
        // Round two: the same proposal comes back, from the executor this time.
        Reply::Json(tool_use_reply(
            "msg-x2",
            "asking what the config loader should look like",
            "call-x2",
            "delegate",
            json!({"task": "propose a way to parse the config"}),
        )),
        Reply::Json(text_reply(
            "msg-x3",
            "PROPOSAL: parse the config with a regex",
        )),
        Reply::Json(text_reply("msg-x4", "refused: that was already ruled out")),
    ]);
    let run = decide("reproposal", &endpoint, 4).await;

    assert_eq!(
        run.outcome.ledger.rejected.len(),
        1,
        "the rejection is on the ledger"
    );

    // Every request the layer made *after* ruling it out has to carry it.
    let after: Vec<Value> = endpoint
        .to_the_decision_layer()
        .into_iter()
        .filter(|request| {
            message_texts(request)
                .iter()
                .any(|text| text.contains("PROPOSAL: parse the config with a regex"))
        })
        .collect();
    assert!(
        !after.is_empty(),
        "the scripted re-proposal never reached the layer, so this test would \
         pass without testing anything"
    );
    for (index, request) in after.iter().enumerate() {
        let view = system_text(request);
        assert!(
            view.contains("parse the config with a regex"),
            "request {index} carries the re-proposal but not the rejection, so the \
             layer has to rely on remembering it: {view}"
        );
        assert!(
            view.contains("Already rejected"),
            "and it must be under the heading that says what to do about it, \
             rather than mixed in with open questions: {view}"
        );
    }
}

/// **M2: "连续 8 轮后视野不随轮数单调增长."**
///
/// The property is not "the view is small" — L1 is complete state, so it grows
/// when the layer decides things, and that growth is the ledger working. The
/// property is that **the view is a function of state, not of history**: eight
/// rounds in which nothing was written must render the same view as one.
///
/// The run below never moves — every turn names a tool this layer does not have —
/// so it reaches the round ceiling with an untouched ledger, and every round's
/// first request must be the same view. Requests with exactly one message are the
/// freshly rendered view; the others are the same round's conversation.
///
/// **What this does not cover, and why.** It cannot see the evidence section: the
/// test registry has one synthetic tool, and evidence is only produced by `exec`,
/// so nothing in this run generates any. A version of this test that restored
/// the old accumulating `build_view` still passed — measured, not assumed. The
/// evidence guard is `the_rounds_material_does_not_grow_with_the_runs_history`
/// and its neighbours in `decision.rs`, which drive `carry_for_this_round`
/// directly. This test guards the other half: that a round's view is rendered
/// from state rather than appended to, and that per-request costs are not
/// multiplied by the round count.
#[tokio::test]
async fn eight_rounds_that_decide_nothing_do_not_grow_the_view() {
    // Three barren turns end a round, so eight rounds is twenty-four turns.
    let script: Vec<Reply> = (0..24)
        .map(|i| {
            Reply::Json(tool_use_reply(
                &format!("msg-n{i}"),
                "reading it myself",
                &format!("call-n{i}"),
                "read_file",
                json!({"path": "check.py"}),
            ))
        })
        .collect();
    let endpoint = FakeEndpoint::start(script);
    let run = decide("no-growth", &endpoint, 8).await;

    assert_eq!(
        run.outcome.rounds, 8,
        "the run has to actually reach the eighth round"
    );
    assert!(
        run.outcome.stopped_early.is_some(),
        "and it never decided anything, which it must say"
    );
    assert!(
        run.outcome.ledger.decisions.is_empty() && run.outcome.ledger.rejected.is_empty(),
        "the ledger is untouched, so nothing in the growth below is state"
    );

    let requests = endpoint.to_the_decision_layer();
    let fresh: Vec<&yu_observe::ContextComposition> = requests
        .iter()
        .enumerate()
        .filter(|(_, request)| message_count(request) == 1)
        .map(|(index, _)| &run.outcome.contexts[index])
        .collect();

    assert_eq!(
        fresh.len(),
        8,
        "one freshly rendered view per round, and the run had eight rounds"
    );
    let first = fresh[0];
    assert!(first.total_chars > 0, "a rendered view is not empty");
    for (round, context) in fresh.iter().enumerate() {
        assert_eq!(
            context.total_chars, first.total_chars,
            "round {}'s view is {} chars against round 0's {}: the view is \
             accumulating rather than being rendered",
            round + 1,
            context.total_chars,
            first.total_chars
        );
        assert_eq!(
            context.tool_definition_chars, first.tool_definition_chars,
            "the tool definitions are charged once per request, not once per round"
        );
    }
}

// ---------------------------------------------------------------------------
// M4 — the executor is given its conditions, and the declaration governs the run
// ---------------------------------------------------------------------------

/// A constraint the tests below put in the charter.
const STANDING_RULE: &str = "never touch the release branch";

/// A run whose executor only reports: one delegation, and a decision.
fn one_delegation(task: &str) -> Vec<Reply> {
    vec![
        Reply::Json(tool_use_reply(
            "msg-d1",
            "hand the work over",
            "call-d1",
            "delegate",
            json!({ "task": task }),
        )),
        Reply::Json(text_reply("msg-x1", "REPORT: done")),
        Reply::Json(text_reply("msg-d2", "decided")),
    ]
}

/// **M4: "把 constraints 从 charter 里删掉,执行者的行为会变 —— 证明它真的被注入."**
///
/// What this proves, and what it does not. It does not prove that a real model
/// *obeys* a constraint: no scripted endpoint can, and a claim about model
/// compliance is not something a deterministic test can make. What it proves is
/// the mechanism DESIGN's third derived principle rests on — "约束靠注入,不靠记忆":
/// the standing conditions of the run reach the **executor**, in its system
/// block, because the harness puts them there; and removing them from the
/// charter removes them from the request.
///
/// The second half is what makes the first half mean anything. Without it, an
/// executor that happened not to violate a constraint looks identical to one
/// that was never told about it — which is precisely the difference between
/// injection and luck.
///
/// The constraint must *not* arrive in the per-delegation text either. A task is
/// per-delegation and a constraint is a standing condition, so a standing
/// condition carried in the task text is dropped on the delegation where nobody
/// restated it — and nothing in the record would show the loss.
#[tokio::test]
async fn the_charters_constraints_are_injected_into_the_executors_system_block() {
    async fn run_with(name: &str, constraints: Vec<String>) -> FakeEndpoint {
        let endpoint = FakeEndpoint::start(one_delegation("run the probe"));
        let mut charter = charter();
        charter.constraints = constraints;
        let run = decide_with(name, &endpoint, 4, charter).await;
        assert_eq!(run.outcome.delegations, 1, "one delegation ran");
        endpoint
    }

    let given = run_with("constraints-present", vec![STANDING_RULE.to_owned()]).await;
    let absent = run_with("constraints-absent", Vec::new()).await;

    let asked = given.to_an_executor();
    assert_eq!(asked.len(), 1);
    assert!(
        system_text(&asked[0]).contains(STANDING_RULE),
        "the executor was not given the constraint the charter states, so it could only \
         have obeyed it by luck: {}",
        system_text(&asked[0])
    );
    assert!(
        !message_texts(&asked[0])
            .iter()
            .any(|text| text.contains(STANDING_RULE)),
        "a standing condition in the per-delegation text is dropped the round nobody \
         restates it, which is the failure this injects to prevent: {:?}",
        message_texts(&asked[0])
    );

    let unconstrained = absent.to_an_executor();
    assert_eq!(unconstrained.len(), 1);
    assert!(
        !system_text(&unconstrained[0]).contains(STANDING_RULE),
        "removing the constraint from the charter has to remove it from the request, or \
         the first assertion above proves nothing"
    );
    assert_eq!(given.beyond_the_script(), 0);
    assert_eq!(absent.beyond_the_script(), 0);
}

/// **M4: "改一条拓扑声明,运行行为跟着变(不是只有渲染变)."**
///
/// The declaration is the only difference between the two runs below: same task,
/// same items, same script shape. `width = 2` runs two executors and `width = 4`
/// runs four, and the count is read off the endpoint — a request the model
/// answered — rather than off the rendered view. That is the difference between
/// a strategy and a comment.
///
/// Each derived agent is also given the conditions, which is the other half of
/// "每一层每一轮被给予约束": if injection stopped at the first level, a fan-out
/// would be a way to get work done outside the constraints.
#[tokio::test]
async fn the_declared_width_decides_how_many_executors_run() {
    const ITEMS: [&str; 4] = [
        "read module a",
        "read module b",
        "read module c",
        "read module d",
    ];

    async fn fan_out(name: &str, width: &str) -> FakeEndpoint {
        let mut script = vec![Reply::Json(tool_use_reply(
            "msg-d1",
            "read them in parallel",
            "call-d1",
            "delegate",
            json!({"task": "read every module", "items": ITEMS}),
        ))];
        for (index, item) in ITEMS.iter().enumerate().take(width.parse().unwrap()) {
            script.push(Reply::Json(text_reply(
                &format!("msg-x{index}"),
                &format!("REPORT: {item} read"),
            )));
        }
        script.push(Reply::Json(text_reply("msg-d2", "decided")));

        let endpoint = FakeEndpoint::start(script);
        let mut charter = charter();
        charter.constraints = vec![STANDING_RULE.to_owned()];
        charter.topology = Some(TopologySpec::new(TopologyKind::FanOut).param("width", width));
        let run = decide_with(name, &endpoint, 4, charter).await;
        assert_eq!(
            run.outcome.delegations, 1,
            "a fan-out is one delegation, however many agents it derives"
        );
        endpoint
    }

    let two = fan_out("width-2", "2").await;
    let four = fan_out("width-4", "4").await;

    assert_eq!(
        two.to_an_executor().len(),
        2,
        "the declaration said two, so two agents ran"
    );
    assert_eq!(
        four.to_an_executor().len(),
        4,
        "the declaration said four and one executor ran — the shape did not reach the run"
    );
    assert_eq!(two.beyond_the_script(), 0);
    assert_eq!(four.beyond_the_script(), 0);

    // Which items ran, and under what conditions: a fan-out that derived the
    // right number of agents with the wrong tasks would satisfy a count.
    let requests = four.to_an_executor();
    for (index, request) in requests.iter().enumerate() {
        let carried = message_texts(request).join("\n");
        assert!(
            carried.contains(ITEMS[index]),
            "derived agent {} was not given its item: {carried}",
            index + 1
        );
        assert!(
            system_text(request).contains(STANDING_RULE),
            "derived agent {} was not given the run's constraints, so injection stops at \
             the first level: {}",
            index + 1,
            system_text(request)
        );
    }

    // And all four reports come back. A parent that reduced only the last child
    // would satisfy every count above while losing three quarters of the work —
    // the failure the whole "parent reduces" half of the shape exists to avoid.
    let back = four
        .to_the_decision_layer()
        .last()
        .cloned()
        .expect("the decision layer was answered");
    let carried = message_texts(&back).join("\n");
    assert!(
        carried.contains("DELEGATION DERIVED 4 AGENTS"),
        "the reports did not come back as a derivation: {carried}"
    );
    for item in ITEMS {
        assert!(
            carried.contains(item),
            "`{item}` was read by nobody the decision layer can see: {carried}"
        );
    }
}

/// **M4: "追踪一次运行,能画出一棵深度 ≥ 3 的树."**
///
/// One run — the decision layer delegating once, and executors deriving executors
/// below that — leaves a record from which the tree can be drawn, edge by edge.
/// The edges come from the typed `delegated` events rather than from task text or
/// timestamps, so "血缘记录完整" is checkable: every node that ran has exactly one
/// parent, and the tree is as deep as the run went.
///
/// The script is the shape: each reply belongs to the agent at that level, in the
/// order the depth-first walk reaches it.
#[tokio::test]
async fn one_run_leaves_a_tree_three_delegations_deep() {
    let endpoint = FakeEndpoint::start(vec![
        // decision → executor#1
        Reply::Json(tool_use_reply(
            "msg-d1",
            "hand the whole thing over",
            "call-d1",
            "delegate",
            json!({"task": "split the work"}),
        )),
        // executor#1 → executor#1.1
        Reply::Json(tool_use_reply(
            "msg-e1",
            "more than one job here",
            "call-e1",
            "delegate",
            json!({"task": "split it once more"}),
        )),
        // executor#1.1 → executor#1.1.1, which is deep enough to just work
        Reply::Json(tool_use_reply(
            "msg-e2",
            "one piece left to hand out",
            "call-e2",
            "delegate",
            json!({"task": "do the last piece"}),
        )),
        Reply::Json(text_reply("msg-e3", "REPORT: the piece is done")),
        Reply::Json(text_reply("msg-e4", "REPORT: the split is done")),
        Reply::Json(text_reply("msg-e5", "REPORT: the work is done")),
        Reply::Json(text_reply("msg-d2", "decided: the work is done")),
    ]);
    let mut charter = charter();
    charter.constraints = vec![STANDING_RULE.to_owned()];
    let run = decide_with("tree", &endpoint, 4, charter).await;

    assert_eq!(
        endpoint.beyond_the_script(),
        0,
        "the run asked for more replies than the shape has"
    );

    // Draw the tree the record describes.
    let edges = spawn_edges(&run.journal.events().await.expect("reading the journal"));
    let tree = children_of(&edges);
    let depth = depth_from(&tree, "decision");
    assert!(
        depth >= 3,
        "the record draws a tree {depth} level(s) deep, not the three the run walked: {edges:#?}"
    );
    assert_eq!(
        endpoint.to_an_executor().len(),
        5,
        "three executors: the two that derived took a step to derive and one to answer, \
         and the deepest answered on its first"
    );

    assert_eq!(
        depth_from(&tree, "decision#wrong"),
        0,
        "a node that never ran has no subtree, or the drawing is inventing one"
    );
    let roots = edges
        .iter()
        .filter(|edge| edge.parent == AgentId::decision())
        .count();
    assert_eq!(roots, 1, "one delegation was made at the top: {edges:#?}");
    let mut seen = BTreeSet::new();
    for edge in &edges {
        assert!(
            seen.insert(edge.child.as_str().to_owned()),
            "`{}` has two parents, so this is not a tree: {edges:#?}",
            edge.child
        );
    }
    assert!(
        tree.contains_key("executor#1"),
        "the middle node is an executor that derived in its own run: {edges:#?}"
    );
    assert_eq!(
        edges.iter().map(|edge| edge.depth).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "each edge records the level it belongs to"
    );

    // The deepest agent is still given the run's conditions: the chain of
    // injection does not stop at the level the decision layer can see.
    let deepest = endpoint
        .to_an_executor()
        .into_iter()
        .find(|request| {
            message_texts(request)
                .iter()
                .any(|text| text.contains("do the last piece"))
        })
        .expect("the deepest agent was asked to do the last piece");
    assert!(
        system_text(&deepest).contains(STANDING_RULE),
        "the constraint did not survive two levels of derivation: {}",
        system_text(&deepest)
    );
}

/// **M4: "一个子 agent 能在自己的运行里再派 agent", and what stops it.**
///
/// The bound is [`yu_core::delegate::MAX_DELEGATION_DEPTH`]: the decision layer's
/// delegations are depth 1, and a run at the ceiling is not offered `delegate` at
/// all. A call to the name anyway is answered with the reason — an agent that
/// cannot derive should be told which wall it hit, not handed "no such tool",
/// because the two call for different next moves.
///
/// The count is the guard on the bound: three edges and six executor requests,
/// where an unbounded tree would have taken a fourth level and a seventh.
#[tokio::test]
async fn the_depth_ceiling_refuses_a_fourth_level_rather_than_running_it() {
    /// One agent's run: it derives, the derived run happens inside that step,
    /// and it answers once the report comes back. Nested, because that is the
    /// order the requests arrive in.
    fn node(index: usize, task: &str, inside: Vec<Reply>) -> Vec<Reply> {
        let mut out = vec![Reply::Json(tool_use_reply(
            &format!("msg-e{index}"),
            "this is more than one job",
            &format!("call-e{index}"),
            "delegate",
            json!({ "task": task }),
        ))];
        out.extend(inside);
        out.push(Reply::Json(text_reply(
            &format!("msg-f{index}"),
            "REPORT: done here",
        )));
        out
    }

    // The agent at the ceiling asks anyway. This is the call under test.
    let at_the_ceiling = vec![
        Reply::Json(tool_use_reply(
            "msg-e3",
            "hand off the last piece",
            "call-e3",
            "delegate",
            json!({"task": "the fourth level"}),
        )),
        Reply::Json(text_reply("msg-f3", "REPORT: I did it here")),
    ];
    let mut script = vec![Reply::Json(tool_use_reply(
        "msg-d1",
        "hand it over",
        "call-d1",
        "delegate",
        json!({"task": "the first level"}),
    ))];
    script.extend(node(
        1,
        "the second level",
        node(2, "the third level", at_the_ceiling),
    ));
    script.push(Reply::Json(text_reply("msg-d2", "decided")));

    let endpoint = FakeEndpoint::start(script);
    // This test reads the **record**, not the run's return value. With the
    // ceiling gone a fourth level starts and asks for replies this script does
    // not have, so the run would end in an endpoint error — a failure, but one
    // that names the endpoint rather than the defect. The tree is the assertion.
    let dir = scratch("depth-ceiling");
    let timeline = Timeline::new();
    let journal = Arc::new(
        Journal::open(&dir, SessionId::new("depth-ceiling"))
            .await
            .expect("opening the journal"),
    );
    let mut layer = DecisionLayer::new(
        client(&endpoint, &timeline),
        journal.clone(),
        registry(),
        timeline.clone(),
        DecisionConfig::new(TEST_MODEL),
        charter(),
        dir.clone(),
    );
    let _ = layer.run().await;

    let edges = spawn_edges(&journal.events().await.expect("reading the journal"));
    assert_eq!(
        edges.len(),
        3,
        "a fourth level ran despite the ceiling: {edges:#?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(edges.last().map(|edge| edge.depth), Some(3));
    assert_eq!(
        endpoint.to_an_executor().len(),
        6,
        "three agents, two steps each: the one at the ceiling answered rather than derived"
    );

    let asked = endpoint
        .to_an_executor()
        .into_iter()
        .find(|request| {
            message_texts(request)
                .iter()
                .any(|text| text.contains("there is no `delegate` here"))
        })
        .expect("the refusal has to reach the agent that asked");
    let refusal = message_texts(&asked)
        .into_iter()
        .find(|text| text.contains("there is no `delegate` here"))
        .expect("found by the filter above");
    assert!(
        refusal.contains(&format!("{} levels", yu_core::delegate::MAX_DELEGATION_DEPTH)),
        "the refusal has to name the bound it hit: {refusal}"
    );

    // And the surface does not offer what it will refuse: the agent at the
    // ceiling is not handed a `delegate` tool, so a model is not invited to
    // spend a step on a call that cannot run.
    let tools = tool_names(&asked);
    assert!(
        !tools.contains(&"delegate".to_owned()),
        "an agent at the ceiling is still offered `delegate`: {tools:?}"
    );
    let shallower = endpoint
        .to_an_executor()
        .into_iter()
        .find(|request| {
            message_texts(request)
                .iter()
                .any(|text| text.contains("the second level"))
        })
        .expect("the second-level agent ran");
    assert!(
        tool_names(&shallower).contains(&"delegate".to_owned()),
        "an agent with depth left is offered the tool it may use: {:?}",
        tool_names(&shallower)
    );
}

/// The children of each node, from the recorded edges.
fn children_of(edges: &[SpawnEdge]) -> BTreeMap<String, Vec<String>> {
    let mut tree: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for edge in edges {
        tree.entry(edge.parent.as_str().to_owned())
            .or_default()
            .push(edge.child.as_str().to_owned());
    }
    tree
}

/// How many edges there are from `node` down to the deepest leaf below it.
fn depth_from(tree: &BTreeMap<String, Vec<String>>, node: &str) -> usize {
    match tree.get(node) {
        None => 0,
        Some(children) => children
            .iter()
            .map(|child| 1 + depth_from(tree, child))
            .max()
            .unwrap_or(0),
    }
}

// ---------------------------------------------------------------------------
// Continuing a session
// ---------------------------------------------------------------------------

/// The events of one kind in a record, in order.
fn events_of(record: &str, kind: &str) -> Vec<Value> {
    record
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["body"]["event"] == kind)
        .collect()
}

/// **What it takes to continue a session, from the record.**
///
/// Run one delegates, decides two things — one of them a rejection — and then
/// runs out of rounds. Run two reopens the same record and continues it: the
/// ledger comes back (including the rejection, which is what stops a settled
/// question from being re-opened), the round numbering continues instead of
/// restarting, and the derivation numbering continues too, so no two runs in one
/// record share an address.
///
/// What each assertion fails on if the mechanism is reverted:
///
/// * the ledger assertions fail if `Resumed::read` skips the recorded calls;
/// * `rounds == 2` fails if the continuation starts its rounds at one again —
///   which is also the revert that would spend the session's budget twice;
/// * the child naming fails if the continuation's derivation counter restarts;
/// * the single `session_started` fails if the layer starts the session again.
#[tokio::test]
async fn a_continued_session_picks_up_its_ledger_rounds_and_numbering_from_the_record() {
    let name = "continued";
    let dir = scratch(name);

    // ---- run one: a delegation, two ledger writes, then the round ceiling
    let first = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-d1",
            "the probe is what should run",
            "call-d1",
            "delegate",
            json!({"task": "run the probe and report what it says"}),
        )),
        Reply::Json(tool_use_reply(
            "msg-x1",
            "run it once",
            "call-x1",
            "probe",
            json!({"what": "once"}),
        )),
        Reply::Json(text_reply("msg-x2", "REPORT: probe: ok")),
        // Two ledger writes in one turn: one decision and one rejection, which
        // is what the next run has to find in front of it.
        Reply::Json(json!({
            "id": "msg-d2",
            "type": "message",
            "role": "assistant",
            "model": TEST_MODEL,
            "content": [
                {"type": "thinking", "thinking": "record what this run settled",
                 "signature": "sig-msg-d2"},
                {"type": "tool_use", "id": "call-a1", "name": "decide",
                 "input": {"what": "keep the probe's output",
                           "why": "it answered with evidence",
                           "verdict": "accepted"}},
                {"type": "tool_use", "id": "call-a2", "name": "decide",
                 "input": {"what": "inline the raw probe output",
                           "why": "it is the raw material this view exists to exclude",
                           "verdict": "rejected"}}
            ],
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": usage(120, 30),
        })),
    ]);
    let outcome_one = {
        let timeline = Timeline::new();
        let journal = Arc::new(
            Journal::open(&dir, SessionId::new(name))
                .await
                .expect("opening the record")
                .with_timeline(timeline.clone()),
        );
        let mut config = DecisionConfig::new(TEST_MODEL);
        config.max_rounds = 1;
        // A constraint, so the run has L0 beyond its intent: the only thing
        // that can carry it across the restart is the recorded seed, and a
        // continuation that lost it would work without it.
        let mut charter = charter();
        charter.constraints = vec!["never touch the release branch".to_owned()];
        let mut layer = DecisionLayer::new(
            client(&first, &timeline),
            journal.clone(),
            registry(),
            timeline.clone(),
            config,
            charter,
            dir.clone(),
        );
        layer.run().await.expect("the first run completes")
    };
    assert_eq!(
        outcome_one.stopped_early,
        Some(StoppedEarly::RoundCeiling { ceiling: 1 }),
        "run one has to end at its ceiling for this test to be about a continuation"
    );
    assert_eq!(outcome_one.rounds, 1);
    assert_eq!(outcome_one.delegations, 1);
    assert_eq!(first.beyond_the_script(), 0);

    // ---- run two: the record reopened, read back, and continued
    let second = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-r1",
            "one more delegation",
            "call-d2",
            "delegate",
            json!({"task": "run the probe again"}),
        )),
        Reply::Json(text_reply("msg-r2", "REPORT: probe: ok")),
        Reply::Json(text_reply("msg-r3", "decided: the probe stands")),
    ]);
    let timeline = Timeline::new();
    let journal = Arc::new(
        Journal::open(&dir, SessionId::new(name))
            .await
            .expect("reopening the record")
            .with_timeline(timeline.clone()),
    );
    let resumed = Resumed::read(&journal, &AgentId::decision())
        .await
        .expect("reading the record back");
    assert_eq!(resumed.rounds_spent, Some(1));
    assert_eq!(resumed.ceiling, Some(1));
    assert_eq!(resumed.delegations, 1);
    assert_eq!(resumed.derived, 1);
    assert_eq!(resumed.ledger_calls, 2);

    let mut config = DecisionConfig::new(TEST_MODEL);
    // The CLI gives a continuation what is left of the session's ceiling; the
    // layer simply runs the rounds it is given.
    config.max_rounds = 2;
    let mut layer = DecisionLayer::new(
        client(&second, &timeline),
        journal.clone(),
        registry(),
        timeline.clone(),
        config,
        resumed.charter.clone(),
        dir.clone(),
    )
    .resume(resumed);
    let outcome_two = layer.run().await.expect("the continuation completes");

    // The session's totals, not the invocation's.
    assert_eq!(
        outcome_two.rounds, 2,
        "the continuation runs the session's second round, not its first"
    );
    assert_eq!(outcome_two.delegations, 2);
    assert_eq!(second.beyond_the_script(), 0);

    // **The state, in front of it.** The decision and the rejection the first
    // run recorded are in the view the continuation sent — the rejection under
    // the heading that rules it out.
    let request = second
        .to_the_decision_layer()
        .into_iter()
        .next()
        .expect("the continuation asked the layer something");
    let system = system_text(&request);
    assert!(
        system.contains("keep the probe's output"),
        "the recorded decision has to come back into L1: {system}"
    );
    // **L0 as well.** The constraint was never written into the record as
    // prose: it lived in the charter the first run was handed, and the seed
    // event is the only place a continuation can read it. Lose the seed and the
    // continuation silently works without the standing condition.
    assert!(
        system.contains("## Constraints") && system.contains("never touch the release branch"),
        "the constraints the session ran under have to come back with it: {system}"
    );
    assert!(
        system.contains("do not re-propose"),
        "the recorded rejection has to come back under its heading: {system}"
    );
    assert!(
        system.contains("inline the raw probe output"),
        "and it has to be the rejection's own words: {system}"
    );

    // **The record.** One session beginning, one round per round the session
    // actually ran, and the continuation's own work numbered after the work
    // that already happened.
    let record = std::fs::read_to_string(journal.path()).expect("the record is a file");
    assert_eq!(
        events_of(&record, "session_started").len(),
        1,
        "a continuation must not start the session a second time"
    );
    assert_eq!(events_of(&record, "session_seeded").len(), 1);
    let rounds: Vec<(u64, u64)> = events_of(&record, "round_started")
        .iter()
        .map(|event| {
            (
                event["body"]["round"].as_u64().unwrap(),
                event["body"]["ceiling"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rounds,
        vec![(1, 1), (2, 2)],
        "the continuation has to continue the session's round numbering"
    );
    let children: Vec<String> = events_of(&record, "delegated")
        .iter()
        .map(|event| event["body"]["child"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        children,
        vec!["executor#1".to_owned(), "executor#2".to_owned()],
        "two runs in one record must not share an address"
    );
    assert!(
        record.contains("resumed at seq"),
        "the boundary has to be an event in the record, not a silent append: {record}"
    );
    assert!(
        record.contains("delegation 2:"),
        "the delegation numbering has to continue: {record}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

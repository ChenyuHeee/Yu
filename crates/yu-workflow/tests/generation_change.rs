//! **M5 criterion 1, end to end: "换代后,新会话里没有丢失任何仍在生效的约束."**
//!
//! The criterion asks to verify this by whether *behaviour* changes after the
//! injection, not by comparing text, and this is the strongest deterministic
//! form of that available here. Two sessions are driven against a scripted
//! endpoint — the first one decides something, hands over, and the second one
//! starts from that handover — and the thing asserted is what the new session's
//! **executor is given**: the standing conditions in its system block, which are
//! what it works under.
//!
//! What this establishes: the constraint the dying session produced exists in
//! L2 (as the note its `decide` call wrote), the readers lifted it out with the
//! address it came from, the handover carried it into the new session's L0, and
//! `DecisionLayer` injected it into the executor's system block — and removing
//! the handover removes it from the request. In M4's words, this is a
//! **mechanism proof**: it does not claim that a live model *obeys* a constraint,
//! because no scripted endpoint can, and it fixes exactly which link in the chain
//! is being asserted rather than dressing a request assertion up as behaviour.
//! What it does establish is the difference between carrying a constraint and
//! remembering one, which is the whole of DESIGN's derived principle 3.
//!
//! The endpoint below is the furniture `yu-core`'s own integration test uses,
//! trimmed to what these assertions need: `yu-core`'s in-crate double is
//! `cfg(test)`-gated and counts requests rather than recording them, and the
//! thing under test here is the *content* of the executor's request.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use yu_core::decision::{DecisionConfig, DecisionLayer};
use yu_core::render::Charter;
use yu_journal::{Journal, SessionId};
use yu_observe::Timeline;
use yu_tools::ToolRegistry;
use yu_wire::client::{Auth, RetryPolicy, WireClient};
use yu_wire::Quirks;
use yu_workflow::handover::{
    HandoverError, ReaderAngle, ReaderModel, Trigger, change_generation, trigger_for,
};

const TEST_MODEL: &str = "claude-opus-5";
const TEST_KEY: &str = "sk-test-not-a-real-key";

/// What the dying session learns and writes down. It has to be long enough for
/// the verifier's importance floor to see it, and distinctive enough that the
/// assertion cannot pass on an accident.
const CARRIED_RULE: &str = "never hard-code the socket path; the launcher reads it once at start-up";

/// The phrase the first session records, and the reader recognises.
const RECORDED: &str = "the socket path is read once at start-up and must stay configurable";

// ---------------------------------------------------------------------------
// A scripted endpoint
// ---------------------------------------------------------------------------

/// A loopback endpoint that answers each request with the next scripted reply,
/// and records every request verbatim so a test can read what was sent.
struct FakeEndpoint {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl FakeEndpoint {
    fn start(script: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let addr = listener.local_addr().expect("listener address");
        listener
            .set_nonblocking(true)
            .expect("a non-blocking listener");

        let requests = Arc::new(Mutex::new(Vec::new()));
        {
            let requests = Arc::clone(&requests);
            let mut script: VecDeque<Value> = script.into();
            std::thread::spawn(move || {
                // An idle deadline, so the thread ends on its own however the
                // run under test finishes.
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
                    let _ = sock.set_nonblocking(false);
                    let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
                    let body = read_request_body(&mut sock);
                    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
                        continue;
                    };
                    requests.lock().expect("no panic in the endpoint").push(request);
                    // A request beyond the script gets a 500 rather than silence:
                    // a run that asks more often than the script says it should
                    // has to fail its assertions, not hang the suite.
                    let reply = script.pop_front().unwrap_or_else(|| {
                        json!({"type": "error", "error": {
                            "type": "api_error",
                            "message": "the scripted endpoint has no reply left",
                        }})
                    });
                    let _ = sock.write_all(&http_json(&reply.to_string()));
                }
            });
        }
        Self { addr, requests }
    }

    fn client(&self) -> Arc<WireClient> {
        Arc::new(
            WireClient::builder()
                .base_url(format!("http://{}", self.addr))
                .auth(Auth::ApiKey(TEST_KEY.to_owned()))
                .quirks(Quirks::unknown())
                .retry(RetryPolicy {
                    max_attempts: 1,
                    base_delay_ms: 1,
                    max_delay_ms: 1,
                })
                .default_model(TEST_MODEL)
                .build()
                .expect("building the client"),
        )
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("no panic in the endpoint").clone()
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

fn http_json(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

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

// ---------------------------------------------------------------------------
// Scripted turns
// ---------------------------------------------------------------------------

fn tool_use_reply(id: &str, call_id: &str, name: &str, input: Value) -> Value {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": TEST_MODEL,
        "content": [
            // The strict history rule: a `tool_use` cannot be replayed without
            // its thinking block, and the client checks before sending.
            {"type": "thinking", "thinking": "working", "signature": format!("sig-{id}")},
            {"type": "tool_use", "id": call_id, "name": name, "input": input},
        ],
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": usage(),
    })
}

fn text_reply(id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": TEST_MODEL,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": usage(),
    })
}

fn usage() -> Value {
    json!({
        "input_tokens": 120,
        "output_tokens": 30,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    })
}

/// One session of the decision layer, against a scripted endpoint.
async fn run_session(name: &str, endpoint: &FakeEndpoint, charter: Charter) -> (Charter, Arc<Journal>, DecisionLayer, yu_core::decision::DecisionOutcome) {
    let dir = scratch(name);
    let timeline = Timeline::new();
    let journal = Arc::new(
        Journal::open(&dir, SessionId::new(name))
            .await
            .expect("opening the journal"),
    );
    let tools = Arc::new(ToolRegistry::native(&dir));
    let mut config = DecisionConfig::new(TEST_MODEL);
    config.max_rounds = 4;
    let mut layer = DecisionLayer::new(
        endpoint.client(),
        journal.clone(),
        tools,
        timeline,
        config,
        charter.clone(),
        dir.clone(),
    );
    let outcome = layer.run().await.expect("the scripted session completes");
    (charter, journal, layer, outcome)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yu-workflow-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn charter_for(text: &str) -> Charter {
    Charter::from_the_humans_own_words(text, 1_700_000_000_000, "test")
}

// ---------------------------------------------------------------------------
// The readers, scripted
// ---------------------------------------------------------------------------

/// A reader that behaves the way the prompt asks: it finds the record line
/// carrying a marker, copies that line's address, and reports what it extracted
/// from it. Scripted because a reader's *judgement* is not testable; what is
/// testable, and what this exercises, is that the address it copies is one the
/// record answers and that the extraction reaches the handover anchored.
struct MarkerReader;

impl ReaderModel for MarkerReader {
    async fn read(&self, prompt: &str) -> Result<String, HandoverError> {
        if prompt.contains("You are the session that is ending") {
            // The dying session has nothing that was never written down: this
            // test is about what was written down and carried.
            return Ok("none".to_owned());
        }
        if !prompt.contains(ReaderAngle::ConstraintsAndInvariants.subject()) {
            return Ok(String::new());
        }
        for line in prompt.lines() {
            if line.contains(RECORDED)
                && let Some((address, _)) = line
                    .strip_prefix('[')
                    .and_then(|rest| rest.split_once("] "))
            {
                return Ok(format!("- [{address}] {CARRIED_RULE}"));
            }
        }
        Ok(String::new())
    }
}

// ---------------------------------------------------------------------------
// M5 criterion 1
// ---------------------------------------------------------------------------

/// **A constraint the dying session only ever wrote down still reaches the new
/// session's executors.**
///
/// Session one delegates, is told a fact about the launcher, records what it
/// learned with `decide` — and ends by deciding. That note is the only place the
/// constraint exists: it is in L2, not in any charter, and nothing in the new
/// session remembers it. The generation change lifts it out with the address it
/// came from, and the new session's L0 is built from the handover.
///
/// The differential is the point. Two runs of the *same* second session, with
/// the same script, differ only in whether the handover was applied — and the
/// request the executor is given differs by exactly the carried constraint. That
/// is what "the injection changes behaviour" can mean deterministically: the run
/// does something different because of what it was given.
#[tokio::test]
async fn a_constraint_only_the_dying_session_wrote_down_reaches_the_new_sessions_executor() {
    // ---- session one: the dying session
    let dying = FakeEndpoint::start(vec![
        tool_use_reply(
            "msg-a1",
            "call-a1",
            "delegate",
            json!({"task": "trace how the launcher reads its socket path"}),
        ),
        text_reply("msg-a2", "REPORT: the launcher reads its path once, at start-up"),
        tool_use_reply(
            "msg-a3",
            "call-a3",
            "decide",
            json!({
                "what": RECORDED,
                "why": "the executor traced the launcher, and a second read would change it",
                "verdict": "accepted",
            }),
        ),
        text_reply("msg-a4", "decided: the launcher's path is read once"),
    ]);
    let first = run_session("dying", &dying, charter_for("make the launcher robust")).await;
    let (_, journal, _, outcome) = first;
    assert_eq!(outcome.delegations, 1);
    assert_eq!(
        outcome.stopped_early, None,
        "the settled shape: a session that reached a decision"
    );

    // The trigger, decided from what the run says about itself.
    let trigger = trigger_for(&outcome, None).expect("a decided session hands over");
    assert_eq!(trigger, Trigger::NaturalCompletion);

    // ---- the change
    let change = change_generation(
        &MarkerReader,
        &journal,
        trigger,
        &charter_for("make the launcher robust"),
        &outcome.ledger,
        &ReaderAngle::ALL,
    )
    .await
    .expect("the flow runs over the session's record");

    assert!(
        change.handover.constraints.iter().any(|c| c == CARRIED_RULE),
        "the reader's extraction of the record's constraint did not reach the handover: \
         {:#?}",
        change.handover.constraints
    );
    assert_eq!(
        change.handover.anchors.len(),
        1,
        "and it has to come with the address it was lifted from: {:#?}",
        change.handover.anchors
    );

    // ---- the new session, from the handover's state
    let handover = change.handover.clone();
    let applied = {
        let seed = handover.new_session(&charter_for("finish the launcher work"));
        assert!(
            seed.charter.constraints.iter().any(|c| c == CARRIED_RULE),
            "the carried constraint has to be L0 of the new session, or it is a fact the \
             new session has to remember: {:#?}",
            seed.charter.constraints
        );
        seed
    };

    /// The new session's script: delegate once, hear back, decide.
    fn second_session() -> Vec<Value> {
        vec![
            tool_use_reply(
                "msg-b1",
                "call-b1",
                "delegate",
                json!({"task": "write the launcher change"}),
            ),
            text_reply("msg-b2", "REPORT: done"),
            text_reply("msg-b3", "decided"),
        ]
    }

    let carried = FakeEndpoint::start(second_session());
    let second = run_session("carried", &carried, applied.charter.clone()).await;
    assert_eq!(second.3.delegations, 1);

    // The new session's own view carries it first: L0 is rendered into every
    // request, and the constraint is L0.
    let first_request = carried
        .to_the_decision_layer()
        .into_iter()
        .next()
        .expect("the new session asked the layer something");
    assert!(
        system_text(&first_request).contains(CARRIED_RULE),
        "the carried constraint is not in the new session's own view, so the layer would \
         have to delegate work without knowing the condition on it: {}",
        system_text(&first_request)
    );

    let with_handover = carried.to_an_executor();
    assert_eq!(with_handover.len(), 1);
    assert!(
        system_text(&with_handover[0]).contains(CARRIED_RULE),
        "the new session's executor was not given the constraint the previous session \
         learned, so after the generation change it could only have obeyed it by luck: {}",
        system_text(&with_handover[0])
    );
    assert!(
        !message_texts(&with_handover[0])
            .iter()
            .any(|text| text.contains(CARRIED_RULE)),
        "a standing condition carried in the per-delegation text is dropped the delegation \
         nobody restates it — the failure injection exists to prevent: {:?}",
        message_texts(&with_handover[0])
    );

    // ---- the differential: the same session, without the handover
    let bare = FakeEndpoint::start(second_session());
    let plain = run_session("bare", &bare, charter_for("finish the launcher work")).await;
    assert_eq!(plain.3.delegations, 1);
    let without_handover = bare.to_an_executor();
    assert_eq!(without_handover.len(), 1);
    assert!(
        !system_text(&without_handover[0]).contains(CARRIED_RULE),
        "removing the handover has to remove the constraint from the request, or the first \
         assertion above proves nothing: {}",
        system_text(&without_handover[0])
    );

    // The two runs differ in the request the executor is given and in nothing
    // else — same task, same script, same tool surface.
    assert_eq!(
        message_texts(&with_handover[0]),
        message_texts(&without_handover[0]),
        "the runs were not otherwise identical, so the difference above is not the handover"
    );
}

/// The other half of the trigger, on a real run: a session whose window grew
/// hands over even though it never decided, and one that stayed quiet does not.
///
/// This is the backstop DESIGN describes, and the count is what makes it a
/// backstop rather than a trap: growth is measured from where the session
/// started, so a window that is merely large — the shape of a freshly seeded
/// one — does not fire. The test sets a threshold of one character, because
/// every rendered view is larger than the one before it, and checks the other
/// direction by giving the same run no threshold at all.
#[tokio::test]
async fn a_session_that_never_decided_hands_over_only_when_its_view_grew() {
    // Every turn names a tool this layer does not have: nothing moves, and the
    // run reaches its ceiling without a decision.
    let spin: Vec<Value> = (0..12)
        .map(|i| {
            tool_use_reply(
                &format!("msg-s{i}"),
                &format!("call-s{i}"),
                "read_file",
                json!({"path": "check.py"}),
            )
        })
        .collect();
    let endpoint = FakeEndpoint::start(spin);
    let (_, _, _, outcome) = run_session("never-decided", &endpoint, charter_for("churn")).await;
    assert!(
        outcome.stopped_early.is_some(),
        "the session has to actually stop without deciding for this to test anything"
    );

    assert_eq!(
        trigger_for(&outcome, Some(1)),
        Some(Trigger::Threshold {
            growth_since_last: outcome
                .peak()
                .expect("a run measured its contexts")
                .total_chars
                - outcome.contexts[0].total_chars,
            threshold: 1,
        }),
        "a session that grew past a one-character threshold has to fire"
    );
    assert_eq!(
        trigger_for(&outcome, None),
        None,
        "without a declared threshold a session that did not decide is not handed over, \
         and the caller is told so rather than given a default"
    );
}

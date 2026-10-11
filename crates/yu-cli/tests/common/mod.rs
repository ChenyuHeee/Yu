//! The furniture the CLI's integration tests stand on: a real binary, and a
//! scripted endpoint it can be pointed at.
//!
//! Every test in this directory runs `yu` as a **process** — `CARGO_BIN_EXE_yu`
//! is the binary `cargo test` just built — because the criteria these tests exist
//! for are about processes: one that is killed, one that is restarted, one whose
//! policy file was edited between two runs. A test that called the same functions
//! in-process would establish nothing about any of the three.
//!
//! There are no live-model credentials in this sandbox, so what the process talks
//! to is a loopback endpoint answering a scripted sequence of replies. That makes
//! the runs deterministic and it is why the reply shapes matter: the endpoint
//! refuses a `tool_use` replayed without its thinking block (a measured quirk), so
//! every scripted tool call carries one.

#![allow(dead_code)] // each test binary uses a subset of these

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub const TEST_MODEL: &str = "claude-opus-5";

/// One scripted answer.
pub enum Reply {
    /// A complete, non-streaming message body. `stream()` accepts one as a
    /// fallback, which is what keeps the scripts small.
    Json(Value),
}

impl Reply {
    fn render(&self) -> Vec<u8> {
        let Reply::Json(value) = self;
        let body = value.to_string();
        format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
}

/// A loopback endpoint that answers each request with the next scripted reply.
pub struct FakeEndpoint {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    beyond_the_script: Arc<AtomicUsize>,
}

impl FakeEndpoint {
    pub fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let addr = listener.local_addr().expect("listener address");
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
                const IDLE: Duration = Duration::from_secs(30);
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
                    let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
                    let body = read_request_body(&mut sock);
                    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
                        continue;
                    };
                    requests.lock().expect("no panic in the endpoint").push(request);
                    let reply = script.pop_front().unwrap_or_else(|| {
                        beyond_the_script.fetch_add(1, Ordering::SeqCst);
                        Reply::Json(json!({
                            "type": "error",
                            "error": {"type": "api_error",
                                      "message": "the scripted endpoint has no reply left"},
                        }))
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

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("no panic in the endpoint").clone()
    }

    /// How many requests arrived with no reply left. Zero means the run asked
    /// exactly as often as the script says it should.
    pub fn beyond_the_script(&self) -> usize {
        self.beyond_the_script.load(Ordering::SeqCst)
    }

    /// Every text block carried in a request's messages, joined.
    pub fn last_message_text(&self) -> String {
        let Some(request) = self.requests().pop() else {
            return String::new();
        };
        message_texts(&request).join("\n")
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

/// Every text block carried in a request's messages, joined.
pub fn message_texts(request: &Value) -> Vec<String> {
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

/// A turn that calls a tool, with the thinking block the endpoint insists on.
pub fn tool_use_reply(id: &str, call_id: &str, name: &str, input: Value) -> Value {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": TEST_MODEL,
        "content": [
            {"type": "thinking", "thinking": "one tool call", "signature": format!("sig-{id}")},
            {"type": "tool_use", "id": call_id, "name": name, "input": input},
        ],
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": usage(120, 30),
    })
}

pub fn text_reply(id: &str, text: &str) -> Value {
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

// ---------------------------------------------------------------------------
// Running the binary
// ---------------------------------------------------------------------------

/// The `yu` binary this test was built alongside.
pub fn yu() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_yu"))
}

/// A child process, and where its output went.
pub struct Ran {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// Whether the process was killed by this signal, rather than exiting.
    pub fn killed_by(&self, signal: i32) -> bool {
        use std::os::unix::process::ExitStatusExt;
        self.status.signal() == Some(signal)
    }

    pub fn ok(&self) -> &Self {
        assert!(
            self.status.success(),
            "the run failed: {}\n{}",
            self.status,
            self.stderr
        );
        self
    }
}

/// Run `yu` to completion against `endpoint`, in `cwd`, with `dir` as the state
/// directory. `YU_QUIRKS` is left alone, so the client uses its conservative
/// built-in profile — the one that insists a tool call replays its thinking
/// block, which the scripted replies above carry.
pub fn run_yu(endpoint: &FakeEndpoint, args: &[&str], cwd: &Path, dir: &Path) -> Ran {
    let out = Command::new(yu())
        .args(args)
        .current_dir(cwd)
        .env("ANTHROPIC_BASE_URL", endpoint.url())
        .env("ANTHROPIC_API_KEY", "sk-test-not-a-real-key")
        .env("ANTHROPIC_MODEL", TEST_MODEL)
        .env("YU_QUIRKS", "unknown")
        .env("YU_DIR", dir)
        .output()
        .expect("the yu binary must be runnable");
    Ran {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Spawn `yu` without waiting for it: for the tests that kill it.
pub fn spawn_yu(endpoint: &FakeEndpoint, args: &[&str], cwd: &Path) -> Child {
    Command::new(yu())
        .args(args)
        .current_dir(cwd)
        .env("ANTHROPIC_BASE_URL", endpoint.url())
        .env("ANTHROPIC_API_KEY", "sk-test-not-a-real-key")
        .env("ANTHROPIC_MODEL", TEST_MODEL)
        .env("YU_QUIRKS", "unknown")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the yu binary must be spawnable")
}

/// `kill -9`, by asking the system to do it rather than by writing the syscall
/// out. A test that wanted a signal would otherwise need `libc`, and this is the
/// same signal either way.
pub fn sigkill(child: &Child) {
    let status = Command::new("kill")
        .arg("-9")
        .arg(child.id().to_string())
        .status()
        .expect("kill must run");
    assert!(status.success(), "kill -9 {} failed", child.id());
}

/// Wait for `path` to exist, up to a deadline. Returns false on timeout rather
/// than panicking, so a caller can assert something more useful than "timed out".
pub fn wait_for_file(path: &Path, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// Wait until `text_of(path)` contains `needle`, up to a deadline.
pub fn wait_for_text(path: &Path, needle: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.contains(needle)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

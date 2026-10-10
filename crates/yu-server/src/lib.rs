//! Daemon and embedded dual mode.
//!
//! DESIGN: flower's largest architectural mistake was one subprocess per step.
//! The answer is Codex's shape — one protocol, three hosts:
//!
//! ```text
//! Embedded                     // zero overhead, same process
//!   | LocalDaemon { UnixSocket } // resident daemon
//!   | Remote
//! ```
//!
//! Sessions are owned by the daemon and reached over a Unix socket; embedding
//! in-process avoids the socket entirely. Crash recovery is a first-class
//! concern (`thread_recovery`), not an afterthought. Headless goes through
//! `exec` with JSONL output.

pub mod session;

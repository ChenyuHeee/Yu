//! Parallel admission gate.
//!
//! DESIGN: "并行准入闸(抄 Codex `tools/parallel.rs`):每个工具声明
//! `supports_parallel`,调度时查一次,`tokio::spawn` 包 `AbortOnDropHandle`,
//! 一把 `Arc<RwLock<()>>` 当闸门 —— 支持并行的并发跑,不支持的串行".
//!
//! The correctness property is the interesting part, and it is easy to get wrong
//! by making only the parallel case a lock: a serial tool must exclude *both*
//! other serial tools **and** every parallel tool, or a write can interleave with
//! a read. Modelling admission as a read/write lock gets that for free, because
//! the read side is shared and the write side excludes everything.

use std::sync::Arc;

use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

/// Held while a tool runs.
///
/// Dropping it releases admission, so a cancelled or panicking tool cannot wedge
/// the gate — which matters because cancellation is normal here, not exceptional.
#[derive(Debug)]
pub enum Admission {
    /// Several may be held at once.
    Shared(OwnedRwLockReadGuard<()>),
    /// Held alone; excludes every other admission, shared or exclusive.
    Exclusive(OwnedRwLockWriteGuard<()>),
}

impl Admission {
    pub fn is_exclusive(&self) -> bool {
        matches!(self, Admission::Exclusive(_))
    }
}

#[derive(Debug, Clone, Default)]
pub struct AdmissionGate {
    gate: Arc<RwLock<()>>,
}

impl AdmissionGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for admission on behalf of one tool.
    ///
    /// The decision is made once, from the tool's declared `supports_parallel`,
    /// and never re-derived — a tool that changes its mind mid-flight would make
    /// the gate advisory rather than binding.
    pub async fn admit(&self, supports_parallel: bool) -> Admission {
        if supports_parallel {
            Admission::Shared(self.gate.clone().read_owned().await)
        } else {
            Admission::Exclusive(self.gate.clone().write_owned().await)
        }
    }
}

/// Declares whether a tool may run alongside others.
///
/// Kept as a plain field rather than a method on the tool trait so the
/// dispatcher can read it without the tool being constructed yet — dispatch
/// order is decided before anything is instantiated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parallelism {
    pub tool: String,
    pub supports_parallel: bool,
}

impl Parallelism {
    pub fn parallel(tool: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            supports_parallel: true,
        }
    }

    pub fn serial(tool: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            supports_parallel: false,
        }
    }
}

/// A task handle that aborts its task when dropped.
///
/// DESIGN names this from Codex's dispatcher. `tokio`'s own `JoinHandle`
/// *detaches* on drop rather than aborting, which is exactly the wrong default
/// here: abandoning a run would leave its tool calls executing — spending money
/// and mutating the workspace for a decision nobody is waiting on any more.
///
/// (`tokio_util::task::AbortOnDropHandle` is the same idea. This is written out
/// rather than pulled in so the crate does not need `tokio-util`'s `rt` feature
/// enabled for one twenty-line type.)
pub struct AbortOnDrop<T> {
    handle: Option<tokio::task::JoinHandle<T>>,
}

impl<T> AbortOnDrop<T> {
    pub fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    /// Abort now, without waiting for the drop.
    pub fn abort(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }

    pub fn abort_handle(&self) -> Option<tokio::task::AbortHandle> {
        self.handle.as_ref().map(|h| h.abort_handle())
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl<T> std::future::Future for AbortOnDrop<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        let handle = this
            .handle
            .as_mut()
            .expect("AbortOnDrop polled after it completed");
        std::pin::Pin::new(handle).poll(cx)
    }
}

/// Spawn a task that is aborted if its handle is dropped.
pub fn spawn_abortable<F>(future: F) -> AbortOnDrop<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    AbortOnDrop::new(tokio::spawn(future))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn parallel_admissions_are_held_concurrently() {
        let gate = AdmissionGate::new();
        let a = gate.admit(true).await;
        // Would deadlock if the shared side were exclusive.
        let b = tokio::time::timeout(Duration::from_millis(500), gate.admit(true))
            .await
            .expect("two parallel admissions must coexist");
        assert!(!a.is_exclusive());
        assert!(!b.is_exclusive());
    }

    #[tokio::test]
    async fn an_exclusive_admission_waits_for_parallel_ones_to_drain() {
        let gate = AdmissionGate::new();
        let held = gate.admit(true).await;
        let exclusive = tokio::time::timeout(Duration::from_millis(50), gate.admit(false)).await;
        assert!(
            exclusive.is_err(),
            "a serial tool must not start while a parallel one runs"
        );
        drop(held);
        tokio::time::timeout(Duration::from_millis(500), gate.admit(false))
            .await
            .expect("serial admission should proceed once the shared one is released");
    }

    #[tokio::test]
    async fn a_parallel_admission_waits_for_an_exclusive_one() {
        let gate = AdmissionGate::new();
        let held = gate.admit(false).await;
        let shared = tokio::time::timeout(Duration::from_millis(50), gate.admit(true)).await;
        assert!(
            shared.is_err(),
            "a parallel tool must not start while a serial one runs"
        );
        drop(held);
        tokio::time::timeout(Duration::from_millis(500), gate.admit(true))
            .await
            .expect("parallel admission should proceed once exclusive is released");
    }

    #[tokio::test]
    async fn serial_tools_do_not_overlap_each_other() {
        let gate = AdmissionGate::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let gate = gate.clone();
            let counter = counter.clone();
            handles.push(tokio::spawn(async move {
                let _admission = gate.admit(false).await;
                // If serial tools overlapped, this would observe a value > 1.
                let now = counter.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
                counter.fetch_sub(1, Ordering::SeqCst);
                now
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), 1, "two serial tools ran at once");
        }
    }

    #[tokio::test]
    async fn dropping_the_handle_aborts_the_task() {
        let gate = AdmissionGate::new();
        let flag = Arc::new(AtomicUsize::new(0));
        {
            let gate = gate.clone();
            let flag = flag.clone();
            let _handle = spawn_abortable(async move {
                let _admission = gate.admit(true).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
                flag.store(1, Ordering::SeqCst);
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
        } // handle dropped here
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            flag.load(Ordering::SeqCst),
            0,
            "an abandoned tool must not run to completion"
        );
    }

    #[tokio::test]
    async fn a_dropped_admission_releases_the_gate() {
        let gate = AdmissionGate::new();
        for _ in 0..8 {
            let admission = gate.admit(false).await;
            drop(admission);
        }
        tokio::time::timeout(Duration::from_millis(100), gate.admit(false))
            .await
            .expect("gate must not wedge after repeated acquire/release");
    }
}

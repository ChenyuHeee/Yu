//! Allocation attribution.
//!
//! DESIGN: "RSS + **按 agent 归因的分配量**(自定义全局分配器 —— Rust 的一处真实
//! 兑现)". This is one of the few places the language choice buys something a
//! scripted harness cannot have.
//!
//! ## What this measures, and what it does not
//!
//! It counts **cumulative bytes allocated**, attributed to whichever agent is
//! entered on the *allocating thread*. That is deliberately not live-heap size:
//! tracking live bytes would require carrying an owning slot on every pointer,
//! which means changing the allocation ABI.
//!
//! The honest limitation: a `tokio` task can migrate between threads, so work
//! that is entered around an `.await` may attribute to whichever thread the
//! continuation resumed on. Attribution is therefore exact for synchronous,
//! CPU-bound work — parsing, rendering, reduction, journal encoding, which is
//! most of what the harness itself does — and approximate across awaits.
//! Rather than pretend otherwise, `yu metrics` reports it as cumulative and
//! labels it best-effort.
//!
//! ## Cost when unused
//!
//! With no agent entered, the only work per allocation is two relaxed atomic
//! adds. `CURRENT_SLOT` is `const`-initialised, so reading it can never itself
//! allocate — no reentrancy hazard.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::span::AgentId;

const MAX_SLOTS: usize = 64;
const UNATTRIBUTED: u16 = 0;

static TOTAL_BYTES: AtomicU64 = AtomicU64::new(0);
static SLOT_BYTES: [AtomicU64; MAX_SLOTS] = [const { AtomicU64::new(0) }; MAX_SLOTS];

thread_local! {
    /// `const`-initialised on purpose: a lazily-initialised thread-local would
    /// allocate on first use *from inside the allocator*.
    static CURRENT_SLOT: Cell<u16> = const { Cell::new(UNATTRIBUTED) };
}

/// The global allocator. Install it in the binary:
///
/// ```ignore
/// #[global_allocator]
/// static ALLOC: yu_observe::Allocator = yu_observe::Allocator;
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct Allocator;

// SAFETY: every method forwards to `System` unchanged and only adds atomic
// counters. No pointer is derived, retained, or reinterpreted.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            Self::note(layout.size() as u64);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            Self::note(layout.size() as u64);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(ptr, layout, new_size) };
        if !out.is_null() && new_size > layout.size() {
            Self::note((new_size - layout.size()) as u64);
        }
        out
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

impl Allocator {
    #[inline]
    fn note(bytes: u64) {
        TOTAL_BYTES.fetch_add(bytes, Ordering::Relaxed);
        let slot = CURRENT_SLOT.with(Cell::get);
        if slot != UNATTRIBUTED {
            // Bounds are guaranteed by `register`, which only ever hands out
            // slots < MAX_SLOTS.
            //
            // Slot 0 is skipped deliberately: it is the "no agent entered" case,
            // which is the common path for anything not inside a scope, and
            // writing it would put a contended cache line on every allocation in
            // the process. `unattributed_bytes` recovers the value by
            // subtraction instead, which costs nothing here.
            SLOT_BYTES[slot as usize].fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// Total bytes allocated since process start, across all agents.
    pub fn total_bytes() -> u64 {
        TOTAL_BYTES.load(Ordering::Relaxed)
    }
}

fn registry() -> &'static Mutex<Vec<AgentId>> {
    static REGISTRY: OnceLock<Mutex<Vec<AgentId>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(vec![AgentId::new("<unattributed>")]))
}

/// Claim an attribution slot for `agent`, reusing an existing one if present.
///
/// Slots are finite (`MAX_SLOTS`). Beyond that, an agent's work is attributed to
/// `<unattributed>` rather than silently to somebody else.
pub fn register(agent: &AgentId) -> Option<u16> {
    let mut reg = registry().lock().expect("allocation registry poisoned");
    if let Some(i) = reg.iter().position(|a| a == agent) {
        return Some(i as u16);
    }
    if reg.len() >= MAX_SLOTS {
        return None;
    }
    reg.push(agent.clone());
    Some((reg.len() - 1) as u16)
}

/// Attribution scope. Enter it around synchronous work to charge that work's
/// allocations to an agent. Restores the previous agent on drop, so scopes nest.
pub struct AgentScope {
    previous: u16,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl AgentScope {
    pub fn enter(agent: &AgentId) -> Self {
        // Register first: registration may allocate (it grows the registry Vec),
        // and doing it before touching CURRENT_SLOT keeps those allocations from
        // being charged to whichever agent happened to be active.
        let slot = register(agent).unwrap_or(UNATTRIBUTED);
        let previous = CURRENT_SLOT.with(Cell::get);
        CURRENT_SLOT.with(|c| c.set(slot));
        Self {
            previous,
            _not_send: std::marker::PhantomData,
        }
    }

    /// Run `f` with allocations charged to `agent`.
    pub fn with<T>(agent: &AgentId, f: impl FnOnce() -> T) -> T {
        let _scope = Self::enter(agent);
        f()
    }
}

impl Drop for AgentScope {
    fn drop(&mut self) {
        CURRENT_SLOT.with(|c| c.set(self.previous));
    }
}

/// Cumulative bytes per agent. Best-effort: see the module docs.
pub fn snapshot() -> BTreeMap<String, u64> {
    let reg = registry().lock().expect("allocation registry poisoned");
    let mut out = BTreeMap::new();
    for (slot, agent) in reg.iter().enumerate() {
        let bytes = SLOT_BYTES[slot].load(Ordering::Relaxed);
        if bytes > 0 {
            out.insert(agent.as_str().to_owned(), bytes);
        }
    }
    out
}

/// Bytes allocated outside any agent scope.
///
/// Derived by subtraction rather than counted directly — see the note in
/// [`Allocator::note`]. Because the total and the per-slot counters are separate
/// atomics, a concurrent read can transiently see the slots ahead of the total,
/// so this saturates at zero rather than wrapping.
pub fn unattributed_bytes() -> u64 {
    let attributed: u64 = SLOT_BYTES[1..]
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .sum();
    TOTAL_BYTES.load(Ordering::Relaxed).saturating_sub(attributed)
}

// Attribution is tested in `tests/alloc_attribution.rs`, not here.
//
// It cannot be tested in this module: a `#[global_allocator]` is per-binary, and
// `cargo test -p yu-observe` builds the lib's own test harness, which installs
// none. Unit tests here would exercise `System` and measure nothing, while
// appearing to pass. The integration test declares the allocator itself.

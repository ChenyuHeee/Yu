//! Allocation attribution tests.
//!
//! These live in an integration test rather than a unit test because a
//! `#[global_allocator]` is per-binary. Under `cargo test -p yu-observe`, the
//! lib's unit-test harness installs no allocator, so any attribution assertion
//! there would exercise `System`, measure zero, and pass or fail for reasons
//! unrelated to the code. Declaring the allocator here is what makes the
//! measurement real.

use std::sync::{Mutex, MutexGuard, OnceLock};

use yu_observe::alloc::{AgentScope, Allocator, snapshot, unattributed_bytes};
use yu_observe::AgentId;

#[global_allocator]
static ALLOC: Allocator = Allocator;

/// Allocate without letting the optimiser remove it.
fn allocate(bytes: usize) {
    let v: Vec<u8> = vec![0u8; bytes];
    std::hint::black_box(&v);
}

/// Serialise these tests against each other.
///
/// Every test here reads process-global counters, and `unattributed_bytes()` is
/// derived by subtracting one atomic total from another (see the note on
/// `Allocator::note`). Two tests allocating concurrently can therefore observe
/// each other's half-updated counters, which makes the measurements flaky for
/// reasons that have nothing to do with the code under test. Taking this lock
/// keeps the globals still while a test is measuring them.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[test]
fn scope_charges_allocations_to_the_agent() {
    let _guard = serial();
    let agent = AgentId::new("alloc-scope-test");
    let before = snapshot().get("alloc-scope-test").copied().unwrap_or(0);
    AgentScope::with(&agent, || allocate(4096));
    let after = snapshot().get("alloc-scope-test").copied().unwrap_or(0);
    assert!(
        after >= before + 4096,
        "expected >= 4096 bytes charged to the agent, got {}",
        after - before
    );
}

#[test]
fn nested_scopes_restore_the_outer_agent() {
    let _guard = serial();
    let outer = AgentId::new("alloc-outer-test");
    let inner = AgentId::new("alloc-inner-test");

    AgentScope::with(&outer, || {
        let before_outer = snapshot().get("alloc-outer-test").copied().unwrap_or(0);
        let before_inner = snapshot().get("alloc-inner-test").copied().unwrap_or(0);

        AgentScope::with(&inner, || allocate(2048));
        let after_inner = snapshot().get("alloc-inner-test").copied().unwrap_or(0);
        assert!(
            after_inner >= before_inner + 2048,
            "inner scope should hold the inner agent's allocation"
        );

        allocate(2048);
        let after_outer = snapshot().get("alloc-outer-test").copied().unwrap_or(0);
        assert!(
            after_outer >= before_outer + 2048,
            "allocations after the inner scope must return to the outer agent"
        );
    });
}

#[test]
fn work_outside_any_scope_is_attributed_to_nobody_in_particular() {
    let _guard = serial();
    let before = unattributed_bytes();
    allocate(4096);
    let after = unattributed_bytes();
    assert!(
        after >= before + 4096,
        "unscoped work should land in <unattributed>, not on some agent"
    );
}

#[test]
fn the_allocator_still_actually_works() {
    let _guard = serial();
    // A forwarding allocator that broke allocation would be a spectacular
    // failure mode; prove it does not.
    let data: Vec<String> = (0..1000).map(|i| format!("value-{i}")).collect();
    assert_eq!(data.len(), 1000);
    assert_eq!(data[999], "value-999");
    assert!(Allocator::total_bytes() > 0);
}

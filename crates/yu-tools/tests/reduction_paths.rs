//! The reduction table, from outside the crate.
//!
//! DESIGN's claim is that all five paths live on one table and are chosen by
//! `kind × size × reproducible × task shape × current pressure`; every other
//! harness implements one of them and calls it a truncation policy. These tests
//! are the table's acceptance criteria, plus the three properties DESIGN claims
//! make it better:
//!
//! 1. the reader prompt is tiny;
//! 2. manifests do not lie about what was left out;
//! 3. read-back is closed against re-spilling.

use serde_json::json;

use yu_tools::reduce::{Budget, Pressure, Reducer, Reduction};
use yu_tools::result::{Manifest, Payload, Provenance, ResultKind, ToolResult};

const INLINE: u64 = 1_000;
const READER: u64 = 400;

fn budget() -> Budget {
    Budget::new(INLINE, READER)
}

fn reducer() -> Reducer {
    Reducer::new(budget())
}

fn synthetic(kind: ResultKind, size: u64) -> ToolResult {
    ToolResult {
        payload: Payload::Inline("x".repeat(size as usize)),
        kind,
        size,
        reproducible: false,
        addressable: false,
        provenance: Provenance::new("some_tool", json!({"path": "somewhere"})),
    }
}

// ---------------------------------------------------------------------------
// The five paths are all reachable from `choose`
// ---------------------------------------------------------------------------

#[test]
fn small_passes_through() {
    let result = synthetic(ResultKind::File, INLINE);
    assert_eq!(
        reducer().choose(&result, Pressure::low()),
        Reduction::PassThrough
    );
    let mut spill = |_: &[u8]| "never".to_owned();
    let applied = reducer().apply(result, &Reduction::PassThrough, &mut spill);
    assert!(applied.is_complete());
    assert_eq!(applied.omitted_bytes(), 0);
}

#[test]
fn large_reproducible_is_dropped_with_the_rerun_command() {
    let mut result = synthetic(ResultKind::File, 5_000);
    result.reproducible = true;
    result.addressable = true;
    result.provenance = Provenance::new("read_file", json!({"path": "huge.log"}));
    match reducer().choose(&result, Pressure::high()) {
        Reduction::DropWithManifest { rerun } => {
            assert_eq!(rerun.as_deref(), Some("read_file({\"path\":\"huge.log\"})"));
        }
        other => panic!("expected a drop, got {other:?}"),
    }

    // A command's own re-run key is the command, verbatim.
    let mut exec = synthetic(ResultKind::Diff, 5_000);
    exec.reproducible = true;
    exec.provenance = Provenance::new("exec", json!({"command": "cargo test --all"}));
    match reducer().choose(&exec, Pressure::high()) {
        Reduction::DropWithManifest { rerun } => {
            assert_eq!(rerun.as_deref(), Some("cargo test --all"));
        }
        other => panic!("expected a drop, got {other:?}"),
    }
}

#[test]
fn large_irreproducible_addressable_is_spilled_with_a_pointer() {
    let mut result = synthetic(ResultKind::File, 5_000);
    result.addressable = true;
    match reducer().choose(&result, Pressure::high()) {
        Reduction::SpillWithPointer { digest, pointer } => {
            assert!(digest.starts_with("fnv1a64:"), "{digest}");
            assert!(pointer.contains(&digest), "{pointer}");
            assert!(pointer.contains("5000"), "{pointer}");
            assert!(pointer.contains("Do not read"), "{pointer}");
        }
        other => panic!("expected a spill, got {other:?}"),
    }
}

#[test]
fn an_unbounded_shape_dispatches_a_shape_specialised_reader() {
    for kind in [ResultKind::Search, ResultKind::Log] {
        let result = synthetic(kind, 5_000);
        match reducer().choose(&result, Pressure::high()) {
            Reduction::DelegateReader { prompt, subagent } => {
                assert_eq!(subagent, format!("{}_reader", kind.as_str()));
                assert!(prompt.len() < 1200);
            }
            other => panic!("{kind:?} is unbounded; expected a reader, got {other:?}"),
        }
    }
}

#[test]
fn large_structured_goes_to_the_sandbox() {
    let result = synthetic(ResultKind::Structured, 5_000);
    assert_eq!(
        reducer().choose(&result, Pressure::high()),
        Reduction::SandboxReduce
    );
}

#[test]
fn pressure_is_one_of_the_inputs_the_path_is_chosen_by() {
    // An unbounded payload just over the floor: affordable while the window is
    // empty, delegated once it is not.
    let result = synthetic(ResultKind::Search, INLINE + 10);
    assert_eq!(
        reducer().choose(&result, Pressure::low()),
        Reduction::PassThrough
    );
    assert!(matches!(
        reducer().choose(&result, Pressure::high()),
        Reduction::DelegateReader { .. }
    ));
}

// ---------------------------------------------------------------------------
// Claim 3 — read-back is closed against re-spilling
// ---------------------------------------------------------------------------

#[test]
fn a_payload_read_back_from_a_spill_is_never_spilled_again() {
    for kind in ResultKind::ALL {
        for size in [INLINE + 1, READER * 10, 1_000_000] {
            for addressable in [true, false] {
                for reproducible in [true, false] {
                    for pressure in [Pressure::low(), Pressure::high()] {
                        let mut result = synthetic(kind, size);
                        result.addressable = addressable;
                        result.reproducible = reproducible;
                        result.provenance.from_spill = Some("fnv1a64:spilled-once".to_owned());

                        let reduction = reducer().choose(&result, pressure);
                        assert!(
                            !matches!(reduction, Reduction::SpillWithPointer { .. }),
                            "claim 3 violated for {kind:?} size={size} addressable={addressable} \
                             reproducible={reproducible} high={}: {reduction:?}",
                            pressure.is_high()
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn apply_refuses_to_write_a_second_copy_even_if_asked() {
    let mut result = synthetic(ResultKind::File, 5_000);
    result.addressable = true;
    result.payload = Payload::Spilled {
        digest: "fnv1a64:original".to_owned(),
        pointer: "original pointer".to_owned(),
        manifest: Manifest {
            omitted_bytes: 5_000,
            rerun: None,
            note: "already on disk".to_owned(),
            blob: Some("fnv1a64:original".to_owned()),
        },
    };
    let mut written = 0usize;
    let mut spill = |bytes: &[u8]| {
        written += bytes.len();
        "fnv1a64:second".to_owned()
    };
    let applied = reducer().apply(
        result,
        &Reduction::SpillWithPointer {
            digest: "fnv1a64:second".to_owned(),
            pointer: "second pointer".to_owned(),
        },
        &mut spill,
    );
    assert_eq!(written, 0);
    match applied.payload {
        Payload::Spilled { digest, .. } => assert_eq!(digest, "fnv1a64:original"),
        other => panic!("expected the original spill to stand, got {other:?}"),
    }
}

#[test]
fn the_self_loop_is_closed_end_to_end() {
    // A blob store that addresses bytes by digest, like the real one.
    let mut store: std::collections::BTreeMap<String, Vec<u8>> = std::collections::BTreeMap::new();
    let mut spills = 0usize;

    let mut log = synthetic(ResultKind::File, 5_000);
    log.addressable = true;
    let reduction = reducer().choose(&log, Pressure::high());
    let shown = {
        let mut spill = |bytes: &[u8]| {
            spills += 1;
            let digest = format!("blob:{}", bytes.len());
            store.insert(digest.clone(), bytes.to_vec());
            digest
        };
        reducer().apply(log, &reduction, &mut spill)
    };
    let (digest, pointer) = match &shown.payload {
        Payload::Spilled {
            digest, pointer, ..
        } => (digest.clone(), pointer.clone()),
        other => panic!("expected a spill, got {other:?}"),
    };
    assert_eq!(spills, 1);

    // Now read the blob back, as a reader agent would, and reduce the read
    // result. flower spilled this back out and looped; claim 3 forbids it.
    let read_back = ToolResult {
        payload: Payload::Inline(String::from_utf8(store[&digest].clone()).unwrap()),
        kind: ResultKind::File,
        size: store[&digest].len() as u64,
        reproducible: true,
        addressable: true,
        provenance: Provenance {
            tool: "read_blob".to_owned(),
            input: json!({"digest": digest}),
            from_spill: Some(digest.clone()),
        },
    };
    let second = reducer().choose(&read_back, Pressure::high());
    assert!(
        !matches!(second, Reduction::SpillWithPointer { .. }),
        "reading a spill must not produce another spill: {second:?}"
    );
    let shown_again = {
        let mut spill = |bytes: &[u8]| {
            spills += 1;
            format!("blob:{}", bytes.len())
        };
        reducer().apply(read_back, &second, &mut spill)
    };
    assert_eq!(spills, 1, "no second write");
    assert!(shown_again.render().contains(&digest));
    assert!(pointer.contains(&digest));
}

// ---------------------------------------------------------------------------
// Claim 2 — reductions are explicit, and manifests do not lie
// ---------------------------------------------------------------------------

#[test]
fn a_dropped_payload_names_what_is_missing_and_never_claims_completeness() {
    let mut result = synthetic(ResultKind::File, 40_000);
    result.reproducible = true;
    result.addressable = true;
    result.provenance = Provenance::new("read_file", json!({"path": "huge.rs"}));
    let reduction = reducer().choose(&result, Pressure::high());
    let mut spill = |_: &[u8]| "unused".to_owned();
    let shown = reducer().apply(result, &reduction, &mut spill);

    let text = shown.render();
    assert!(
        text.contains("40000"),
        "the byte count must be named: {text}"
    );
    assert!(text.contains("huge.rs"), "the re-run must be named: {text}");
    assert!(text.contains("read_file"), "{text}");
    assert!(text.contains("dropped"), "{text}");
    assert!(!shown.is_complete());
    assert_eq!(shown.omitted_bytes(), 40_000);
    assert_eq!(shown.present_bytes(), 0);
    assert!(
        !text.contains("complete"),
        "a reduction must not call itself complete: {text}"
    );
}

#[test]
fn a_spilled_payload_carries_the_pointer_and_the_delegation_instruction() {
    let mut result = synthetic(ResultKind::File, 9_000);
    result.addressable = true;
    let reduction = reducer().choose(&result, Pressure::high());
    let mut spill = |bytes: &[u8]| format!("store:{}", bytes.len());
    let shown = reducer().apply(result, &reduction, &mut spill);
    let text = shown.render();
    assert!(text.contains("store:9000"), "the pointer: {text}");
    assert!(text.contains("9000"), "{text}");
    assert!(
        text.contains("Delegate"),
        "the pointer must carry its instruction: {text}"
    );
    assert!(
        text.contains("Do not read the whole blob yourself"),
        "{text}"
    );
    assert!(!shown.is_complete());
}

#[test]
fn a_reduced_payload_says_it_is_a_reduction() {
    let result = synthetic(ResultKind::Search, 2_000);
    let shown = reducer().apply_reader_answer(result, "one hit in parser.rs");
    let text = shown.render();
    assert!(text.starts_with("one hit in parser.rs"), "{text}");
    assert!(text.contains("reduction"), "{text}");
    assert!(text.contains("2000"), "the omitted count: {text}");
    assert!(!shown.is_complete());
}

// ---------------------------------------------------------------------------
// is_complete / omitted_bytes agree with every payload variant
// ---------------------------------------------------------------------------

#[test]
fn accounting_is_consistent_with_each_payload_variant() {
    let inline = synthetic(ResultKind::File, 10);
    assert!(inline.is_complete());
    assert_eq!(inline.omitted_bytes(), 0);
    assert_eq!(inline.present_bytes(), 10);

    let mut spill = |bytes: &[u8]| format!("d:{}", bytes.len());

    let dropped = reducer().apply(
        synthetic(ResultKind::Log, 100),
        &Reduction::DropWithManifest { rerun: None },
        &mut spill,
    );
    assert!(!dropped.is_complete());
    assert_eq!(dropped.omitted_bytes(), 100);
    assert_eq!(dropped.present_bytes(), 0);
    assert_eq!(dropped.size, 100, "size is the payload the tool produced");

    let spilled = reducer().apply(
        synthetic(ResultKind::File, 100),
        &Reduction::SpillWithPointer {
            digest: "d:100".to_owned(),
            pointer: "p".to_owned(),
        },
        &mut spill,
    );
    assert!(!spilled.is_complete());
    assert_eq!(spilled.omitted_bytes(), 100);
    assert_eq!(spilled.present_bytes(), 0);

    let reduced = reducer().apply_reader_answer(synthetic(ResultKind::Log, 100), "short");
    assert!(!reduced.is_complete());
    assert_eq!(reduced.omitted_bytes(), 95);
    assert_eq!(reduced.present_bytes(), 5);
    assert_eq!(
        reduced.omitted_bytes() + reduced.present_bytes(),
        reduced.size,
        "a reduction accounts for every byte"
    );
}

#[test]
fn a_pass_through_leaves_the_payload_untouched() {
    let result = synthetic(ResultKind::Diff, 500);
    let before = result.clone();
    let mut spill = |_: &[u8]| "unused".to_owned();
    let after = reducer().apply(result, &Reduction::PassThrough, &mut spill);
    assert_eq!(after, before);
}

// ---------------------------------------------------------------------------
// The reduction is a value, so it can be journalled before anything happens
// ---------------------------------------------------------------------------

#[test]
fn a_reduction_round_trips_as_json() {
    let cases = [
        Reduction::PassThrough,
        Reduction::DropWithManifest {
            rerun: Some("cargo test".to_owned()),
        },
        Reduction::SpillWithPointer {
            digest: "d".to_owned(),
            pointer: "p".to_owned(),
        },
        Reduction::DelegateReader {
            prompt: "ask".to_owned(),
            subagent: "search_reader".to_owned(),
        },
        Reduction::SandboxReduce,
    ];
    for case in cases {
        let json = serde_json::to_string(&case).unwrap();
        let back: Reduction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, case);
    }
    let tag = serde_json::to_value(Reduction::SandboxReduce).unwrap();
    assert_eq!(tag["path"], json!("sandbox_reduce"));
}

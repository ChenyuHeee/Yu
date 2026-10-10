//! Claim 1: a specialised reader costs only the size of our own subagent prompt.
//!
//! Claude Code's Task tool pins that prompt at a fixed lower bound; a reader is
//! only a reader and does not need full tool discipline, so the prompt can be
//! generated from the shape — a sentence per kind — plus the caller's question.
//! If the prompt were a fixed blob, delegating would never be cheap enough to be
//! the default answer to an unbounded shape.

use yu_tools::reduce::{Budget, Pressure, Reducer, Reduction};
use yu_tools::result::{Payload, Provenance, ResultKind, ToolResult};

const CEILING: usize = 1200;

#[test]
fn the_prompt_is_under_1200_characters_for_every_kind() {
    for kind in ResultKind::ALL {
        let prompt = Reducer::reader_prompt(kind, "what is the first failure?");
        assert!(
            prompt.len() < CEILING,
            "{} reader prompt is {} characters",
            kind.as_str(),
            prompt.len()
        );
    }
}

#[test]
fn no_question_a_caller_can_pass_inflates_the_prompt() {
    // A caller pasting a whole file into the question must not turn the reader
    // into an expensive subagent.
    let questions = [
        "short".to_owned(),
        "q".repeat(5_000),
        "\u{4f60}\u{597d}".repeat(3_000),
        String::new(),
    ];
    for kind in ResultKind::ALL {
        for question in &questions {
            let prompt = Reducer::reader_prompt(kind, question);
            assert!(
                prompt.len() < CEILING,
                "{} with a {}-char question produced {} characters",
                kind.as_str(),
                question.len(),
                prompt.len()
            );
        }
    }
}

#[test]
fn the_prompt_names_the_shape_and_keeps_the_question() {
    for kind in ResultKind::ALL {
        let prompt = Reducer::reader_prompt(kind, "which file holds the parser?");
        assert!(prompt.contains(kind.as_str()));
        assert!(
            prompt.contains("which file holds the parser?"),
            "the question is the point of the prompt: {prompt}"
        );
        assert!(prompt.contains("reader"));
    }
}

#[test]
fn every_shape_gets_its_own_instructions() {
    let prompts: Vec<String> = ResultKind::ALL
        .iter()
        .map(|kind| Reducer::reader_prompt(*kind, "same question"))
        .collect();
    for (i, a) in prompts.iter().enumerate() {
        for (j, b) in prompts.iter().enumerate() {
            if i != j {
                assert_ne!(a, b, "a generic summariser is exactly what DESIGN rejects");
            }
        }
    }
}

#[test]
fn choose_generates_the_prompt_rather_than_embedding_a_constant() {
    let make = |tool: &str, input: serde_json::Value, kind: ResultKind| ToolResult {
        payload: Payload::Inline("x".repeat(20_000)),
        kind,
        size: 20_000,
        reproducible: false,
        addressable: false,
        provenance: Provenance::new(tool, input),
    };

    let search = Reducer::new(Budget::default()).choose(
        &make(
            "grep",
            serde_json::json!({"pattern": "fn main"}),
            ResultKind::Search,
        ),
        Pressure::high(),
    );
    let other = Reducer::new(Budget::default()).choose(
        &make(
            "grep",
            serde_json::json!({"pattern": "unrelated"}),
            ResultKind::Search,
        ),
        Pressure::high(),
    );

    match (search, other) {
        (
            Reduction::DelegateReader { prompt: a, .. },
            Reduction::DelegateReader { prompt: b, .. },
        ) => {
            assert!(
                a.contains("fn main"),
                "the generated question uses provenance: {a}"
            );
            assert_ne!(
                a, b,
                "two different results cannot produce one fixed prompt"
            );
        }
        other => panic!("expected two readers, got {other:?}"),
    }
}

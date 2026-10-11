//! Constraint 0: Rust's iteration-speed tax must be paid off by the architecture.
//!
//! > **prompt / 工具描述 / 角色 / 拓扑 / 归约规则 / hook / 预算 —— 一律外部资源。**
//!
//! Two tiers:
//!
//! * tier 1 — declarative data (TOML/JSON);
//! * tier 2 — embedded scripts, for policy that contains judgement. Rhai is the
//!   DESIGN recommendation (pure Rust, sandboxed by default, no extra deps);
//!   `wasmtime` if stronger isolation is ever needed.
//!
//! Acceptance criterion, to be honoured literally: **changing one policy rule
//! must not require recompiling Rust.**

pub mod outcomes;
pub mod resource;

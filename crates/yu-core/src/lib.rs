//! Loop, agent lifecycle, rendering, topology, scheduling.
//!
//! DESIGN's three core operations live here:
//!
//! * `render()` — builds the view **and** the not-loaded manifest alongside it.
//!   The harness knows exactly what it left out because it built the view.
//! * `delegate_read(record, question)` — dispatch a fresh reader.
//! * `retire(item, reason)` — explicit retirement, auditable and reversible.
//!   Isomorphic to Codex's `new_context`: the agent may ask for it, rather than
//!   only waiting for a threshold.
//!
//! Layering rule (DESIGN): `core` defines traits; `yu-policy` injects
//! implementations. `core` must not depend on `yu-policy`.

pub mod agent;
pub mod gate;
pub mod render;
pub mod traits;

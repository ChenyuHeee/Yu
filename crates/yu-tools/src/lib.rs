//! Tools, and the typed tool result.
//!
//! DESIGN calls this the project's centre of gravity and its largest original
//! contribution:
//!
//! > **返回值不是一个字符串,是一个带生命周期的类型化对象。**
//!
//! Everyone else implements exactly one reduction path and calls it a truncation
//! policy. Here the path is chosen from `kind × size × reproducible × task shape ×
//! current pressure`:
//!
//! | case | path |
//! |---|---|
//! | small | pass through |
//! | large, reproducible | drop + manifest: what is missing, and the command to re-run |
//! | large, not reproducible, addressable | spill + instruction-carrying pointer, with read-back closed against re-spilling |
//! | unbounded shape (search/log/long thread) | dispatch a shape-specialised reader |
//! | large structured | sandbox, return the reduced result only |
//!
//! Three things DESIGN claims make this better than the alternatives, each of
//! which is a testable property of this crate:
//!
//! 1. A specialised reader costs only the size of *our own* subagent prompt.
//! 2. Every reduction is an explicit event. Manifests do not lie about what was
//!    left out.
//! 3. Read-back is closed against being re-spilled — the self-loop flower hit.

pub mod native;
pub mod profile;
pub mod reduce;
pub mod result;

pub use native::{
    Edit, EffectIntent, EffectVerdict, Exec, ListDir, ReadFile, Tool, ToolContext, ToolRegistry,
    WriteFile,
};
pub use profile::{ProfileEntry, ProfileError, ToolProfile};
pub use reduce::{Axes, Budget, Pressure, Reduction, ReductionPath, ReductionTable, Reducer};
pub use result::{Manifest, Payload, Provenance, ResultKind, ToolResult};

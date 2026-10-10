//! Generation change (换代), workbench, human interaction.
//!
//! DESIGN's diagnosis of flower's failure has two layers. The shallow one is the
//! author: the dying session writes its own handover, so it is simultaneously the
//! compressed object and the compressor — a conflict of interest — working under
//! pressure, and the only entity that cannot see its own gaps. The deep one is
//! the nature of the thing: flower's handover was a *substitute for memory*, so it
//! had to be lossless, and it could not be. **It should have been an index plus
//! state.**
//!
//! The flow:
//!
//! 1. The dying session does the one thing nothing else can: it lists **loose
//!    ends** — the things it knows that were never written down. Loose ends only;
//!    it does not organise.
//! 2. `k` fresh readers, from different angles, read the record plus those loose
//!    ends: decisions and reasons / constraints and invariants / loose ends /
//!    **negative evidence** / artifact index. Take the **union, not a vote** —
//!    redundancy helps *extraction*, not *judgement* — and different framings are
//!    naturally decorrelated.
//! 3. Assemble.
//! 4. A verifier checks back against the record for what is *in the record, is
//!    important, and is missing from the handover*.
//!
//! Triggering prefers natural completion (completion is the legitimate form of
//! contraction), with a threshold as backstop, and the agent may ask for one.

pub mod handover;

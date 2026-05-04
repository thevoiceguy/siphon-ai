//! Bidirectional audio tap on top of forge-engine.
//!
//! This is the Week-1 spike target (see `docs/DEV_PLAN.md` §3.4). The exact
//! `MediaTap` shape depends on what forge-engine exposes once we look at how
//! `forge-ai-stream` plugs in. CLAUDE.md §4.3 governs the audio hot path:
//! no allocations in the steady-state frame loop, no `unwrap`/`panic`, no
//! `std::sync::Mutex`, no blocking I/O.

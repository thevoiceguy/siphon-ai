# HEP / Homer Integration

**Status:** stub. See `DEV_PLAN.md` §3.5 and §11.4 for the architecture.

HEP emission is best-effort (CLAUDE.md §4.7). Sources:
- siphon-rs ships SIP messages via `sip-hep`.
- forge-media ships RTCP/QoS via `forge-hep`.
- SiphonAI ships application events / log breadcrumbs / CDR pointers.

All three use the `HepSink` trait from the `hep-rs` crate.

//! Bot-initiated hold / resume (0.7.2) — the controller-side context.
//!
//! Unlike the peer-initiated `hold`/`resume` *events* (a far end held
//! us — surfaced by the acceptor's `on_reinvite`, see PROTOCOL.md §3.3),
//! this is SiphonAI driving the re-INVITE as the **offerer** on behalf
//! of the WS server (`BridgeIn::Hold` / `Resume`). The controller owns
//! the drive; this struct carries everything it needs.

use std::path::PathBuf;

use crate::transfer::DialogControl;

/// Inputs for driving a bot-initiated hold or resume re-INVITE on this
/// call's leg. One per call, built at setup time.
///
/// `control` is the shared dialog/flow drive (also used by transfer),
/// so hold inherits the TCP/TLS connection-reuse the inbound leg needs.
/// `hold_offer_sdp` / `resume_offer_sdp` are precomputed from the cached
/// local answer SDP with the media direction flipped (`a=sendonly` to
/// hold, `a=sendrecv` to resume) via
/// [`siphon_ai_media_glue::rewrite_sdp_direction`] — mid-call codec /
/// port renegotiation is out of scope, so reusing the original media
/// lines (port, codec, rtpmap, crypto) verbatim is exactly right.
#[derive(Clone, Debug)]
pub struct HoldContext {
    pub control: DialogControl,
    /// The re-INVITE offer that puts the caller on hold (`a=sendonly`).
    pub hold_offer_sdp: String,
    /// The re-INVITE offer that resumes two-way audio (`a=sendrecv`).
    pub resume_offer_sdp: String,
    /// Hold-music file (`[media].moh_file`). `None` → generated comfort
    /// silence. Shared with park's MOH; a fresh `MohSource` is built per
    /// hold so it always starts at the top of the file.
    pub moh_file: Option<PathBuf>,
}

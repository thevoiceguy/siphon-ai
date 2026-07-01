//! Audit-event schema.
//!
//! These payloads are a published, security-sensitive API: a SIEM
//! parses them to build a tamper-evident trail of *who did what* and
//! *what the daemon rejected*. Like the lifecycle-webhook schema
//! (CLAUDE.md §7.9) a new variant is additive — parsers tolerant to
//! unknown `type` values keep working — but renaming or changing an
//! existing field is a breaking change and bumps [`AUDIT_VERSION`].
//!
//! ## Naming
//!
//! - `version` is the schema version, per event, starting at 1.
//! - `type` is the on-the-wire discriminator (snake_case).
//! - `timestamp` is when the event was recorded (UTC).
//!
//! Events are deliberately coarse-grained: one variant per class of
//! security decision, not one per call site. That keeps the SIEM-side
//! correlation rules stable as internal code moves.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Version of this audit-event schema. Bump only on breaking changes
/// (rename / type change / removal of an existing field).
pub const AUDIT_VERSION: u32 = 1;

/// A single audit event. Serialised as a tagged JSON object — `type`
/// is the discriminator, the category fields hang off as siblings.
///
/// Every variant carries a `version` and a `timestamp`; the rest is
/// category-specific.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditEvent {
    /// An authenticated (or rejected) request to the `[admin]`
    /// listener. Covers the success, `401 unauthenticated`, and
    /// `403 forbidden` (RBAC denial) cases — the SIEM tells them apart
    /// via `result`.
    AdminRequest(AdminRequestEvent),

    /// The outcome of inbound SIP digest authentication (`[sip.auth]`)
    /// on an INVITE — a challenge issued, a bad/stale credential, or a
    /// success.
    SipAuth(SipAuthEvent),

    /// An inbound INVITE refused by admission control (`[sip.admission]`
    /// rate limit / flood shedding) or the `[[trunk]]` allowlist. The
    /// per-packet silent-drop path under a flood is intentionally
    /// *not* audited (see the daemon wiring) — only the onset of
    /// shedding is.
    InviteRejected(InviteRejectedEvent),

    /// An inbound INVITE refused because its STIR/SHAKEN attestation
    /// was below the configured policy floor, or an Identity header was
    /// required and absent.
    AttestationRejected(AttestationRejectedEvent),

    /// The outcome of a `SIGHUP` configuration reload — applied,
    /// unchanged, or rejected (kept the running config).
    ConfigReload(ConfigReloadEvent),

    /// A TLS certificate hot-reload on `SIGHUP` for the admin or SIP
    /// listener — success or failure (kept the previous cert).
    CertReload(CertReloadEvent),
}

impl AuditEvent {
    /// The `type` discriminator string — used by the `events`
    /// allowlist filter without reaching into `serde_json`.
    pub fn type_str(&self) -> &'static str {
        match self {
            AuditEvent::AdminRequest(_) => "admin_request",
            AuditEvent::SipAuth(_) => "sip_auth",
            AuditEvent::InviteRejected(_) => "invite_rejected",
            AuditEvent::AttestationRejected(_) => "attestation_rejected",
            AuditEvent::ConfigReload(_) => "config_reload",
            AuditEvent::CertReload(_) => "cert_reload",
        }
    }

    /// A short human subject for `tracing` correlation lines — the peer
    /// or actor the event is about. Never `None`; falls back to `"-"`.
    pub fn subject(&self) -> &str {
        match self {
            AuditEvent::AdminRequest(e) => e.actor.as_deref().unwrap_or(&e.peer),
            AuditEvent::SipAuth(e) => &e.peer,
            AuditEvent::InviteRejected(e) => &e.peer,
            AuditEvent::AttestationRejected(e) => e.from_tn.as_deref().unwrap_or("-"),
            AuditEvent::ConfigReload(_) => "config",
            AuditEvent::CertReload(e) => &e.component,
        }
    }
}

/// A request to the authenticated `[admin]` listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminRequestEvent {
    pub version: u32,
    pub timestamp: DateTime<Utc>,
    /// Source socket address of the client.
    pub peer: String,
    /// Token *name* (never the secret) that authenticated, or `None`
    /// when the request was unauthenticated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// The role the token carries (`readonly` / `operator` / `admin`),
    /// or `None` when unauthenticated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// HTTP method (`GET`, `POST`, ...).
    pub method: String,
    /// Matched route template (bounded cardinality — e.g.
    /// `/admin/v1/calls/:id`), not the raw path with ids inlined.
    pub endpoint: String,
    /// HTTP status returned.
    pub status: u16,
    /// `ok` | `unauthenticated` | `forbidden`.
    pub result: String,
    /// For `forbidden`: the role the endpoint required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_role: Option<String>,
}

/// The outcome of inbound SIP digest authentication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SipAuthEvent {
    pub version: u32,
    pub timestamp: DateTime<Utc>,
    /// Source socket address of the INVITE.
    pub peer: String,
    /// The `[sip.auth]` credential source (e.g. trunk name) the
    /// challenge is scoped to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub register_source: Option<String>,
    /// `ok` | `challenged` | `failed` | `stale`.
    pub result: String,
}

/// An inbound INVITE refused before it reached routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteRejectedEvent {
    pub version: u32,
    pub timestamp: DateTime<Utc>,
    /// Source socket address of the INVITE.
    pub peer: String,
    /// `rate_limited` (503) | `no_trunk` (403 allowlist) | `draining`
    /// (503 shutdown).
    pub result: String,
}

/// An inbound INVITE refused on STIR/SHAKEN grounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestationRejectedEvent {
    pub version: u32,
    pub timestamp: DateTime<Utc>,
    /// Originating telephone number from the PASSporT, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_tn: Option<String>,
    /// Configured minimum attestation the call failed to meet, e.g.
    /// `A` / `B` / `C`, or `identity_required`.
    pub required: String,
    /// The attestation the call actually presented, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attestation: Option<String>,
    /// SIP response code used to reject (403 / 428 / 438 / 606 ...).
    pub code: u16,
    /// Human reason phrase.
    pub reason: String,
}

/// The outcome of a `SIGHUP` config reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigReloadEvent {
    pub version: u32,
    pub timestamp: DateTime<Utc>,
    /// `applied` | `no_change` | `failed`.
    pub result: String,
    /// Sections that changed but need a full restart to take effect
    /// (empty when none / not applicable).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart_required: Vec<String>,
    /// Failure detail for `failed`, else `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A TLS cert hot-reload on `SIGHUP`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertReloadEvent {
    pub version: u32,
    pub timestamp: DateTime<Utc>,
    /// `admin_tls` | `sip_tls`.
    pub component: String,
    /// Certificate file path.
    pub cert_path: String,
    /// `ok` | `failed`.
    pub result: String,
    /// Failure detail for `failed`, else `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ─── Terse constructors ─────────────────────────────────────────────
//
// Call sites build events with these so `version` / `timestamp` are
// stamped in one place and the site stays a one-liner. `timestamp`
// defaults to `Utc::now()`.

impl AuditEvent {
    /// Build an [`AuditEvent::AdminRequest`].
    #[allow(clippy::too_many_arguments)]
    pub fn admin_request(
        peer: impl Into<String>,
        actor: Option<String>,
        role: Option<String>,
        method: impl Into<String>,
        endpoint: impl Into<String>,
        status: u16,
        result: impl Into<String>,
        required_role: Option<String>,
    ) -> Self {
        AuditEvent::AdminRequest(AdminRequestEvent {
            version: AUDIT_VERSION,
            timestamp: Utc::now(),
            peer: peer.into(),
            actor,
            role,
            method: method.into(),
            endpoint: endpoint.into(),
            status,
            result: result.into(),
            required_role,
        })
    }

    /// Build an [`AuditEvent::SipAuth`].
    pub fn sip_auth(
        peer: impl Into<String>,
        register_source: Option<String>,
        result: impl Into<String>,
    ) -> Self {
        AuditEvent::SipAuth(SipAuthEvent {
            version: AUDIT_VERSION,
            timestamp: Utc::now(),
            peer: peer.into(),
            register_source,
            result: result.into(),
        })
    }

    /// Build an [`AuditEvent::InviteRejected`].
    pub fn invite_rejected(peer: impl Into<String>, result: impl Into<String>) -> Self {
        AuditEvent::InviteRejected(InviteRejectedEvent {
            version: AUDIT_VERSION,
            timestamp: Utc::now(),
            peer: peer.into(),
            result: result.into(),
        })
    }

    /// Build an [`AuditEvent::AttestationRejected`].
    pub fn attestation_rejected(
        from_tn: Option<String>,
        required: impl Into<String>,
        attestation: Option<String>,
        code: u16,
        reason: impl Into<String>,
    ) -> Self {
        AuditEvent::AttestationRejected(AttestationRejectedEvent {
            version: AUDIT_VERSION,
            timestamp: Utc::now(),
            from_tn,
            required: required.into(),
            attestation,
            code,
            reason: reason.into(),
        })
    }

    /// Build an [`AuditEvent::ConfigReload`].
    pub fn config_reload(
        result: impl Into<String>,
        restart_required: Vec<String>,
        detail: Option<String>,
    ) -> Self {
        AuditEvent::ConfigReload(ConfigReloadEvent {
            version: AUDIT_VERSION,
            timestamp: Utc::now(),
            result: result.into(),
            restart_required,
            detail,
        })
    }

    /// Build an [`AuditEvent::CertReload`].
    pub fn cert_reload(
        component: impl Into<String>,
        cert_path: impl Into<String>,
        result: impl Into<String>,
        detail: Option<String>,
    ) -> Self {
        AuditEvent::CertReload(CertReloadEvent {
            version: AUDIT_VERSION,
            timestamp: Utc::now(),
            component: component.into(),
            cert_path: cert_path.into(),
            result: result.into(),
            detail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_request_serialises_with_type_tag() {
        let ev = AuditEvent::admin_request(
            "10.0.0.5:5555",
            Some("ops-token".into()),
            Some("operator".into()),
            "POST",
            "/admin/v1/calls",
            200,
            "ok",
            None,
        );
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "admin_request");
        assert_eq!(v["actor"], "ops-token");
        assert_eq!(v["version"], AUDIT_VERSION);
        // `required_role` is None → omitted.
        assert!(v.get("required_role").is_none());
    }

    #[test]
    fn forbidden_admin_request_carries_required_role() {
        let ev = AuditEvent::admin_request(
            "10.0.0.5:5555",
            Some("ro-token".into()),
            Some("readonly".into()),
            "POST",
            "/admin/v1/calls",
            403,
            "forbidden",
            Some("operator".into()),
        );
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["result"], "forbidden");
        assert_eq!(v["required_role"], "operator");
    }

    #[test]
    fn type_str_and_subject_are_consistent() {
        let ev = AuditEvent::sip_auth("1.2.3.4:5060", Some("carrier-a".into()), "failed");
        assert_eq!(ev.type_str(), "sip_auth");
        assert_eq!(ev.subject(), "1.2.3.4:5060");
    }

    #[test]
    fn config_reload_omits_empty_restart_list() {
        let ev = AuditEvent::config_reload("applied", vec![], None);
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["result"], "applied");
        assert!(v.get("restart_required").is_none());
        assert!(v.get("detail").is_none());
    }

    #[test]
    fn round_trips_through_json() {
        let ev = AuditEvent::attestation_rejected(
            Some("+13125551212".into()),
            "A",
            Some("C".into()),
            403,
            "attestation below policy",
        );
        let json = serde_json::to_string(&ev).unwrap();
        let back: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }
}

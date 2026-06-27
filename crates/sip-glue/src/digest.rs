//! Inbound RFC 3261 §22 digest authentication for new INVITEs.
//!
//! Wraps the upstream [`sip_auth::DigestAuthenticator`] (challenge /
//! verify / stale-challenge + nonce replay protection) with the
//! SiphonAI-side policy: which sources must authenticate, and a
//! credential store backed by the configured `[[sip.auth.user]]` set.
//!
//! The verifier needs the **cleartext** password to recompute HA1
//! (`H(username:realm:password)`) on each challenge — there is no
//! HA1-direct path upstream — so credentials are held in memory exactly
//! as `[[gateway]]` / `[[register]]` already hold outbound passwords.
//! Operators keep them out of the config file via `${file:…}` /
//! `${cred:…}` (v0.18.0).
//!
//! The gate runs in [`crate::handler`] *after* the trunk allowlist and
//! *before* route dispatch: an INVITE must pass the allowlist **and**
//! digest (decision: AND-gate, per-trunk opt-in via `auth_required`).

use std::collections::HashSet;

use sip_auth::{
    Authenticator, CredentialStore, Credentials, DigestAlgorithm, DigestAuthenticator, Qop,
};
use sip_core::{Request, Response};
use sip_uas::UserAgentServer;

/// Credential store over the configured `[[sip.auth.user]]` set. All
/// users share the single `[sip.auth].realm`.
struct ConfigCredentialStore {
    realm: String,
    users: Vec<(String, String)>, // (username, cleartext password)
}

impl CredentialStore for ConfigCredentialStore {
    fn fetch(&self, username: &str, realm: &str) -> Option<Credentials> {
        if realm != self.realm {
            return None;
        }
        self.users
            .iter()
            .find(|(u, _)| u == username)
            .map(|(u, p)| Credentials::new(u.clone(), p.clone(), self.realm.clone()))
    }
}

/// What to do with an INVITE that requires digest auth.
#[derive(Debug, PartialEq, Eq)]
pub enum DigestOutcome {
    /// The `Authorization` header verified — proceed with the call.
    Authenticated,
    /// Send a `401` challenge. `stale` ⇒ the presented nonce was one we
    /// issued but has expired (re-challenge silently, RFC 7616 §3.5);
    /// `had_credentials` ⇒ the client *did* present an `Authorization`
    /// header but it failed (bad password / unknown user) — used only
    /// to label the metric (`failed` vs first-time `challenged`).
    Challenge { stale: bool, had_credentials: bool },
}

impl DigestOutcome {
    /// The `siphon_ai_sip_auth_total{result}` label for this outcome.
    pub fn metric_result(&self) -> &'static str {
        match self {
            DigestOutcome::Authenticated => "ok",
            DigestOutcome::Challenge { stale: true, .. } => "stale",
            DigestOutcome::Challenge {
                had_credentials: true,
                ..
            } => "failed",
            DigestOutcome::Challenge { .. } => "challenged",
        }
    }
}

/// Inbound digest authenticator + per-source policy.
pub struct InboundDigestAuth {
    authn: DigestAuthenticator<ConfigCredentialStore>,
    /// Challenge every source (no trunk gate / legacy mode).
    require_all: bool,
    /// Trunk names that set `auth_required = true`.
    required_trunks: HashSet<String>,
}

impl InboundDigestAuth {
    /// Build from compiled `[sip.auth]` primitives. `algorithm` / `qop`
    /// are the canonical RFC tokens the config layer validated, so
    /// parsing here can't realistically fail; if it ever did we keep the
    /// upstream defaults (SHA-256 / auth) rather than panic.
    ///
    /// `require_all` is `true` when no trunk allowlist is configured
    /// (legacy mode — challenge everyone); otherwise only trunks in
    /// `required_trunks` are challenged.
    pub fn new(
        realm: &str,
        algorithm: &str,
        qop: &str,
        users: Vec<(String, String)>,
        require_all: bool,
        required_trunks: HashSet<String>,
    ) -> Self {
        let store = ConfigCredentialStore {
            realm: realm.to_string(),
            users,
        };
        let mut authn = DigestAuthenticator::new(realm, store);
        if let Some(a) = DigestAlgorithm::parse(algorithm) {
            authn = authn.with_algorithm(a);
        }
        if let Some(q) = Qop::parse(qop) {
            authn = authn.with_qop(q);
        }
        Self {
            authn,
            require_all,
            required_trunks,
        }
    }

    /// Does an INVITE attributed to `register_source` require digest?
    pub fn requires_auth(&self, register_source: &str) -> bool {
        self.require_all || self.required_trunks.contains(register_source)
    }

    /// Evaluate the request's `Authorization` header against the policy.
    pub fn evaluate(&self, request: &Request) -> DigestOutcome {
        let had_credentials = request.headers().get("Authorization").is_some();
        match self.authn.verify(request, request.headers()) {
            Ok(true) => DigestOutcome::Authenticated,
            _ => DigestOutcome::Challenge {
                stale: self.authn.nonce_is_stale(request),
                had_credentials,
            },
        }
    }

    /// Build the `401 Unauthorized` challenge for `request`, carrying a
    /// fresh `WWW-Authenticate` (nonce / realm / qop / opaque) lifted
    /// from the upstream challenge builder. Built on the daemon's normal
    /// [`UserAgentServer::create_response`] shell so the caller's
    /// `fill_response` adds Contact/Server consistently with every other
    /// response.
    pub fn challenge(&self, request: &Request, stale: bool) -> Response {
        let mut response = UserAgentServer::create_response(request, 401, "Unauthorized");
        let upstream = if stale {
            self.authn.challenge_stale(request)
        } else {
            Authenticator::challenge(&self.authn, request)
        };
        if let Ok(upstream) = upstream {
            if let Some(value) = upstream.headers().get("WWW-Authenticate") {
                let _ = response
                    .headers_mut()
                    .set_or_push("WWW-Authenticate", value);
            }
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_core::{Headers, Method, Request, RequestLine, SipUri};

    fn store_users() -> Vec<(String, String)> {
        vec![("alice".to_string(), "secret".to_string())]
    }

    fn auth(require_all: bool, trunks: &[&str]) -> InboundDigestAuth {
        InboundDigestAuth::new(
            "siphon.example",
            "SHA-256",
            "auth",
            store_users(),
            require_all,
            trunks.iter().map(|s| s.to_string()).collect(),
        )
    }

    fn invite(auth_header: Option<&str>) -> Request {
        let uri = SipUri::parse("sip:9000@siphon.example").unwrap();
        let mut h = Headers::new();
        h.push("Via", "SIP/2.0/UDP test;branch=z9hG4bKxa").unwrap();
        h.push("From", "<sip:caller@siphon.example>;tag=t").unwrap();
        h.push("To", "<sip:9000@siphon.example>").unwrap();
        h.push("Call-ID", "digest-test").unwrap();
        h.push("CSeq", "1 INVITE").unwrap();
        if let Some(a) = auth_header {
            h.push("Authorization", a).unwrap();
        }
        Request::new(
            RequestLine::new(Method::Invite, uri),
            h,
            bytes::Bytes::new(),
        )
        .unwrap()
    }

    #[test]
    fn requires_auth_honours_policy() {
        let all = auth(true, &[]);
        assert!(all.requires_auth("anything"));

        let per_trunk = auth(false, &["roaming"]);
        assert!(per_trunk.requires_auth("roaming"));
        assert!(!per_trunk.requires_auth("static-carrier"));
    }

    #[test]
    fn no_authorization_header_is_challenged_fresh() {
        let a = auth(true, &[]);
        let outcome = a.evaluate(&invite(None));
        assert_eq!(
            outcome,
            DigestOutcome::Challenge {
                stale: false,
                had_credentials: false
            }
        );
        assert_eq!(outcome.metric_result(), "challenged");
    }

    #[test]
    fn challenge_response_is_401_with_www_authenticate() {
        let a = auth(true, &[]);
        let resp = a.challenge(&invite(None), false);
        assert_eq!(resp.code(), 401);
        let wa = resp
            .headers()
            .get("WWW-Authenticate")
            .expect("WWW-Authenticate present");
        assert!(wa.contains("Digest"), "scheme: {wa}");
        assert!(wa.contains("realm=\"siphon.example\""), "realm: {wa}");
        assert!(wa.contains("nonce="), "nonce: {wa}");
    }

    #[test]
    fn round_trip_challenge_then_authenticate() {
        // Issue a challenge, parse its nonce/opaque, compute the client
        // response with the correct password, and assert it verifies.
        let a = auth(true, &[]);
        let challenge = a.challenge(&invite(None), false);
        let wa = challenge.headers().get("WWW-Authenticate").unwrap();
        let nonce = extract_param(wa, "nonce");
        let realm = extract_param(wa, "realm");
        let opaque = extract_param(wa, "opaque");

        let uri = "sip:9000@siphon.example";
        let cnonce = "0a4f113b";
        let nc = "00000001";
        let qop = "auth";
        // HA1 = SHA256(user:realm:pass); HA2 = SHA256(INVITE:uri);
        // response = SHA256(HA1:nonce:nc:cnonce:qop:HA2)
        let ha1 = sha256_hex(&format!("alice:{realm}:secret"));
        let ha2 = sha256_hex(&format!("INVITE:{uri}"));
        let response = sha256_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}"));
        let header = format!(
            "Digest username=\"alice\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", \
             algorithm=SHA-256, qop=auth, nc={nc}, cnonce=\"{cnonce}\", response=\"{response}\", \
             opaque=\"{opaque}\""
        );
        let outcome = a.evaluate(&invite(Some(&header)));
        assert_eq!(outcome, DigestOutcome::Authenticated, "header: {header}");
    }

    #[test]
    fn wrong_password_is_labelled_failed() {
        let a = auth(true, &[]);
        let challenge = a.challenge(&invite(None), false);
        let wa = challenge.headers().get("WWW-Authenticate").unwrap();
        let nonce = extract_param(wa, "nonce");
        let realm = extract_param(wa, "realm");
        let opaque = extract_param(wa, "opaque");
        let uri = "sip:9000@siphon.example";
        let (cnonce, nc, qop) = ("0a4f113b", "00000001", "auth");
        // Compute with the WRONG password.
        let ha1 = sha256_hex(&format!("alice:{realm}:WRONG"));
        let ha2 = sha256_hex(&format!("INVITE:{uri}"));
        let response = sha256_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}"));
        let header = format!(
            "Digest username=\"alice\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", \
             algorithm=SHA-256, qop=auth, nc={nc}, cnonce=\"{cnonce}\", response=\"{response}\", \
             opaque=\"{opaque}\""
        );
        let outcome = a.evaluate(&invite(Some(&header)));
        assert!(
            matches!(
                outcome,
                DigestOutcome::Challenge {
                    had_credentials: true,
                    ..
                }
            ),
            "expected a credentialed failure, got {outcome:?}"
        );
        assert_eq!(outcome.metric_result(), "failed");
    }

    fn extract_param(header: &str, name: &str) -> String {
        // Find `name=` then read the quoted or bare token.
        let key = format!("{name}=");
        let start = header.find(&key).expect("param present") + key.len();
        let rest = &header[start..];
        if let Some(stripped) = rest.strip_prefix('"') {
            stripped[..stripped.find('"').unwrap()].to_string()
        } else {
            rest.split([',', ' ']).next().unwrap().to_string()
        }
    }

    fn sha256_hex(input: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(input.as_bytes());
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

//! Serve REGISTER: browsers (SIP.js over `[sip.ws]`/`[sip.wss]`) and
//! softphones registering *to* this daemon (`[registrar]`,
//! DEV_PLAN_WebRTC.md Phase 1 §3.2).
//!
//! Thin adapter over upstream `sip-registrar` (CLAUDE.md §4.8): the
//! REGISTER protocol machinery — header validation, digest auth with
//! the AOR-to-identity authorization step, expires clamping (423 on
//! too-brief), Contact processing, the location store — is all
//! upstream `BasicRegistrar`. What this module adds is the part only
//! the daemon can know: **which connection each registration arrived
//! on**, and what to do when that connection dies.
//!
//! ## Connection binding (RFC 7118 §5.2 / RFC 5626-lite)
//!
//! A browser's Contact URI is unroutable — the only way to reach it is
//! the WebSocket connection it opened. On every successful REGISTER
//! over a stream transport we remember the per-connection writer next
//! to the AOR. One connection per client; a reconnect re-REGISTERs and
//! replaces the binding (full RFC 5626 flow failover is explicitly out
//! of scope for v1, per the plan).
//!
//! ## Expiry
//!
//! The sweeper task runs every few seconds and expires a registration
//! two ways: the ordinary Expires clock (upstream
//! `cleanup_expired()`), and **connection loss** — a stream-registered
//! AOR whose writer has closed is expired after a grace window (the
//! Phase 0 transport-loss semantic: a browser tab closing IS a
//! transport drop; the grace absorbs a quick reload re-REGISTERing).
//!
//! Auth note: digest here is upstream `sip-auth`, deliberately *not*
//! sip-glue's INVITE digest — `BasicRegistrar` couples verification to
//! the AOR-to-identity authorization check (user A cannot register
//! user B's AOR), which a bare digest gate does not provide. Both are
//! fed from the same `[sip.auth]` users, so clients see one credential
//! set.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use sip_auth::{Credentials, DigestAlgorithm, DigestAuthenticator, MemoryCredentialStore};
use sip_core::Request;
use sip_parse::{header, parse_to_header};
use sip_registrar::{normalize_aor, BasicRegistrar, MemoryLocationStore, Registrar};
use siphon_ai_telemetry::metrics::{REGISTRAR_BINDINGS, REGISTRAR_REGISTERS_TOTAL};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// How long a stream-registered AOR survives its connection closing
/// before the registration is expired. Long enough for a page reload
/// (which reconnects and re-REGISTERs, replacing the binding anyway);
/// short enough that a closed tab's identity stops being "registered"
/// promptly.
pub const DISCONNECT_GRACE: Duration = Duration::from_secs(32);

/// Sweep cadence. Bounds how far past its deadline a registration can
/// linger; correctness lives in the deadlines themselves.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Tunables from `[registrar]`.
#[derive(Debug, Clone)]
pub struct RegistrarSettings {
    pub default_expires: Duration,
    pub min_expires: Duration,
    pub max_expires: Duration,
    /// `[sip.auth]`-shaped credentials; `None` only with the explicit
    /// lab flag (config validation enforces that).
    pub auth: Option<RegistrarAuth>,
}

/// Digest parameters, from `[sip.auth]`.
#[derive(Debug, Clone)]
pub struct RegistrarAuth {
    pub realm: String,
    /// Canonical token: `MD5` | `SHA-256` | `SHA-512`.
    pub algorithm: String,
    pub users: Vec<(String, String)>,
}

/// Per-AOR connection state, tracked alongside the upstream location
/// store.
struct ConnState {
    /// The inbound connection's writer for stream transports; `None`
    /// for UDP registrations (routable by address, no binding needed).
    writer: Option<mpsc::Sender<Bytes>>,
    /// Set when the writer was first observed closed; the registration
    /// expires once this is older than [`DISCONNECT_GRACE`].
    dead_since: Option<tokio::time::Instant>,
}

type Inner = BasicRegistrar<MemoryLocationStore, DigestAuthenticator<MemoryCredentialStore>>;

/// The daemon's registrar. Cheap to share (`Arc` it once).
pub struct RegistrarService {
    inner: Inner,
    connections: Mutex<HashMap<String, ConnState>>,
    disconnect_grace: Duration,
}

impl RegistrarService {
    pub fn new(settings: RegistrarSettings) -> anyhow::Result<Self> {
        let store = MemoryLocationStore::new();
        let authenticator = match &settings.auth {
            None => None,
            Some(auth) => {
                let creds = auth
                    .users
                    .iter()
                    .map(|(u, p)| Credentials::new(u.as_str(), p.as_str(), auth.realm.as_str()))
                    .collect();
                let algorithm = match auth.algorithm.as_str() {
                    "MD5" => DigestAlgorithm::Md5,
                    "SHA-256" => DigestAlgorithm::Sha256,
                    "SHA-512" => DigestAlgorithm::Sha512,
                    other => anyhow::bail!("[sip.auth].algorithm {other:?} unsupported"),
                };
                Some(
                    DigestAuthenticator::new(&auth.realm, MemoryCredentialStore::with(creds))
                        .with_algorithm(algorithm),
                )
            }
        };
        let inner = BasicRegistrar::new(store, authenticator)
            .with_default_expires(settings.default_expires)
            .with_min_expires(settings.min_expires)
            .with_max_expires(settings.max_expires);
        Ok(Self {
            inner,
            connections: Mutex::new(HashMap::new()),
            disconnect_grace: DISCONNECT_GRACE,
        })
    }

    #[cfg(test)]
    fn with_disconnect_grace(mut self, grace: Duration) -> Self {
        self.disconnect_grace = grace;
        self
    }

    /// Handle one REGISTER. `stream` is the inbound connection's
    /// writer for stream transports (TCP/TLS/WS/WSS), `None` for UDP.
    /// Returns the response to send (the caller applies its usual
    /// response fill and sends it).
    pub fn handle_register(
        &self,
        request: &Request,
        stream: Option<&mpsc::Sender<Bytes>>,
    ) -> sip_core::Response {
        let response = match self.inner.handle_register(request) {
            Ok(r) => r,
            Err(e) => {
                // Upstream returns protocol errors as responses; an Err
                // is an internal fault (store poisoned, response build
                // failed). 500 and keep serving.
                warn!(error = %e, "registrar internal error");
                metrics::counter!(REGISTRAR_REGISTERS_TOTAL, "result" => "error").increment(1);
                return sip_uas::UserAgentServer::create_response(
                    request,
                    500,
                    "Server Internal Error",
                );
            }
        };

        let code = response.code();
        let result = match code {
            200 => "ok",
            401 => "challenged",
            403 => "forbidden",
            423 => "interval_too_brief",
            _ => "rejected",
        };
        metrics::counter!(REGISTRAR_REGISTERS_TOTAL, "result" => result).increment(1);

        if code == 200 {
            self.sync_binding(request, stream);
        }
        response
    }

    /// After a 200: mirror the location store into the connection map
    /// (register + refresh replace the entry; Expires: 0 removes it).
    fn sync_binding(&self, request: &Request, stream: Option<&mpsc::Sender<Bytes>>) {
        let Some(aor) = request_aor(request) else {
            return; // upstream 200'd it, so this cannot really happen
        };
        let registered = !self
            .inner
            .location_store()
            .lookup(&aor)
            .map(|b| b.is_empty())
            .unwrap_or(true);
        let mut conns = self.connections.lock();
        if registered {
            info!(aor = %aor, via_stream = stream.is_some(), "registration bound");
            conns.insert(
                aor,
                ConnState {
                    writer: stream.cloned(),
                    dead_since: None,
                },
            );
        } else {
            info!(aor = %aor, "registration removed (Expires: 0)");
            conns.remove(&aor);
        }
        metrics::gauge!(REGISTRAR_BINDINGS).set(conns.len() as f64);
    }

    /// The connection writer bound to `aor`, when it registered over a
    /// stream transport and the connection is still up. This is how a
    /// future browser-terminated INVITE finds its way out (plan Phase
    /// 2+); unused for call routing today.
    pub fn connection_for(&self, aor: &str) -> Option<mpsc::Sender<Bytes>> {
        let conns = self.connections.lock();
        conns
            .get(aor)
            .and_then(|c| c.writer.clone())
            .filter(|w| !w.is_closed())
    }

    /// Registered AORs, for logs/tests.
    pub fn bound_aors(&self) -> Vec<String> {
        self.connections.lock().keys().cloned().collect()
    }

    /// One sweep: expire by Expires clock, then by dead connection
    /// (after the grace window). Returns how many registrations were
    /// expired. Called from [`spawn_sweeper`]; public for tests.
    pub fn sweep(&self) -> usize {
        let clock_expired = self.inner.location_store().cleanup_expired().unwrap_or(0);
        let mut removed = 0;
        let now = tokio::time::Instant::now();
        let mut conns = self.connections.lock();
        conns.retain(|aor, state| {
            // Expires-clock removal upstream → drop our mirror entry.
            let still_stored = !self
                .inner
                .location_store()
                .lookup(aor)
                .map(|b| b.is_empty())
                .unwrap_or(true);
            if !still_stored {
                debug!(aor = %aor, "registration expired (Expires clock)");
                removed += 1;
                return false;
            }
            // Connection-loss expiry, Phase 0 semantics: mark when the
            // writer is first seen closed, expire after the grace.
            if let Some(w) = &state.writer {
                if w.is_closed() {
                    let dead = *state.dead_since.get_or_insert(now);
                    if now.duration_since(dead) >= self.disconnect_grace {
                        info!(aor = %aor, "registration expired (connection lost)");
                        let _ = self.inner.location_store().remove_all(aor);
                        removed += 1;
                        return false;
                    }
                } else {
                    state.dead_since = None;
                }
            }
            true
        });
        metrics::gauge!(REGISTRAR_BINDINGS).set(conns.len() as f64);
        drop(conns);
        if clock_expired > 0 {
            debug!(clock_expired, "registrar sweep reclaimed expired bindings");
        }
        removed + clock_expired
    }
}

/// Spawn the sweeper. Ends when every other `Arc` owner is gone
/// (daemon shutdown).
pub fn spawn_sweeper(service: Arc<RegistrarService>) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(&service);
    drop(service);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let Some(svc) = weak.upgrade() else {
                debug!("registrar sweeper stopping; service dropped");
                return;
            };
            svc.sweep();
        }
    })
}

/// The normalized AOR a REGISTER addresses, per the same To-header
/// path upstream `handle_register` uses.
fn request_aor(request: &Request) -> Option<String> {
    let to = header(request.headers(), "To")?;
    let parsed = parse_to_header(to)?;
    normalize_aor(parsed.inner().uri()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_core::{Headers, Method, RequestLine, SipUri};

    fn register_request(aor_user: &str, contact: &str, expires: u32) -> Request {
        register_request_cseq(aor_user, contact, expires, 1)
    }

    /// RFC 3261 §10.3: a re-REGISTER on the same Call-ID must carry a
    /// higher CSeq, and upstream enforces it — tests that register
    /// twice bump it.
    fn register_request_cseq(aor_user: &str, contact: &str, expires: u32, cseq: u32) -> Request {
        let uri = SipUri::parse("sip:siphon.example.com").unwrap();
        let mut h = Headers::new();
        h.push("Via", "SIP/2.0/WS client.invalid;branch=z9hG4bK-r1")
            .unwrap();
        h.push("Max-Forwards", "70").unwrap();
        h.push(
            "From",
            format!("<sip:{aor_user}@siphon.example.com>;tag=r1"),
        )
        .unwrap();
        h.push("To", format!("<sip:{aor_user}@siphon.example.com>"))
            .unwrap();
        h.push("Call-ID", "reg-1@client.invalid").unwrap();
        h.push("CSeq", format!("{cseq} REGISTER")).unwrap();
        h.push("Contact", contact).unwrap();
        h.push("Expires", expires.to_string()).unwrap();
        Request::new(RequestLine::new(Method::Register, uri), h, Bytes::new()).unwrap()
    }

    fn open_service() -> RegistrarService {
        RegistrarService::new(RegistrarSettings {
            default_expires: Duration::from_secs(3600),
            min_expires: Duration::from_secs(1),
            max_expires: Duration::from_secs(86400),
            auth: None,
        })
        .unwrap()
        .with_disconnect_grace(Duration::from_millis(200))
    }

    #[tokio::test]
    async fn register_binds_and_expires_on_connection_loss() {
        let svc = open_service();
        let (tx, rx) = mpsc::channel::<Bytes>(4);
        let req = register_request("browser", "<sip:browser@client.invalid;transport=ws>", 600);
        let resp = svc.handle_register(&req, Some(&tx));
        assert_eq!(resp.code(), 200, "{resp:?}");
        let aor = "sip:browser@siphon.example.com";
        assert!(svc.connection_for(aor).is_some());

        // Connection dies (browser tab closed): writer's receiver drops.
        drop(rx);
        assert_eq!(svc.sweep(), 0, "grace window must hold the binding");
        assert!(
            svc.connection_for(aor).is_none(),
            "closed writer must not be handed out even inside the grace"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(svc.sweep(), 1, "grace elapsed; registration expires");
        assert!(svc.bound_aors().is_empty());
        assert!(svc.inner.location_store().lookup(aor).unwrap().is_empty());
    }

    #[tokio::test]
    async fn reconnect_re_register_replaces_the_binding_and_survives() {
        let svc = open_service();
        let (tx1, rx1) = mpsc::channel::<Bytes>(4);
        let req = register_request("browser", "<sip:browser@client.invalid;transport=ws>", 600);
        assert_eq!(svc.handle_register(&req, Some(&tx1)).code(), 200);

        // Page reload: old connection dies, new one re-REGISTERs
        // within the grace.
        drop(rx1);
        svc.sweep();
        let (tx2, _rx2) = mpsc::channel::<Bytes>(4);
        let req2 = register_request_cseq(
            "browser",
            "<sip:browser@client.invalid;transport=ws>",
            600,
            2,
        );
        assert_eq!(svc.handle_register(&req2, Some(&tx2)).code(), 200);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(svc.sweep(), 0, "fresh connection must not be reaped");
        assert!(svc
            .connection_for("sip:browser@siphon.example.com")
            .is_some());
    }

    #[tokio::test]
    async fn unregister_with_expires_zero_removes_the_binding() {
        let svc = open_service();
        let (tx, _rx) = mpsc::channel::<Bytes>(4);
        let contact = "<sip:browser@client.invalid;transport=ws>";
        assert_eq!(
            svc.handle_register(&register_request("browser", contact, 600), Some(&tx))
                .code(),
            200
        );
        assert_eq!(svc.bound_aors().len(), 1);
        assert_eq!(
            svc.handle_register(&register_request_cseq("browser", contact, 0, 2), Some(&tx))
                .code(),
            200
        );
        assert!(svc.bound_aors().is_empty());
    }

    #[tokio::test]
    async fn digest_auth_challenges_then_admits() {
        let svc = RegistrarService::new(RegistrarSettings {
            default_expires: Duration::from_secs(3600),
            min_expires: Duration::from_secs(1),
            max_expires: Duration::from_secs(86400),
            auth: Some(RegistrarAuth {
                realm: "siphon.example".into(),
                algorithm: "MD5".into(),
                users: vec![("browser".into(), "s3cret".into())],
            }),
        })
        .unwrap();
        let req = register_request("browser", "<sip:browser@client.invalid;transport=ws>", 600);
        let resp = svc.handle_register(&req, None);
        assert_eq!(resp.code(), 401, "{resp:?}");
        assert!(
            resp.headers().get("WWW-Authenticate").is_some(),
            "401 must carry a challenge"
        );
        // The full digest round-trip is upstream sip-registrar's
        // covered ground; the harness ws phase exercises it e2e.
    }

    #[tokio::test]
    async fn udp_registration_has_no_connection_binding_but_registers() {
        let svc = open_service();
        let req = register_request("phone", "<sip:phone@192.0.2.10:5060>", 600);
        assert_eq!(svc.handle_register(&req, None).code(), 200);
        assert_eq!(svc.bound_aors().len(), 1);
        assert!(svc.connection_for("sip:phone@siphon.example.com").is_none());
        // No writer to lose — only the Expires clock applies.
        assert_eq!(svc.sweep(), 0);
    }
}

//! CSV rendering of [`CdrRecord`] for the file sink (0.36.0).
//!
//! CSV is a *flat* view of the record: nested optional blocks
//! (`audio`, `termination`, `consent`, `park`, `hold`, `reconnect`,
//! `quality`) become prefixed columns, and an absent block / unmeasured
//! value is an **empty cell** — same "absent ≠ zero" semantics as the
//! JSON shape. The column set is fixed per CDR schema version; adding a
//! CDR field means appending a column here (and the header) in the same
//! PR, per CLAUDE.md §7.7.
//!
//! Quoting follows RFC 4180: a field containing a comma, double quote,
//! CR, or LF is wrapped in double quotes with embedded quotes doubled.
//! We deliberately hand-roll this instead of pulling a csv crate — the
//! writer is ~30 lines and the dep tree stays small.

use std::fmt::Write as _;

use crate::schema::CdrRecord;

/// Header row (no trailing newline). Column order is append-only: new
/// columns go at the end so downstream ingestors keyed by position
/// survive additive schema changes.
pub const HEADER: &str = "version,call_id,sip_call_id,started_at,ended_at,duration_ms,\
from,to,direction,route,ws_url,\
audio_codec,audio_payload_type,audio_sample_rate,\
termination_cause,termination_bridge_disconnect,termination_tap_disconnect,\
verstat_attest,verstat_passed,\
recording_id,recording_path,recording_encrypted,recording_url,\
consent_announced,consent_announcement_ms,consent_server,\
park_count,park_total_ms,hold_count,hold_total_ms,\
reconnect_count,reconnect_total_gap_ms,\
quality_first_audio_out_ms,quality_barge_in_count,\
quality_avg_jitter_ms,quality_max_jitter_ms,\
quality_avg_packet_loss_ratio,quality_max_packet_loss_ratio,\
quality_avg_rtcp_rtt_ms,\
quality_rx_packets_received,quality_rx_packets_lost,\
quality_rx_packets_out_of_order,quality_rx_packets_duplicate,\
quality_mos_estimate_min,quality_mos_estimate_avg,\
quality_tx_packets_sent,quality_tx_octets_sent,\
quality_tx_packets_lost_reported";

/// Number of columns in [`HEADER`] (and every row). Only asserted in
/// tests — production code appends by name, not position.
#[cfg(test)]
pub(crate) const COLUMNS: usize = 48;

/// RFC 4180 field escaping: quote when the value contains a comma,
/// quote, or line break; double embedded quotes.
fn push_field(out: &mut String, value: &str) {
    if value.contains([',', '"', '\n', '\r']) {
        out.push('"');
        for c in value.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
    } else {
        out.push_str(value);
    }
}

/// Serialize an enum-on-the-wire value (Direction, TerminationCause)
/// to its snake_case wire string, exactly as the JSON shape emits it.
fn wire_str<T: serde::Serialize>(v: &T) -> String {
    // Only enum-unit → string serialization reaches this; a non-string
    // would be a schema bug caught by the round-trip tests below.
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

macro_rules! cell {
    // Optional value: absent → empty cell.
    (opt $out:ident, $v:expr) => {
        if let Some(x) = $v {
            let _ = write!($out, "{x}");
        }
        $out.push(',');
    };
    // Required Display value.
    ($out:ident, $v:expr) => {
        let _ = write!($out, "{}", $v);
        $out.push(',');
    };
}

/// Render one record as a CSV row (no trailing newline), column-for-
/// column matching [`HEADER`].
pub fn record_to_row(r: &CdrRecord) -> String {
    let mut o = String::with_capacity(256);

    cell!(o, r.version);
    push_field(&mut o, &r.call_id);
    o.push(',');
    push_field(&mut o, &r.sip_call_id);
    o.push(',');
    cell!(
        o,
        r.started_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
    cell!(
        o,
        r.ended_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
    cell!(o, r.duration_ms);
    push_field(&mut o, &r.from);
    o.push(',');
    push_field(&mut o, &r.to);
    o.push(',');
    cell!(o, wire_str(&r.direction));
    push_field(&mut o, &r.route);
    o.push(',');
    push_field(&mut o, &r.ws_url);
    o.push(',');

    push_field(&mut o, &r.audio.codec);
    o.push(',');
    cell!(o, r.audio.payload_type);
    cell!(o, r.audio.sample_rate);

    cell!(o, wire_str(&r.termination.cause));
    push_field(&mut o, &r.termination.bridge_disconnect);
    o.push(',');
    push_field(&mut o, &r.termination.tap_disconnect);
    o.push(',');

    cell!(opt o, &r.verstat_attest);
    cell!(opt o, r.verstat_passed);

    cell!(opt o, &r.recording_id);
    if let Some(p) = &r.recording_path {
        push_field(&mut o, p);
    }
    o.push(',');
    cell!(opt o, r.recording_encrypted);
    cell!(opt o, &r.recording_url);

    let c = r.consent.as_ref();
    cell!(opt o, c.map(|c| c.announced));
    cell!(opt o, c.map(|c| c.announcement_ms));
    cell!(opt o, c.and_then(|c| c.server.as_ref()));

    cell!(opt o, r.park.map(|p| p.count));
    cell!(opt o, r.park.map(|p| p.total_ms));
    cell!(opt o, r.hold.map(|h| h.count));
    cell!(opt o, r.hold.map(|h| h.total_ms));
    cell!(opt o, r.reconnect.map(|c| c.count));
    cell!(opt o, r.reconnect.map(|c| c.total_gap_ms));

    let q = r.quality.as_ref();
    cell!(opt o, q.and_then(|q| q.first_audio_out_ms));
    cell!(opt o, q.map(|q| q.barge_in_count));
    cell!(opt o, q.and_then(|q| q.avg_jitter_ms));
    cell!(opt o, q.and_then(|q| q.max_jitter_ms));
    cell!(opt o, q.and_then(|q| q.avg_packet_loss_ratio));
    cell!(opt o, q.and_then(|q| q.max_packet_loss_ratio));
    cell!(opt o, q.and_then(|q| q.avg_rtcp_rtt_ms));
    cell!(opt o, q.and_then(|q| q.rx_packets_received));
    cell!(opt o, q.and_then(|q| q.rx_packets_lost));
    cell!(opt o, q.and_then(|q| q.rx_packets_out_of_order));
    cell!(opt o, q.and_then(|q| q.rx_packets_duplicate));
    cell!(opt o, q.and_then(|q| q.mos_estimate_min));
    // 0.38.0 TX columns append after the 0.30.0 quality block rather
    // than sitting next to their rx_* counterparts: HEADER order is
    // append-only so position-keyed ingestors survive the addition.
    cell!(opt o, q.and_then(|q| q.mos_estimate_avg));
    cell!(opt o, q.and_then(|q| q.tx_packets_sent));
    cell!(opt o, q.and_then(|q| q.tx_octets_sent));
    // Last column: no trailing comma.
    if let Some(v) = q.and_then(|q| q.tx_packets_lost_reported) {
        let _ = write!(o, "{v}");
    }

    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        AudioInfo, ConsentInfo, Direction, HoldInfo, ParkInfo, QualityInfo, ReconnectInfo,
        TerminationCause, TerminationInfo, CDR_VERSION,
    };
    use chrono::TimeZone;

    fn sample() -> CdrRecord {
        CdrRecord {
            version: CDR_VERSION,
            call_id: "siphon-7f3a".into(),
            sip_call_id: "abc-123@pbx.example.com".into(),
            started_at: chrono::Utc.with_ymd_and_hms(2026, 5, 5, 14, 30, 0).unwrap(),
            ended_at: chrono::Utc
                .with_ymd_and_hms(2026, 5, 5, 14, 30, 42)
                .unwrap(),
            duration_ms: 42_000,
            from: "+13125551234".into(),
            to: "5000".into(),
            direction: Direction::Inbound,
            route: "main_reception".into(),
            ws_url: "wss://reception.example.com/sip-bridge".into(),
            audio: AudioInfo {
                codec: "PCMU".into(),
                payload_type: 0,
                sample_rate: 8000,
            },
            termination: TerminationInfo {
                cause: TerminationCause::ServerHangup,
                bridge_disconnect: "stop_sent".into(),
                tap_disconnect: "controller_hung_up".into(),
            },
            verstat_attest: None,
            verstat_passed: None,
            recording_id: None,
            recording_path: None,
            recording_encrypted: None,
            recording_url: None,
            consent: None,
            park: None,
            hold: None,
            reconnect: None,
            quality: None,
        }
    }

    /// Split a CSV row into fields, honouring RFC 4180 quoting.
    fn split(row: &str) -> Vec<String> {
        let mut fields = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut chars = row.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' if in_quotes && chars.peek() == Some(&'"') => {
                    cur.push('"');
                    chars.next();
                }
                '"' => in_quotes = !in_quotes,
                ',' if !in_quotes => fields.push(std::mem::take(&mut cur)),
                c => cur.push(c),
            }
        }
        fields.push(cur);
        fields
    }

    #[test]
    fn header_has_declared_column_count() {
        assert_eq!(HEADER.split(',').count(), COLUMNS);
        assert!(!HEADER.contains(' '), "header must be bare column names");
    }

    #[test]
    fn minimal_record_renders_all_columns() {
        let row = record_to_row(&sample());
        let fields = split(&row);
        assert_eq!(fields.len(), COLUMNS, "row: {row}");

        let header: Vec<&str> = HEADER.split(',').collect();
        let get = |name: &str| {
            let i = header.iter().position(|h| *h == name).expect(name);
            fields[i].clone()
        };
        assert_eq!(get("version"), CDR_VERSION.to_string());
        assert_eq!(get("call_id"), "siphon-7f3a");
        assert_eq!(get("started_at"), "2026-05-05T14:30:00.000Z");
        assert_eq!(get("direction"), "inbound");
        assert_eq!(get("termination_cause"), "server_hangup");
        assert_eq!(get("audio_codec"), "PCMU");
        // Absent blocks are empty cells, not zeros.
        assert_eq!(get("park_count"), "");
        assert_eq!(get("quality_barge_in_count"), "");
        assert_eq!(get("quality_mos_estimate_avg"), "");
    }

    #[test]
    fn fully_populated_record_renders_all_columns() {
        let mut r = sample();
        r.verstat_attest = Some("A".into());
        r.verstat_passed = Some(true);
        r.recording_id = Some("siphon-7f3a".into());
        r.recording_path = Some("/var/rec/siphon-7f3a.wava".into());
        r.recording_encrypted = Some(true);
        r.recording_url = Some("s3://bucket/siphon-7f3a.wava".into());
        r.consent = Some(ConsentInfo {
            announced: true,
            announcement_ms: 3200,
            server: Some("dtmf-1".into()),
        });
        r.park = Some(ParkInfo {
            count: 1,
            total_ms: 15_000,
        });
        r.hold = Some(HoldInfo {
            count: 2,
            total_ms: 4500,
        });
        r.reconnect = Some(ReconnectInfo {
            count: 1,
            total_gap_ms: 1200,
        });
        r.quality = Some(QualityInfo {
            first_audio_out_ms: Some(742),
            barge_in_count: 3,
            avg_jitter_ms: Some(11.5),
            max_jitter_ms: Some(30.0),
            avg_packet_loss_ratio: Some(0.004),
            max_packet_loss_ratio: Some(0.02),
            avg_rtcp_rtt_ms: None,
            rx_packets_received: Some(14_820),
            rx_packets_lost: Some(12),
            rx_packets_out_of_order: Some(3),
            rx_packets_duplicate: Some(0),
            tx_packets_sent: Some(14_900),
            tx_octets_sent: Some(2_384_000),
            tx_packets_lost_reported: Some(12),
            mos_estimate_min: Some(3.9),
            mos_estimate_avg: Some(4.3),
        });

        let fields = split(&record_to_row(&r));
        assert_eq!(fields.len(), COLUMNS);
        let header: Vec<&str> = HEADER.split(',').collect();
        let get = |name: &str| {
            let i = header.iter().position(|h| *h == name).expect(name);
            fields[i].clone()
        };
        assert_eq!(get("verstat_attest"), "A");
        assert_eq!(get("verstat_passed"), "true");
        assert_eq!(get("consent_server"), "dtmf-1");
        assert_eq!(get("park_total_ms"), "15000");
        assert_eq!(get("hold_count"), "2");
        assert_eq!(get("reconnect_total_gap_ms"), "1200");
        assert_eq!(get("quality_first_audio_out_ms"), "742");
        assert_eq!(get("quality_barge_in_count"), "3");
        assert_eq!(get("quality_avg_rtcp_rtt_ms"), "", "unmeasured → empty");
        assert_eq!(get("quality_mos_estimate_avg"), "4.3");
    }

    #[test]
    fn fields_with_commas_and_quotes_are_escaped() {
        let mut r = sample();
        r.from = "\"Alice, ext. 5\" <sip:alice@pbx>".into();
        r.termination.bridge_disconnect = "closed: reason=\"bye\", code=1000".into();
        let row = record_to_row(&r);
        let fields = split(&row);
        assert_eq!(fields.len(), COLUMNS, "escaping must not add columns");
        assert_eq!(fields[6], "\"Alice, ext. 5\" <sip:alice@pbx>");
        assert!(
            row.contains(r#""""Alice, ext. 5"" <sip:alice@pbx>""#),
            "raw row must carry RFC 4180 doubling: {row}"
        );
    }

    #[test]
    fn all_termination_causes_and_directions_have_wire_strings() {
        for cause in [
            TerminationCause::ServerHangup,
            TerminationCause::LocalShutdown,
            TerminationCause::DrainForced,
            TerminationCause::BridgeEnded,
            TerminationCause::TapEnded,
            TerminationCause::AckTimeout,
            TerminationCause::MissingSdpAnswer,
            TerminationCause::InvalidSdpAnswer,
            TerminationCause::NoCompatibleCodec,
            TerminationCause::InvalidRemoteMedia,
        ] {
            assert!(!wire_str(&cause).is_empty(), "{cause:?}");
        }
        assert_eq!(wire_str(&Direction::Outbound), "outbound");
    }
}

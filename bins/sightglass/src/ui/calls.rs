//! Calls tab: fleet-unified call table + focused-call detail pane.
//!
//! The detail pane renders the `GET /admin/v1/calls/:id/stats`
//! payload (the CDR `quality` shape) plus a client-side MOS
//! sparkline. Stats fields are read defensively — a young call
//! legitimately answers with most fields absent.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, TableState};
use ratatui::Frame;

use crate::model::App;

use super::Theme;

pub fn draw(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    // Collapse to a vertical stack on narrow terminals (§7).
    let (table_area, detail_area) = if area.width < 80 {
        let [t, d] =
            Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
        (t, d)
    } else {
        let [t, d] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
            .areas(area);
        (t, d)
    };

    let [active_area, history_area] =
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)])
            .areas(table_area);
    draw_table(frame, app, theme, active_area);
    draw_history(frame, app, theme, history_area);
    draw_detail(frame, app, theme, detail_area);
    // The SIP ladder takes the detail pane's space when open: a
    // call's exchange needs the height, and the stats it covers are
    // one keypress away (DESIGN_SIP_LADDER.md §5).
    super::ladder::draw(frame, app, theme, detail_area);
}

/// Recent completed calls (0.49.0+ daemons): a dim tail under the
/// active table, read defensively off the serialized CDR records —
/// the CDR schema is versioned; absent fields render as dashes.
fn draw_history(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let multi_node = app.nodes.len() > 1;
    let recent = app.visible_recent_cdrs();

    let mut headers = vec!["ENDED", "FROM → TO", "CAUSE", "DUR", "MOS"];
    if multi_node {
        headers.insert(0, "NODE");
    }
    let header = Row::new(
        headers
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, theme.dim_text()))),
    );

    let s = |v: &serde_json::Value, k: &str| {
        v.get(k).and_then(|x| x.as_str()).unwrap_or("—").to_string()
    };
    let rows = recent.iter().map(|(node, c)| {
        let ended = s(c, "ended_at");
        let ended = ended.get(11..19).unwrap_or(&ended).to_string();
        let route = format!("{} → {}", s(c, "from"), s(c, "to"));
        let cause = c
            .get("termination")
            .and_then(|t| t.get("cause"))
            .and_then(|x| x.as_str())
            .unwrap_or("—")
            .to_string();
        let dur = c
            .get("duration_ms")
            .and_then(|d| d.as_u64())
            .map(|ms| format!("{}s", ms / 1000))
            .unwrap_or_else(|| "—".into());
        let mos = c
            .get("quality")
            .and_then(|q| q.get("mos_estimate_avg"))
            .and_then(|m| m.as_f64())
            .map(|m| format!("{m:.1}"))
            .unwrap_or_else(|| "—".into());
        let mut cells = vec![
            Cell::from(Span::styled(ended, theme.dim_text())),
            Cell::from(Span::styled(route, theme.dim_text())),
            Cell::from(Span::styled(cause, theme.dim_text())),
            Cell::from(Span::styled(dur, theme.dim_text())),
            Cell::from(Span::styled(mos, theme.dim_text())),
        ];
        if multi_node {
            cells.insert(
                0,
                Cell::from(Span::styled(
                    app.nodes[*node].name.as_str(),
                    theme.dim_text(),
                )),
            );
        }
        Row::new(cells)
    });

    let mut widths = vec![
        Constraint::Length(8),
        Constraint::Min(16),
        Constraint::Length(14),
        Constraint::Length(6),
        Constraint::Length(4),
    ];
    if multi_node {
        widths.insert(0, Constraint::Length(10));
    }

    let title = format!(" recent ({}) ", recent.len());
    let table = Table::new(rows, widths).header(header).block(
        Block::new()
            .borders(Borders::ALL)
            .border_style(theme.dim_text())
            .title(Span::styled(title, theme.dim_text())),
    );
    frame.render_widget(table, area);
}

fn draw_table(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let multi_node = app.nodes.len() > 1;
    let visible = app.visible_calls();

    // MEDIA is the browser column (`DEV_PLAN_WebRTC.md` §4.6): one
    // glance answers "is that browser call actually up, and if not, is
    // it stuck on ICE or on DTLS". It appears only when a visible call
    // has a phase to report, so a fleet with no browser traffic keeps
    // the exact table it had — the column costs width, and width is
    // what this table is always short of.
    let any_webrtc = visible.iter().any(|(_, c)| c.webrtc_state.is_some());
    let mut headers = vec!["CALL ID", "SIP CALL ID", "DIR"];
    if any_webrtc {
        headers.push("MEDIA");
    }
    if multi_node {
        headers.insert(0, "NODE");
    }
    let header = Row::new(
        headers
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, theme.dim_text()))),
    );

    let rows = visible.iter().map(|(node, c)| {
        let mut cells = vec![
            Cell::from(Span::styled(c.call_id.as_str(), theme.text())),
            Cell::from(Span::styled(c.sip_call_id.as_str(), theme.dim_text())),
            Cell::from(Span::styled(c.direction.as_str(), theme.text())),
        ];
        if any_webrtc {
            cells.push(Cell::from(match c.webrtc_state.as_deref() {
                Some(state) => Span::styled(
                    media_label(state),
                    ratatui::style::Style::default().fg(theme.webrtc_color(state)),
                ),
                // A classic leg has no phase — a dash, never a blank
                // that could read as "unknown".
                None => Span::styled("—", theme.dim_text()),
            }));
        }
        if multi_node {
            cells.insert(
                0,
                Cell::from(Span::styled(app.nodes[*node].name.as_str(), theme.text())),
            );
        }
        Row::new(cells)
    });

    let mut widths = vec![
        Constraint::Min(18),
        Constraint::Min(16),
        Constraint::Length(8),
    ];
    if any_webrtc {
        widths.push(Constraint::Length(6));
    }
    if multi_node {
        widths.insert(0, Constraint::Length(10));
    }

    let title = format!(" calls ({}) ", visible.len());
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(theme.selected_row())
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(theme.dim_text())
                .title(Span::styled(title, theme.title())),
        );

    let mut state = TableState::default();
    if !visible.is_empty() {
        state.select(Some(app.selected_call.min(visible.len() - 1)));
    }
    frame.render_stateful_widget(table, area, &mut state);

    if visible.is_empty() {
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 2,
            width: area.width.saturating_sub(4),
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Span::styled("no active calls", theme.dim_text())),
            inner,
        );
    }
}

fn draw_detail(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme.dim_text())
        .title(Span::styled(" call detail ", theme.title()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some((node, call_id)) = &app.stats.key else {
        frame.render_widget(
            Paragraph::new(Span::styled("no call selected", theme.dim_text())),
            inner,
        );
        return;
    };

    let [text_area, spark_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(4)]).areas(inner);

    let mut lines = vec![
        field_line(theme, "call", call_id),
        field_line(theme, "node", &app.nodes[*node].name),
    ];
    match &app.stats.latest {
        None => lines.push(Line::styled("fetching stats…", theme.dim_text())),
        Some(stats) => {
            for (key, label) in [
                ("direction", "direction"),
                ("from", "from"),
                ("to", "to"),
                ("sip_call_id", "sip call-id"),
                ("srtp_profile", "srtp"),
                // Only a browser leg reports this, so the line simply
                // does not appear for a classic call.
                ("webrtc_state", "ice/dtls"),
                ("verstat_attest", "attest"),
            ] {
                if let Some(v) = stats.get(key).and_then(|v| v.as_str()) {
                    lines.push(field_line_owned(theme, label, v.to_string()));
                }
            }
            if let Some(rate) = stats.get("sample_rate").and_then(|v| v.as_u64()) {
                lines.push(field_line_owned(theme, "rate", format!("{rate} Hz")));
            }
            lines.push(Line::default());
            push_stat(
                &mut lines,
                theme,
                stats,
                "mos_estimate_avg",
                "MOS avg",
                None,
            );
            push_stat(
                &mut lines,
                theme,
                stats,
                "mos_estimate_min",
                "MOS min",
                None,
            );
            push_stat(
                &mut lines,
                theme,
                stats,
                "avg_jitter_ms",
                "jitter avg",
                Some("ms"),
            );
            push_stat(
                &mut lines,
                theme,
                stats,
                "max_jitter_ms",
                "jitter max",
                Some("ms"),
            );
            push_ratio(
                &mut lines,
                theme,
                stats,
                "avg_packet_loss_ratio",
                "loss avg",
            );
            push_stat(
                &mut lines,
                theme,
                stats,
                "rx_packets_received",
                "rx packets",
                None,
            );
            push_stat(&mut lines, theme, stats, "rx_packets_lost", "rx lost", None);
            push_stat(
                &mut lines,
                theme,
                stats,
                "tx_packets_sent",
                "tx packets",
                None,
            );
            push_stat(
                &mut lines,
                theme,
                stats,
                "first_audio_out_ms",
                "first audio",
                Some("ms"),
            );
            push_stat(
                &mut lines,
                theme,
                stats,
                "barge_in_count",
                "barge-ins",
                None,
            );
            if let Some(ts) = stats.get("sampled_at").and_then(|v| v.as_str()) {
                lines.push(Line::default());
                lines.push(field_line(theme, "sampled", ts));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), text_area);

    let mos: Vec<u64> = app.stats.mos_hist.iter().copied().collect();
    let spark = Sparkline::default()
        .data(&mos)
        // MOS is 1.0–4.5 (×100 in the ring); a fixed max keeps the
        // sparkline comparable across calls instead of auto-scaling.
        .max(450)
        .style(theme.text().fg(theme.accent))
        .block(
            Block::new()
                .borders(Borders::TOP)
                .border_style(theme.dim_text())
                .title(Span::styled(" MOS trend ", theme.dim_text())),
        );
    frame.render_widget(spark, spark_area);
}

/// The daemon's `webrtc_state` vocabulary, compressed to fit a column
/// that shares a narrow table with two call ids. The mapping keeps the
/// distinction that matters — *which* phase a stalled call is stuck in
/// — and spends the saved width on the ids. The full string is in the
/// detail pane (and in `GET /admin/v1/calls`).
fn media_label(state: &str) -> &'static str {
    match state {
        // ICE checks are running: no path yet.
        "connecting" => "ice",
        // A pair is nominated and DTLS is handshaking.
        "ice_connected" => "dtls",
        // Up: this call's media is a browser's.
        "connected" => "webrtc",
        "failed" => "failed",
        "closed" => "closed",
        // A phase this build does not know: say so rather than
        // silently rendering it as healthy.
        _ => "?",
    }
}

fn field_line_owned(theme: &Theme, label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:>12}  "), theme.dim_text()),
        Span::styled(value, theme.text()),
    ])
}

fn field_line<'a>(theme: &Theme, label: &'a str, value: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:>12}  "), theme.dim_text()),
        Span::styled(value, theme.text()),
    ])
}

fn push_stat(
    lines: &mut Vec<Line<'_>>,
    theme: &Theme,
    stats: &serde_json::Value,
    key: &str,
    label: &str,
    unit: Option<&str>,
) {
    let Some(v) = stats.get(key).and_then(|v| v.as_f64()) else {
        return; // unmeasured fields are omitted, not zeroed
    };
    let text = if v.fract() == 0.0 {
        format!(
            "{v:.0}{}",
            unit.map(|u| format!(" {u}")).unwrap_or_default()
        )
    } else {
        format!(
            "{v:.2}{}",
            unit.map(|u| format!(" {u}")).unwrap_or_default()
        )
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{label:>12}  "), theme.dim_text()),
        Span::styled(text, theme.text()),
    ]));
}

fn push_ratio(
    lines: &mut Vec<Line<'_>>,
    theme: &Theme,
    stats: &serde_json::Value,
    key: &str,
    label: &str,
) {
    let Some(v) = stats.get(key).and_then(|v| v.as_f64()) else {
        return;
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{label:>12}  "), theme.dim_text()),
        Span::styled(format!("{:.2}%", v * 100.0), theme.text()),
    ]));
}

#[cfg(test)]
mod tests {
    use super::media_label;

    #[test]
    fn every_daemon_phase_has_a_column_label() {
        // The daemon's vocabulary (webrtc-glue's `LegPhase::as_str`).
        assert_eq!(media_label("connecting"), "ice");
        assert_eq!(media_label("ice_connected"), "dtls");
        assert_eq!(media_label("connected"), "webrtc");
        assert_eq!(media_label("failed"), "failed");
        assert_eq!(media_label("closed"), "closed");
        // A newer daemon than this sightglass must not render as
        // healthy — sightglass is version-skewed against the fleet by
        // design.
        assert_eq!(media_label("teleported"), "?");
    }
}

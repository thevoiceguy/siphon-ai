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

    draw_table(frame, app, theme, table_area);
    draw_detail(frame, app, theme, detail_area);
}

fn draw_table(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let multi_node = app.nodes.len() > 1;
    let visible = app.visible_calls();

    let mut headers = vec!["CALL ID", "SIP CALL ID", "DIR"];
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

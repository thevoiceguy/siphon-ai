//! Errors tab: the fleet's merged recent-errors tail, newest first
//! (DESIGN_SIGHTGLASS.md §4). Fed by each 0.49.0+ daemon's
//! `GET /admin/v1/errors`; a node whose daemon predates the endpoint
//! degrades to a note, never an error screen.

use ratatui::layout::{Constraint, Rect};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::model::App;

use super::Theme;

pub fn draw(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let multi_node = app.nodes.len() > 1;
    let visible = app.visible_errors();

    let mut headers = vec!["TIME", "LVL", "CALL", "MESSAGE"];
    if multi_node {
        headers.insert(1, "NODE");
    }
    let header = Row::new(
        headers
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, theme.dim_text()))),
    );

    let rows = visible.iter().map(|(node, e)| {
        let level_color = if e.level == "error" {
            theme.err
        } else {
            theme.warn
        };
        let mut cells = vec![
            Cell::from(Span::styled(hhmmss_utc(e.ts_ms), theme.dim_text())),
            Cell::from(Span::styled(e.level.as_str(), theme.text().fg(level_color))),
            Cell::from(Span::styled(
                e.call_id.as_deref().unwrap_or("—"),
                theme.dim_text(),
            )),
            Cell::from(Span::styled(e.message.as_str(), theme.text())),
        ];
        if multi_node {
            cells.insert(
                1,
                Cell::from(Span::styled(app.nodes[*node].name.as_str(), theme.text())),
            );
        }
        Row::new(cells)
    });

    let mut widths = vec![
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Length(22),
        Constraint::Min(20),
    ];
    if multi_node {
        widths.insert(1, Constraint::Length(10));
    }

    let title = format!(" errors ({}) ", visible.len());
    let table = Table::new(rows, widths).header(header).block(
        Block::new()
            .borders(Borders::ALL)
            .border_style(theme.dim_text())
            .title(Span::styled(title, theme.title())),
    );
    frame.render_widget(table, area);

    // Nodes that answered but don't serve the endpoint (pre-0.49
    // daemons) get a note rather than silently contributing nothing.
    let missing: Vec<&str> = app
        .nodes
        .iter()
        .enumerate()
        .filter(|(id, n)| {
            app.node_filter.is_none_or(|f| f == *id)
                && n.errors.is_none()
                && n.health == crate::model::NodeHealth::Up
        })
        .map(|(_, n)| n.name.as_str())
        .collect();
    let note_y = if visible.is_empty() && missing.is_empty() {
        Some(("no captured warnings or errors", theme.dim_text()))
    } else {
        None
    };
    if let Some((note, style)) = note_y {
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 2,
            width: area.width.saturating_sub(4),
            height: 1,
        };
        frame.render_widget(Paragraph::new(Span::styled(note, style)), inner);
    } else if !missing.is_empty() {
        let inner = Rect {
            x: area.x + 2,
            y: area.y + area.height.saturating_sub(2),
            width: area.width.saturating_sub(4),
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(
                    "errors endpoint unavailable on: {} (daemon < 0.49)",
                    missing.join(", ")
                ),
                theme.dim_text(),
            )),
            inner,
        );
    }
}

/// `HH:MM:SS` UTC from epoch millis — enough for a same-day tail;
/// the full timestamp stays available over curl.
fn hhmmss_utc(ts_ms: u64) -> String {
    let s = ts_ms / 1000;
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hhmmss_wraps_by_day() {
        assert_eq!(hhmmss_utc(0), "00:00:00");
        // 2026-08-17T19:32:38Z
        assert_eq!(hhmmss_utc(1_786_995_158_000), "19:32:38");
    }
}

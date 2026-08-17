//! System tab: per-node operational controls (DESIGN_SIGHTGLASS.md
//! §4) — the "first five minutes of an incident" surface. One row
//! per visible node: live log filter, drain state, HEP; `L`/`H`/`D`
//! act on the selected row (admin role, node-named confirms).

use ratatui::layout::{Constraint, Rect};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::model::{App, NodeHealth};

use super::Theme;

pub fn draw(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let visible = app.visible_system_nodes();

    let header = Row::new(
        ["", "NODE", "LOG FILTER", "DRAIN", "HEP", "VERSION"]
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, theme.dim_text()))),
    );

    let rows = visible.iter().map(|id| {
        let n = &app.nodes[*id];
        let (glyph, color) = theme.health_glyph(&n.health);
        let stale = matches!(n.health, NodeHealth::Down { .. });
        let base = if stale {
            theme.dim_text()
        } else {
            theme.text()
        };
        let drain = match &n.drain {
            Some(d) if d.draining => match d.remaining_secs {
                Some(s) => format!("draining ({s}s)"),
                None => "draining".to_string(),
            },
            Some(_) => "idle".to_string(),
            None => "—".to_string(),
        };
        let drain_style = if drain.starts_with("draining") {
            theme.text().fg(theme.warn)
        } else {
            base
        };
        let hep = match &n.status {
            Some(s) if s.hep_enabled => "enabled",
            Some(_) => "off",
            None => "—",
        };
        Row::new(vec![
            Cell::from(Span::styled(glyph, theme.text().fg(color))),
            Cell::from(Span::styled(n.name.as_str(), base)),
            Cell::from(Span::styled(n.log_filter.as_deref().unwrap_or("—"), base)),
            Cell::from(Span::styled(drain, drain_style)),
            Cell::from(Span::styled(hep, base)),
            Cell::from(Span::styled(
                n.status.as_ref().map(|s| s.version.as_str()).unwrap_or("—"),
                base,
            )),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(16),
            Constraint::Min(28),
            Constraint::Length(15),
            Constraint::Length(8),
            Constraint::Length(9),
        ],
    )
    .header(header)
    .row_highlight_style(theme.selected_row())
    .block(
        Block::new()
            .borders(Borders::ALL)
            .border_style(theme.dim_text())
            .title(Span::styled(" system ", theme.title())),
    );

    let mut state = TableState::default();
    if !visible.is_empty() {
        state.select(Some(app.selected_system.min(visible.len() - 1)));
    }
    frame.render_stateful_widget(table, area, &mut state);

    let hint = Rect {
        x: area.x + 2,
        y: area.y + area.height.saturating_sub(2),
        width: area.width.saturating_sub(4),
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            "log-filter changes are live (no restart); drain = programmatic SIGTERM — the node stops taking calls",
            theme.dim_text(),
        )),
        hint,
    );
}

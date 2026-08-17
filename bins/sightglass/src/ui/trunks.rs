//! Trunks tab: every `[[register]]` binding across the fleet.
//!
//! Gateway (IP-auth) trunk health is an open question deliberately
//! deferred (DESIGN_SIGHTGLASS.md §6.6); this tab shows registration
//! state, which is what the daemon has live data for today.

use ratatui::layout::{Constraint, Rect};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Cell, Row, Table};
use ratatui::Frame;

use crate::model::{App, NodeHealth};

use super::Theme;

pub fn draw(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let multi_node = app.nodes.len() > 1;

    let mut headers = vec!["NAME", "SERVER", "STATUS", "EXPIRES", "LAST ERROR"];
    if multi_node {
        headers.insert(0, "NODE");
    }
    let header = Row::new(
        headers
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, theme.dim_text()))),
    );

    let mut rows = Vec::new();
    for (id, n) in app.nodes.iter().enumerate() {
        if app.node_filter.is_some_and(|f| f != id) {
            continue;
        }
        let stale = matches!(n.health, NodeHealth::Down { .. });
        for r in &n.registrations {
            let base = if stale {
                theme.dim_text()
            } else {
                theme.text()
            };
            let status_style = if stale {
                theme.dim_text()
            } else {
                theme.text().fg(theme.registration_color(&r.status))
            };
            let mut cells = vec![
                Cell::from(Span::styled(r.name.as_str(), base)),
                Cell::from(Span::styled(r.server_addr.as_str(), base)),
                Cell::from(Span::styled(r.status.as_str(), status_style)),
                Cell::from(Span::styled(r.expires_at.as_deref().unwrap_or("—"), base)),
                Cell::from(Span::styled(
                    r.last_error.as_deref().unwrap_or(""),
                    theme.dim_text(),
                )),
            ];
            if multi_node {
                cells.insert(0, Cell::from(Span::styled(n.name.as_str(), base)));
            }
            rows.push(Row::new(cells));
        }
    }

    let mut widths = vec![
        Constraint::Length(16),
        Constraint::Length(22),
        Constraint::Length(12),
        Constraint::Length(22),
        Constraint::Min(10),
    ];
    if multi_node {
        widths.insert(0, Constraint::Length(12));
    }

    let empty = rows.is_empty();
    let table = Table::new(rows, widths).header(header).block(
        Block::new()
            .borders(Borders::ALL)
            .border_style(theme.dim_text())
            .title(Span::styled(" trunks ", theme.title())),
    );
    frame.render_widget(table, area);

    if empty {
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 2,
            width: area.width.saturating_sub(4),
            height: 1,
        };
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Span::styled(
                "no [[register]] bindings on the visible nodes",
                theme.dim_text(),
            )),
            inner,
        );
    }
}

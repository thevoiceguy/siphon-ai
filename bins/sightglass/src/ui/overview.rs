//! Overview tab: fleet health grid + aggregate activity.
//!
//! Per-node version/uptime columns arrive with `GET /admin/v1/status`
//! (DESIGN_SIGHTGLASS.md §6.2, PR 4); until then the grid shows what
//! the existing endpoints answer.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Row, Sparkline, Table};
use ratatui::Frame;

use crate::model::{App, NodeHealth, NodeState};

use super::Theme;

pub fn draw(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let [grid_area, spark_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(5)]).areas(area);

    draw_grid(frame, app, theme, grid_area);
    draw_activity(frame, app, theme, spark_area);
}

fn draw_grid(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let header = Row::new(
        ["", "NODE", "STATUS", "CALLS", "REGISTRATIONS", "DRAIN"]
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, theme.dim_text()))),
    );

    let rows = app
        .nodes
        .iter()
        .filter(|_| true)
        .enumerate()
        .filter(|(id, _)| app.node_filter.is_none_or(|f| f == *id))
        .map(|(_, n)| node_row(n, theme));

    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(18),
            Constraint::Min(24),
            Constraint::Length(6),
            Constraint::Length(14),
            Constraint::Length(16),
        ],
    )
    .header(header)
    .block(
        Block::new()
            .borders(Borders::ALL)
            .border_style(theme.dim_text())
            .title(Span::styled(" fleet ", theme.title())),
    );
    frame.render_widget(table, area);
}

fn node_row<'a>(n: &'a NodeState, theme: &Theme) -> Row<'a> {
    let (glyph, color) = theme.health_glyph(&n.health);
    // A down node keeps its last-seen numbers but renders dimmed —
    // stale data must not read as live (§2).
    let data_style = match n.health {
        NodeHealth::Down { .. } => theme.dim_text(),
        _ => theme.text(),
    };
    let status: Line = match &n.health {
        NodeHealth::Up => Line::styled("up", theme.text().fg(theme.ok)),
        NodeHealth::Connecting => Line::styled("connecting…", theme.text().fg(theme.warn)),
        NodeHealth::Down { error } => Line::styled(
            format!("down (retrying) — {error}"),
            theme.text().fg(theme.err),
        ),
    };
    let registered = n
        .registrations
        .iter()
        .filter(|r| r.status == "registered")
        .count();
    let regs = if n.registrations.is_empty() {
        "—".to_string()
    } else {
        format!("{registered}/{}", n.registrations.len())
    };
    let drain = match &n.drain {
        Some(d) if d.draining => match d.remaining_secs {
            Some(s) => format!("draining ({s}s)"),
            None => "draining".to_string(),
        },
        Some(_) => "idle".to_string(),
        None => "—".to_string(),
    };
    Row::new(vec![
        Cell::from(Span::styled(glyph, theme.text().fg(color))),
        Cell::from(Span::styled(n.name.as_str(), data_style)),
        Cell::from(status),
        Cell::from(Span::styled(n.calls.len().to_string(), data_style)),
        Cell::from(Span::styled(regs, data_style)),
        Cell::from(Span::styled(drain, data_style)),
    ])
}

fn draw_activity(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let history: Vec<u64> = app.fleet_history.iter().copied().collect();
    let spark = Sparkline::default()
        .data(&history)
        .style(theme.text().fg(theme.accent))
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(theme.dim_text())
                .title(Span::styled(" active calls (fleet) ", theme.title())),
        );
    frame.render_widget(spark, area);
}

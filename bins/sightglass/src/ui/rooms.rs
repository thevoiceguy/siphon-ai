//! Rooms tab: conference rooms (with members inline) and parked
//! calls across the fleet (DESIGN_SIGHTGLASS.md §4). Selection feeds
//! the `x` (end/kick/hangup) and `u` (retrieve) actions in `keys`.

use ratatui::layout::{Constraint, Rect};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::model::{App, RoomsRowKind};

use super::Theme;

pub fn draw(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let multi_node = app.nodes.len() > 1;
    let visible = app.visible_rooms();

    let mut headers = vec!["TYPE", "ID", "DETAIL"];
    if multi_node {
        headers.insert(0, "NODE");
    }
    let header = Row::new(
        headers
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, theme.dim_text()))),
    );

    let rows = visible.iter().map(|r| {
        let (kind, id, detail, style) = match &r.kind {
            RoomsRowKind::Room {
                room_id,
                sample_rate,
                participants,
            } => (
                "room",
                room_id.clone(),
                format!("{sample_rate} Hz · {participants} member(s)"),
                theme.text().fg(theme.accent),
            ),
            RoomsRowKind::Participant { call_id, .. } => {
                ("  member", call_id.clone(), String::new(), theme.text())
            }
            RoomsRowKind::Parked {
                call_id,
                slot,
                parked_secs,
            } => (
                "parked",
                call_id.clone(),
                match slot {
                    Some(s) => format!("slot {s} · {parked_secs}s"),
                    None => format!("{parked_secs}s"),
                },
                theme.text().fg(theme.warn),
            ),
        };
        let mut cells = vec![
            Cell::from(Span::styled(kind, theme.dim_text())),
            Cell::from(Span::styled(id, style)),
            Cell::from(Span::styled(detail, theme.dim_text())),
        ];
        if multi_node {
            cells.insert(
                0,
                Cell::from(Span::styled(app.nodes[r.node].name.as_str(), theme.text())),
            );
        }
        Row::new(cells)
    });

    let mut widths = vec![
        Constraint::Length(10),
        Constraint::Min(24),
        Constraint::Min(16),
    ];
    if multi_node {
        widths.insert(0, Constraint::Length(10));
    }

    let title = format!(" rooms & parked ({}) ", visible.len());
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
        state.select(Some(app.selected_room.min(visible.len() - 1)));
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
            Paragraph::new(Span::styled(
                "no conference rooms or parked calls on the visible nodes",
                theme.dim_text(),
            )),
            inner,
        );
    }
}

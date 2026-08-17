//! Header, tab bar, footer — the frame around every tab.
//!
//! The footer shows only the keybinds valid on the current tab
//! (DESIGN_SIGHTGLASS.md §7).

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Tabs};
use ratatui::Frame;

use crate::model::{App, Tab};

use super::Theme;

/// Draw the chrome; returns the body area the active tab renders into.
pub fn draw(frame: &mut Frame, app: &App, theme: &Theme) -> Rect {
    let [header, tabs, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, theme, header);
    draw_tabs(frame, app, theme, tabs);
    draw_footer(frame, app, theme, footer);
    body
}

fn draw_header(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let sep = Span::styled("  │  ", theme.dim_text());
    let mut spans = vec![
        Span::styled(
            " sightglass ",
            theme.title().add_modifier(Modifier::REVERSED),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{}/{} up", app.nodes_up(), app.nodes.len()),
            theme.text(),
        ),
        sep.clone(),
        Span::styled(format!("{} calls", app.total_calls()), theme.text()),
    ];
    if let Some(id) = app.node_filter {
        spans.push(sep.clone());
        spans.push(Span::styled(
            format!("node: {}", app.nodes[id].name),
            theme.title(),
        ));
    }
    if app.read_only {
        spans.push(sep);
        spans.push(Span::styled("read-only", theme.text().fg(theme.warn)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_tabs(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let titles = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| Line::from(format!(" {} {} ", i + 1, t.title())));
    let selected = Tab::ALL
        .iter()
        .position(|t| *t == app.tab)
        .unwrap_or_default();
    let tabs = Tabs::new(titles)
        .select(selected)
        .style(theme.dim_text())
        .highlight_style(theme.title().add_modifier(Modifier::UNDERLINED))
        .divider(Span::styled("│", theme.dim_text()));
    frame.render_widget(tabs, area);
}

fn draw_footer(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let mut hints: Vec<(&str, &str)> = vec![("q", "quit"), ("⇥/1-3", "tabs")];
    if app.nodes.len() > 1 {
        hints.push(("n", "node filter"));
    }
    if app.tab == Tab::Calls {
        hints.push(("j/k", "select"));
        hints.push(("g/G", "top/bottom"));
    }
    let mut spans = Vec::with_capacity(hints.len() * 3);
    for (key, what) in hints {
        spans.push(Span::styled(format!(" {key} "), theme.title()));
        spans.push(Span::styled(format!("{what}  "), theme.dim_text()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

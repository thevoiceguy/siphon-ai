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
    // While a form modal is open the only valid keys are its own.
    if matches!(app.modal, Some(crate::model::Modal::Input(_))) {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " enter submit · tab next field · esc cancel ",
                theme.dim_text(),
            )),
            area,
        );
        return;
    }
    let mut spans: Vec<Span> = Vec::new();
    let mut hint = |key: String, what: &str, enabled: bool| {
        if enabled {
            spans.push(Span::styled(format!(" {key} "), theme.title()));
            spans.push(Span::styled(format!("{what}  "), theme.dim_text()));
        } else {
            // Greyed, not hidden: the key exists, this token can't
            // use it (§5 RBAC-aware keybinds).
            spans.push(Span::styled(format!(" {key} "), theme.dim_text()));
            spans.push(Span::styled(format!("{what}✗  "), theme.dim_text()));
        }
    };
    // These two are kept terse on purpose: the tab bar above already
    // shows the tab names and the header shows the node filter chip,
    // so spending footer columns on them squeezes out the action keys
    // — which are the only thing the footer is the sole source of.
    // Adding the `s` hint overflowed 110 columns and truncated
    // `originate`; a render test pins the fit.
    hint("q".into(), "quit", true);
    hint("⇥".into(), "tabs", true);
    if app.nodes.len() > 1 {
        hint("n".into(), "nodes", true);
    }
    if app.tab == Tab::System {
        hint("j/k".into(), "select node", true);
        let node = app
            .visible_system_nodes()
            .get(app.selected_system)
            .copied()
            .unwrap_or(0);
        let admin_ok = app.can(node, crate::model::Role::Admin);
        hint("L".into(), "log filter", admin_ok);
        hint("H".into(), "hep probe", admin_ok);
        hint("D".into(), "drain", admin_ok);
    }
    if app.tab == Tab::Rooms {
        hint("j/k".into(), "select", true);
        let node = app
            .visible_rooms()
            .get(app.selected_room)
            .map(|r| r.node)
            .or(app.node_filter)
            .unwrap_or(0);
        let operator_ok = app.can(node, crate::model::Role::Operator);
        hint("x".into(), "end/kick", operator_ok);
        hint("u".into(), "retrieve", operator_ok);
    }
    if app.tab == Tab::Calls {
        // Action keys grey out per the focused call's node.
        let focus_node = app.focus().map(|(n, _)| n);
        let operator_ok = focus_node.is_some_and(|n| app.can(n, crate::model::Role::Operator));
        let admin_node = app.node_filter.or(focus_node).unwrap_or(0);
        if app.ladder.open {
            // The overlay owns j/k while it is up, so saying "select"
            // here would be a lie about what the key does.
            hint("j/k".into(), "scroll", true);
            hint("⏎".into(), "expand", true);
            hint("y".into(), "copy", true);
            hint("s".into(), "close sip", true);
        } else {
            hint("j/k".into(), "select", true);
            // Reading raw SIP needs operator, so it greys out for a
            // readonly token like every other gated key — but it is
            // listed either way, because a key nobody can see is a
            // feature nobody uses.
            hint("s".into(), "sip", operator_ok);
        }
        hint("x".into(), "hangup", operator_ok);
        hint("p".into(), "park", operator_ok);
        hint("u".into(), "retrieve", operator_ok);
        hint("c".into(), "conf", operator_ok);
        hint(
            "o".into(),
            "originate",
            app.can(admin_node, crate::model::Role::Admin),
        );
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

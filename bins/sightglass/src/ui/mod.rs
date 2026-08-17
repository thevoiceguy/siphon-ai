//! Rendering. `draw` is a pure function of `(App, Theme)` — all state
//! lives in the model, which is what lets the `TestBackend` tests at
//! the bottom render fixtures without a real terminal.

mod calls;
mod chrome;
mod overview;
pub mod theme;
mod trunks;

use ratatui::Frame;

use crate::model::{App, Tab};
pub use theme::Theme;

pub fn draw(frame: &mut Frame, app: &App, theme: &Theme) {
    let body = chrome::draw(frame, app, theme);
    match app.tab {
        Tab::Overview => overview::draw(frame, app, theme, body),
        Tab::Trunks => trunks::draw(frame, app, theme, body),
        Tab::Calls => calls::draw(frame, app, theme, body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Msg, NodeSnapshot};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use siphon_ai_admin_api_types::{AdminCallRow, DrainStatus, RegistrationRow};

    /// Fleet fixture per DESIGN_SIGHTGLASS.md §11: two nodes, one
    /// down, call-id collision across nodes.
    fn fixture_app() -> App {
        let mut app = App::new(vec!["prod-1".into(), "prod-2".into()], false);
        app.update(Msg::Snapshot {
            node: 0,
            result: Ok(NodeSnapshot {
                calls: vec![
                    AdminCallRow {
                        call_id: "siphon-aa11".into(),
                        sip_call_id: "aa11@pbx".into(),
                        direction: "inbound".into(),
                    },
                    AdminCallRow {
                        call_id: "siphon-bb22".into(),
                        sip_call_id: "bb22@pbx".into(),
                        direction: "outbound".into(),
                    },
                ],
                registrations: vec![RegistrationRow {
                    name: "pbx-a".into(),
                    server_addr: "10.0.0.9:5060".into(),
                    status: "registered".into(),
                    last_attempt_at: None,
                    expires_at: Some("2026-08-17T00:05:00Z".into()),
                    last_error: None,
                }],
                drain: DrainStatus {
                    draining: false,
                    active_calls: 2,
                    drain_timeout_secs: 30,
                    remaining_secs: None,
                },
            }),
        });
        app.update(Msg::Snapshot {
            node: 1,
            result: Err("connection refused".into()),
        });
        app.update(Msg::Tick);
        app
    }

    fn render(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::new(false);
        terminal.draw(|f| draw(f, app, &theme)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn overview_shows_fleet_grid_with_down_node() {
        let app = fixture_app();
        let screen = render(&app, 100, 30);
        assert!(screen.contains("prod-1"), "{screen}");
        assert!(screen.contains("prod-2"), "{screen}");
        assert!(screen.contains("connection refused"), "{screen}");
        // Fleet rollup in the header: one of two nodes up, two calls.
        assert!(screen.contains("1/2 up"), "{screen}");
        assert!(screen.contains("2 calls"), "{screen}");
    }

    #[test]
    fn calls_tab_lists_both_id_namespaces_with_node_column() {
        let mut app = fixture_app();
        app.tab = Tab::Calls;
        let screen = render(&app, 100, 30);
        assert!(screen.contains("siphon-aa11"), "{screen}");
        assert!(screen.contains("aa11@pbx"), "{screen}");
        assert!(screen.contains("outbound"), "{screen}");
        // Multi-node fleet → Node column present.
        assert!(screen.contains("NODE"), "{screen}");
    }

    #[test]
    fn single_node_fleet_hides_node_column() {
        let mut app = App::new(vec!["solo".into()], false);
        app.tab = Tab::Calls;
        let screen = render(&app, 100, 30);
        assert!(!screen.contains("NODE"), "{screen}");
    }

    #[test]
    fn trunks_tab_renders_registration_rows() {
        let mut app = fixture_app();
        app.tab = Tab::Trunks;
        let screen = render(&app, 100, 30);
        assert!(screen.contains("pbx-a"), "{screen}");
        assert!(screen.contains("registered"), "{screen}");
        assert!(screen.contains("10.0.0.9:5060"), "{screen}");
    }

    #[test]
    fn narrow_terminal_still_renders_every_tab() {
        // Layout must degrade, not panic or overflow (§7).
        for tab in Tab::ALL {
            let mut app = fixture_app();
            app.tab = tab;
            let screen = render(&app, 60, 20);
            assert!(screen.contains("sightglass"), "{tab:?}: {screen}");
        }
    }

    #[test]
    fn read_only_flag_shows_in_header() {
        let mut app = fixture_app();
        app.read_only = true;
        let screen = render(&app, 100, 30);
        assert!(screen.contains("read-only"), "{screen}");
    }

    #[test]
    fn node_filter_scopes_calls_tab_and_shows_chip() {
        let mut app = fixture_app();
        app.tab = Tab::Calls;
        app.cycle_node_filter();
        app.cycle_node_filter(); // -> prod-2 (down, no calls)
        let screen = render(&app, 100, 30);
        assert!(screen.contains("node: prod-2"), "{screen}");
        assert!(!screen.contains("siphon-aa11"), "{screen}");
    }
}

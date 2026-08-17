//! Rendering. `draw` is a pure function of `(App, Theme)` — all state
//! lives in the model, which is what lets the `TestBackend` tests at
//! the bottom render fixtures without a real terminal.

mod calls;
mod chrome;
mod errors;
mod modal;
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
        Tab::Errors => errors::draw(frame, app, theme, body),
    }
    // Overlays last: modal + toasts sit above the tab content.
    modal::draw(frame, app, theme);
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
                errors: None,
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
    fn errors_tab_renders_merged_tail_with_unavailable_note() {
        use siphon_ai_admin_api_types::ErrorEntry;
        let mut app = fixture_app();
        // prod-1 (up) serves the ring; prod-2 is down. Add a third
        // up-node without the endpoint via a manual snapshot on
        // prod-1's shape is overkill — instead flip prod-1's errors.
        app.nodes[0].errors = Some(vec![ErrorEntry {
            ts_ms: 1_786_995_158_000,
            level: "warn".into(),
            target: "siphon_ai_bridge::conn".into(),
            message: "server_too_slow deadline=5s".into(),
            call_id: Some("siphon-aa11".into()),
        }]);
        app.tab = Tab::Errors;
        let screen = render(&app, 110, 30);
        assert!(screen.contains("errors (1)"), "{screen}");
        assert!(screen.contains("19:32:38"), "{screen}");
        assert!(screen.contains("warn"), "{screen}");
        assert!(screen.contains("server_too_slow"), "{screen}");
        assert!(screen.contains("siphon-aa11"), "{screen}");

        // An up node without the endpoint gets the unavailable note.
        app.nodes[0].errors = None;
        let screen = render(&app, 110, 30);
        assert!(screen.contains("unavailable on: prod-1"), "{screen}");
    }

    #[test]
    fn confirm_modal_names_the_node() {
        use crate::keys;
        use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let mut app = fixture_app();
        app.tab = Tab::Calls;
        app.select_first();
        keys::handle(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        );
        let screen = render(&app, 100, 30);
        assert!(screen.contains("confirm"), "{screen}");
        assert!(screen.contains("on prod-1?"), "{screen}");
    }

    #[test]
    fn originate_form_renders_fields_and_node_title() {
        use crate::model::{Field, InputKind, InputModal, Modal};
        let mut app = fixture_app();
        app.tab = Tab::Calls;
        app.modal = Some(Modal::Input(InputModal {
            kind: InputKind::Originate,
            node: 1,
            call_id: None,
            fields: vec![
                Field::required("to (number/user)"),
                Field::required("gateway"),
                Field::optional("ws_url (blank = default)"),
            ],
            active: 0,
        }));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("originate on prod-2"), "{screen}");
        assert!(screen.contains("gateway"), "{screen}");
        assert!(screen.contains("enter submit"), "{screen}");
    }

    #[test]
    fn toasts_render_bottom_right() {
        let mut app = fixture_app();
        app.push_toast("hangup abc on prod-1: accepted (200)".into(), true);
        let screen = render(&app, 100, 30);
        assert!(screen.contains("accepted (200)"), "{screen}");
    }

    #[test]
    fn footer_greys_action_keys_for_readonly_token() {
        use crate::model::{Msg, Role};
        let mut app = fixture_app();
        app.tab = Tab::Calls;
        app.select_first();
        app.update(Msg::RoleLearned {
            node: 0,
            role: Role::ReadOnly,
        });
        let screen = render(&app, 110, 30);
        // Greyed hints carry the ✗ marker; enabled ones don't.
        assert!(screen.contains("hangup✗"), "{screen}");
        assert!(screen.contains("originate✗"), "{screen}");

        app.update(Msg::RoleLearned {
            node: 0,
            role: Role::Operator,
        });
        let screen = render(&app, 110, 30);
        assert!(!screen.contains("hangup✗"), "{screen}");
        assert!(
            screen.contains("originate✗"),
            "operator can't originate: {screen}"
        );
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

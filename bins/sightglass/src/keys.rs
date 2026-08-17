//! Keyboard dispatch: crossterm key events → [`App`] mutations, and
//! optionally an [`Action`] for the main loop to dispatch over HTTP.
//!
//! Kept apart from the draw code so the keymap is unit-testable. When
//! a modal is open, every key routes to it — `q` must type a letter
//! in a form, not quit the app.

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::model::{Action, App, Field, InputKind, InputModal, Modal, Role, Tab};

pub fn handle(app: &mut App, event: &Event) -> Option<Action> {
    let Event::Key(key) = event else { return None };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if app.modal.is_some() {
        return modal_key(app, key);
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Tab => app.tab = app.tab.next(),
        KeyCode::BackTab => app.tab = app.tab.prev(),
        KeyCode::Char('1') => app.tab = Tab::Overview,
        KeyCode::Char('2') => app.tab = Tab::Trunks,
        KeyCode::Char('3') => app.tab = Tab::Calls,
        KeyCode::Char('n') => app.cycle_node_filter(),
        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
        KeyCode::Char('g') | KeyCode::Home => app.select_first(),
        KeyCode::Char('G') | KeyCode::End => app.select_last(),

        // ── Actions (Calls tab; DESIGN_SIGHTGLASS.md §5) ──
        KeyCode::Char('x') if app.tab == Tab::Calls => {
            open_confirm(app, |node, call_id| Action::Hangup { node, call_id })
        }
        KeyCode::Char('p') if app.tab == Tab::Calls => {
            open_confirm(app, |node, call_id| Action::Park { node, call_id })
        }
        KeyCode::Char('u') if app.tab == Tab::Calls => open_call_form(
            app,
            InputKind::Retrieve,
            vec![Field::optional("ws_url (blank = original)")],
        ),
        KeyCode::Char('c') if app.tab == Tab::Calls => open_call_form(
            app,
            InputKind::AddToConference,
            vec![Field::required("room id")],
        ),
        KeyCode::Char('o') if app.tab == Tab::Calls => open_originate(app),
        _ => {}
    }
    None
}

/// Guard an action key: `--read-only` and a known-insufficient role
/// block with a toast instead of opening the modal.
fn guard(app: &mut App, node: usize, required: Role) -> bool {
    if app.read_only {
        app.push_toast("read-only mode — actions disabled".into(), false);
        return false;
    }
    if !app.can(node, required) {
        let node_name = &app.nodes[node].name;
        app.push_toast(
            format!("{node_name}: token role too low (needs {required:?})"),
            false,
        );
        return false;
    }
    true
}

/// Open a yes/no confirm for an action on the focused call. The
/// prompt always names the node — a fleet operator must never act on
/// the wrong box.
fn open_confirm(app: &mut App, build: impl Fn(usize, String) -> Action) {
    let Some((node, call_id)) = app.focus() else {
        app.push_toast("no call selected".into(), false);
        return;
    };
    let action = build(node, call_id.clone());
    if !guard(app, node, action.required_role()) {
        return;
    }
    let prompt = format!(
        "{} {} on {}? (y/n)",
        action.verb(),
        call_id,
        app.nodes[node].name
    );
    app.modal = Some(Modal::Confirm { action, prompt });
}

/// Open an input form bound to the focused call (retrieve /
/// conference-add).
fn open_call_form(app: &mut App, kind: InputKind, fields: Vec<Field>) {
    let Some((node, call_id)) = app.focus() else {
        app.push_toast("no call selected".into(), false);
        return;
    };
    if !guard(app, node, Role::Operator) {
        return;
    }
    app.modal = Some(Modal::Input(InputModal {
        kind,
        node,
        call_id: Some(call_id),
        fields,
        active: 0,
    }));
}

/// Open the originate form. Not bound to a call: the target node is
/// the node filter if set, else the focused call's node, else node 0.
fn open_originate(app: &mut App) {
    let node = app
        .node_filter
        .or_else(|| app.focus().map(|(n, _)| n))
        .unwrap_or(0);
    if !guard(app, node, Role::Admin) {
        return;
    }
    app.modal = Some(Modal::Input(InputModal {
        kind: InputKind::Originate,
        node,
        call_id: None,
        fields: vec![
            Field::required("to (number/user)"),
            Field::required("gateway"),
            Field::optional("ws_url (blank = default)"),
        ],
        active: 0,
    }));
}

/// Keys while a modal is open.
fn modal_key(app: &mut App, key: &KeyEvent) -> Option<Action> {
    match app.modal.take()? {
        Modal::Confirm { action, prompt } => match key.code {
            KeyCode::Char('y') | KeyCode::Enter => Some(action),
            KeyCode::Char('n') | KeyCode::Esc => None,
            _ => {
                // Any other key keeps the modal up.
                app.modal = Some(Modal::Confirm { action, prompt });
                None
            }
        },
        Modal::Input(mut form) => match key.code {
            KeyCode::Esc => None,
            KeyCode::Enter => match form.submit() {
                Ok(action) => Some(action),
                Err(msg) => {
                    app.push_toast(msg, false);
                    app.modal = Some(Modal::Input(form));
                    None
                }
            },
            KeyCode::Tab | KeyCode::Down => {
                form.active = (form.active + 1) % form.fields.len();
                app.modal = Some(Modal::Input(form));
                None
            }
            KeyCode::BackTab | KeyCode::Up => {
                form.active = (form.active + form.fields.len() - 1) % form.fields.len();
                app.modal = Some(Modal::Input(form));
                None
            }
            KeyCode::Backspace => {
                form.fields[form.active].value.pop();
                app.modal = Some(Modal::Input(form));
                None
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                form.fields[form.active].value.push(c);
                app.modal = Some(Modal::Input(form));
                None
            }
            _ => {
                app.modal = Some(Modal::Input(form));
                None
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Msg, NodeSnapshot};
    use ratatui::crossterm::event::KeyEvent;
    use siphon_ai_admin_api_types::{AdminCallRow, DrainStatus, RegistrationRow};

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn snapshot(calls: Vec<AdminCallRow>) -> NodeSnapshot {
        NodeSnapshot {
            calls,
            registrations: Vec::<RegistrationRow>::new(),
            drain: DrainStatus {
                draining: false,
                active_calls: 0,
                drain_timeout_secs: 30,
                remaining_secs: None,
            },
        }
    }

    fn call(id: &str) -> AdminCallRow {
        AdminCallRow {
            call_id: id.to_string(),
            sip_call_id: format!("{id}@host"),
            direction: "inbound".to_string(),
        }
    }

    /// Two-node app, calls tab, one call on prod-2 focused.
    fn app_on_calls() -> App {
        let mut app = App::new(vec!["prod-1".into(), "prod-2".into()], false);
        app.update(Msg::Snapshot {
            node: 1,
            result: Ok(snapshot(vec![call("abc")])),
        });
        app.tab = Tab::Calls;
        app.select_first();
        app
    }

    #[test]
    fn tab_keys_cycle_and_jump() {
        let mut app = App::new(vec!["a".into()], false);
        handle(&mut app, &press(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Trunks);
        handle(&mut app, &press(KeyCode::Char('3')));
        assert_eq!(app.tab, Tab::Calls);
        handle(&mut app, &press(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Trunks);
    }

    #[test]
    fn q_and_ctrl_c_quit() {
        let mut app = App::new(vec!["a".into()], false);
        handle(&mut app, &press(KeyCode::Char('q')));
        assert!(app.should_quit);

        let mut app = App::new(vec!["a".into()], false);
        handle(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );
        assert!(app.should_quit);
    }

    #[test]
    fn release_events_are_ignored() {
        let mut app = App::new(vec!["a".into()], false);
        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        handle(&mut app, &Event::Key(release));
        assert!(!app.should_quit);
    }

    #[test]
    fn hangup_flows_through_node_named_confirm() {
        let mut app = app_on_calls();
        assert!(handle(&mut app, &press(KeyCode::Char('x'))).is_none());
        let Some(Modal::Confirm { prompt, .. }) = &app.modal else {
            panic!("expected confirm modal, got {:?}", app.modal);
        };
        assert!(prompt.contains("on prod-2"), "{prompt}");

        // Confirm dispatches the node-scoped action and closes.
        let action = handle(&mut app, &press(KeyCode::Char('y'))).expect("action");
        assert_eq!(
            action,
            Action::Hangup {
                node: 1,
                call_id: "abc".into()
            }
        );
        assert!(app.modal.is_none());
    }

    #[test]
    fn confirm_cancels_on_esc() {
        let mut app = app_on_calls();
        handle(&mut app, &press(KeyCode::Char('x')));
        assert!(handle(&mut app, &press(KeyCode::Esc)).is_none());
        assert!(app.modal.is_none());
        assert!(!app.should_quit, "esc must cancel the modal, not quit");
    }

    #[test]
    fn read_only_blocks_actions_with_toast() {
        let mut app = app_on_calls();
        app.read_only = true;
        assert!(handle(&mut app, &press(KeyCode::Char('x'))).is_none());
        assert!(app.modal.is_none());
        assert!(app.toasts.iter().any(|t| t.text.contains("read-only")));
    }

    #[test]
    fn known_low_role_blocks_with_toast() {
        let mut app = app_on_calls();
        app.update(Msg::RoleLearned {
            node: 1,
            role: Role::ReadOnly,
        });
        assert!(handle(&mut app, &press(KeyCode::Char('x'))).is_none());
        assert!(app.modal.is_none());
        assert!(app.toasts.iter().any(|t| t.text.contains("prod-2")));

        // Operator suffices for hangup but not originate.
        app.toasts.clear();
        app.update(Msg::RoleLearned {
            node: 1,
            role: Role::Operator,
        });
        handle(&mut app, &press(KeyCode::Char('x')));
        assert!(app.modal.is_some(), "operator may hang up");
        app.modal = None;
        handle(&mut app, &press(KeyCode::Char('o')));
        assert!(app.modal.is_none(), "operator may not originate");
    }

    #[test]
    fn originate_form_types_submits_and_validates() {
        let mut app = app_on_calls();
        app.update(Msg::RoleLearned {
            node: 1,
            role: Role::Admin,
        });
        handle(&mut app, &press(KeyCode::Char('o')));
        assert!(matches!(app.modal, Some(Modal::Input(_))));

        // Submit with empty required fields → toast, modal stays.
        assert!(handle(&mut app, &press(KeyCode::Enter)).is_none());
        assert!(app.modal.is_some());
        assert!(app.toasts.iter().any(|t| t.text.contains("required")));

        // Type destination, tab to gateway, type, submit.
        for c in "100".chars() {
            handle(&mut app, &press(KeyCode::Char(c)));
        }
        handle(&mut app, &press(KeyCode::Tab));
        for c in "twilio".chars() {
            handle(&mut app, &press(KeyCode::Char(c)));
        }
        let action = handle(&mut app, &press(KeyCode::Enter)).expect("action");
        assert_eq!(
            action,
            Action::Originate {
                node: 1,
                to: "100".into(),
                gateway: "twilio".into(),
                ws_url: None
            }
        );
        assert!(app.modal.is_none());
    }

    #[test]
    fn typing_q_in_a_form_does_not_quit() {
        let mut app = app_on_calls();
        handle(&mut app, &press(KeyCode::Char('c'))); // conference form
        assert!(matches!(app.modal, Some(Modal::Input(_))));
        handle(&mut app, &press(KeyCode::Char('q')));
        assert!(!app.should_quit);
        let Some(Modal::Input(form)) = &app.modal else {
            panic!("form gone");
        };
        assert_eq!(form.fields[0].value, "q");
    }

    #[test]
    fn retrieve_with_blank_ws_url_is_none() {
        let mut app = app_on_calls();
        handle(&mut app, &press(KeyCode::Char('u')));
        let action = handle(&mut app, &press(KeyCode::Enter)).expect("action");
        assert_eq!(
            action,
            Action::Retrieve {
                node: 1,
                call_id: "abc".into(),
                ws_url: None
            }
        );
    }
}

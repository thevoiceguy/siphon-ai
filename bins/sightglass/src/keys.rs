//! Keyboard dispatch: crossterm key events → [`App`] mutations, and
//! optionally an [`Action`] for the main loop to dispatch over HTTP.
//!
//! Kept apart from the draw code so the keymap is unit-testable. When
//! a modal is open, every key routes to it — `q` must type a letter
//! in a form, not quit the app.

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::model::{Action, App, Field, InputKind, InputModal, Modal, Role, RoomsRowKind, Tab};

pub fn handle(app: &mut App, event: &Event) -> Option<Action> {
    let Event::Key(key) = event else { return None };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if app.modal.is_some() {
        return modal_key(app, key);
    }
    // The SIP ladder overlay owns navigation while it is open: j/k
    // must scroll the messages, not move the call selection under it.
    // Esc closes the overlay rather than quitting — the same "one
    // layer at a time" rule modals follow above.
    if app.ladder.open && app.tab == Tab::Calls {
        match key.code {
            KeyCode::Esc => {
                if app.ladder.expanded {
                    app.ladder.expanded = false;
                } else {
                    app.toggle_ladder();
                }
                return None;
            }
            KeyCode::Char('s') => {
                app.toggle_ladder();
                return None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                app.ladder_select_next();
                return None;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.ladder_select_prev();
                return None;
            }
            KeyCode::Enter => {
                app.ladder.expanded = !app.ladder.expanded;
                return None;
            }
            KeyCode::Char('y') => {
                copy_focused_message(app);
                return None;
            }
            _ => {}
        }
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
        KeyCode::Char('4') => app.tab = Tab::Rooms,
        KeyCode::Char('5') => app.tab = Tab::Errors,
        KeyCode::Char('6') => app.tab = Tab::System,
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
        // Read-only view of the focused call's signaling — no
        // confirm modal and no node-named prompt, because it changes
        // nothing on the node. The 403 case is a note in the pane,
        // not a blocked keypress: the role probe can be stale, and
        // the daemon is the authority.
        KeyCode::Char('s') if app.tab == Tab::Calls => app.toggle_ladder(),
        KeyCode::Char('x') if app.tab == Tab::Rooms => rooms_destroy(app),
        KeyCode::Char('u') if app.tab == Tab::Rooms => rooms_retrieve(app),
        KeyCode::Char('L') if app.tab == Tab::System => system_log_filter(app),
        KeyCode::Char('H') if app.tab == Tab::System => system_confirm(
            app,
            |node| Action::HepProbe { node },
            "emit HEP probe packet",
        ),
        KeyCode::Char('D') if app.tab == Tab::System => system_confirm(
            app,
            |node| Action::StartDrain { node },
            "START GRACEFUL DRAIN (stops taking calls!)",
        ),
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

/// `x` on the Rooms tab: end the focused room, kick the focused
/// member, or hang up the focused parked call — always via a
/// node-named confirm.
fn rooms_destroy(app: &mut App) {
    let Some(row) = app.visible_rooms().get(app.selected_room).cloned() else {
        app.push_toast("nothing selected".into(), false);
        return;
    };
    if !guard(app, row.node, Role::Operator) {
        return;
    }
    let node_name = app.nodes[row.node].name.clone();
    let (action, what) = match row.kind {
        RoomsRowKind::Room { room_id, .. } => (
            Action::EndConference {
                node: row.node,
                room_id: room_id.clone(),
            },
            format!("end room {room_id}"),
        ),
        RoomsRowKind::Participant { room_id, call_id } => (
            Action::RemoveParticipant {
                node: row.node,
                room_id: room_id.clone(),
                call_id: call_id.clone(),
            },
            format!("kick {call_id} from {room_id}"),
        ),
        RoomsRowKind::Parked { call_id, .. } => (
            Action::Hangup {
                node: row.node,
                call_id: call_id.clone(),
            },
            format!("hangup parked {call_id}"),
        ),
    };
    let prompt = format!("{what} on {node_name}? (y/n)");
    app.modal = Some(Modal::Confirm { action, prompt });
}

/// `u` on the Rooms tab: retrieve the focused *parked* call (optional
/// new ws_url), same form as the Calls tab.
fn rooms_retrieve(app: &mut App) {
    let Some(row) = app.visible_rooms().get(app.selected_room).cloned() else {
        app.push_toast("nothing selected".into(), false);
        return;
    };
    let RoomsRowKind::Parked { call_id, .. } = row.kind else {
        app.push_toast("select a parked call to retrieve".into(), false);
        return;
    };
    if !guard(app, row.node, Role::Operator) {
        return;
    }
    app.modal = Some(Modal::Input(InputModal {
        kind: InputKind::Retrieve,
        node: row.node,
        call_id: Some(call_id),
        fields: vec![Field::optional("ws_url (blank = original)")],
        active: 0,
    }));
}

/// The System tab's focused node, RBAC-guarded (all System actions
/// are admin-level).
fn system_target(app: &mut App) -> Option<usize> {
    let node = *app.visible_system_nodes().get(app.selected_system)?;
    guard(app, node, Role::Admin).then_some(node)
}

fn system_confirm(app: &mut App, build: impl Fn(usize) -> Action, what: &str) {
    let Some(node) = system_target(app) else {
        return;
    };
    let prompt = format!("{what} on {}? (y/n)", app.nodes[node].name);
    app.modal = Some(Modal::Confirm {
        action: build(node),
        prompt,
    });
}

fn system_log_filter(app: &mut App) {
    let Some(node) = system_target(app) else {
        return;
    };
    app.modal = Some(Modal::Input(InputModal {
        kind: InputKind::LogFilter,
        node,
        call_id: None,
        fields: vec![Field::required("directive (e.g. siphon_ai=debug)")],
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

/// Copy the focused SIP message to the system clipboard using OSC 52,
/// the terminal escape for exactly this.
///
/// Deliberately not a clipboard crate: CLAUDE.md keeps the dep tree
/// small on purpose, and OSC 52 works over SSH — which is where
/// sightglass usually runs, and where a local-clipboard library
/// cannot help anyway. Terminals that don't implement it ignore the
/// sequence, so the toast says "sent" rather than claiming success we
/// can't observe.
fn copy_focused_message(app: &mut App) {
    let Some(msg) = app.ladder.selected_message() else {
        app.push_toast("nothing to copy".into(), false);
        return;
    };
    let payload = msg.payload.clone();
    let bytes = payload.len();
    print!("\x1b]52;c;{}\x07", base64_encode(payload.as_bytes()));
    use std::io::Write;
    let _ = std::io::stdout().flush();
    app.push_toast(format!("sent {bytes} B to clipboard (OSC 52)"), true);
}

/// Minimal base64 for OSC 52. Twenty lines beats a dependency for the
/// one place this crate needs it.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(TABLE[(n >> 18 & 63) as usize] as char);
        out.push(TABLE[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
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
            conferences: vec![],
            parked: vec![],
            status: None,
            log_filter: None,
            recent_cdrs: None,
            errors: None,
            trunks: vec![],
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
            result: Ok(Box::new(snapshot(vec![call("abc")]))),
        });
        app.tab = Tab::Calls;
        app.select_first();
        app
    }

    // ─── SIP ladder keys (DESIGN_SIP_LADDER.md §5) ──────────────

    #[test]
    fn s_toggles_the_ladder_only_on_the_calls_tab() {
        let mut app = app_on_calls();
        handle(&mut app, &press(KeyCode::Char('s')));
        assert!(app.ladder.open);
        handle(&mut app, &press(KeyCode::Char('s')));
        assert!(!app.ladder.open);

        app.tab = Tab::Trunks;
        handle(&mut app, &press(KeyCode::Char('s')));
        assert!(!app.ladder.open, "s is a calls-tab key");
    }

    // The overlay owns navigation while open: j/k must scroll
    // messages rather than move the call selection out from under it.
    #[test]
    fn open_ladder_captures_navigation_keys() {
        let mut app = app_on_calls();
        app.update(Msg::Snapshot {
            node: 1,
            result: Ok(Box::new(snapshot(vec![call("abc"), call("def")]))),
        });
        app.select_first();
        let before = app.selected_call;
        handle(&mut app, &press(KeyCode::Char('s')));
        handle(&mut app, &press(KeyCode::Char('j')));
        assert_eq!(
            app.selected_call, before,
            "j scrolls the ladder, not the call table"
        );

        // Closed again, j moves the table as usual.
        handle(&mut app, &press(KeyCode::Char('s')));
        handle(&mut app, &press(KeyCode::Char('j')));
        assert_ne!(app.selected_call, before);
    }

    #[test]
    fn esc_closes_the_ladder_before_it_quits_the_app() {
        let mut app = app_on_calls();
        handle(&mut app, &press(KeyCode::Char('s')));
        handle(&mut app, &press(KeyCode::Enter));
        assert!(app.ladder.expanded);

        handle(&mut app, &press(KeyCode::Esc));
        assert!(!app.ladder.expanded, "first esc collapses");
        assert!(app.ladder.open, "and does not close the pane");
        assert!(!app.should_quit);

        handle(&mut app, &press(KeyCode::Esc));
        assert!(!app.ladder.open, "second esc closes the pane");
        assert!(!app.should_quit, "and still does not quit");

        handle(&mut app, &press(KeyCode::Esc));
        assert!(app.should_quit, "only then does esc quit");
    }

    // Read-only mode blocks *actions*; the ladder changes nothing on
    // the node, so it stays available. The daemon's 403 is the real
    // gate, and it renders inside the pane.
    #[test]
    fn ladder_opens_in_read_only_mode() {
        let mut app = app_on_calls();
        app.read_only = true;
        handle(&mut app, &press(KeyCode::Char('s')));
        assert!(app.ladder.open);
        assert!(app.toasts.is_empty(), "no 'blocked' toast for a read");
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // CRLF and non-ASCII survive — SIP payloads carry both.
        assert_eq!(base64_encode(b"a\r\nb"), "YQ0KYg==");
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
    fn rooms_x_ends_room_kicks_member_and_u_retrieves_parked() {
        use siphon_ai_admin_api_types::{ConferenceRow, ParkedRow};
        let mut app = App::new(vec!["prod-1".into(), "prod-2".into()], false);
        app.update(Msg::Snapshot {
            node: 1,
            result: Ok(Box::new(NodeSnapshot {
                conferences: vec![ConferenceRow {
                    room_id: "room-9".into(),
                    sample_rate: 8000,
                    participants: vec!["siphon-m".into()],
                }],
                parked: vec![ParkedRow {
                    call_id: "siphon-pk".into(),
                    slot: None,
                    parked_secs: 3,
                }],
                ..snapshot(vec![])
            })),
        });
        app.tab = Tab::Rooms;

        // Row 0 = the room → x proposes ending it, node-named.
        app.select_first();
        handle(&mut app, &press(KeyCode::Char('x')));
        let Some(Modal::Confirm { prompt, .. }) = &app.modal else {
            panic!("expected confirm, got {:?}", app.modal);
        };
        assert!(prompt.contains("end room room-9 on prod-2"), "{prompt}");
        let action = handle(&mut app, &press(KeyCode::Char('y'))).expect("action");
        assert_eq!(
            action,
            Action::EndConference {
                node: 1,
                room_id: "room-9".into()
            }
        );

        // Row 1 = the member → x proposes a kick.
        app.select_next();
        handle(&mut app, &press(KeyCode::Char('x')));
        let Some(Modal::Confirm { prompt, .. }) = &app.modal else {
            panic!("expected confirm");
        };
        assert!(prompt.contains("kick siphon-m from room-9"), "{prompt}");
        handle(&mut app, &press(KeyCode::Esc));

        // Row 2 = parked → u opens the retrieve form; submit builds
        // the retrieve for THAT call.
        app.select_next();
        handle(&mut app, &press(KeyCode::Char('u')));
        assert!(matches!(app.modal, Some(Modal::Input(_))));
        let action = handle(&mut app, &press(KeyCode::Enter)).expect("action");
        assert_eq!(
            action,
            Action::Retrieve {
                node: 1,
                call_id: "siphon-pk".into(),
                ws_url: None
            }
        );

        // u on a non-parked row is refused with a toast.
        app.select_first();
        assert!(handle(&mut app, &press(KeyCode::Char('u'))).is_none());
        assert!(app.toasts.iter().any(|t| t.text.contains("parked")));
    }

    #[test]
    fn system_tab_drain_and_log_filter_flow() {
        let mut app = App::new(vec!["prod-1".into(), "prod-2".into()], false);
        app.update(Msg::RoleLearned {
            node: 1,
            role: Role::Admin,
        });
        app.tab = Tab::System;
        app.select_next(); // -> prod-2

        // D: node-named scary confirm building StartDrain for prod-2.
        handle(&mut app, &press(KeyCode::Char('D')));
        let Some(Modal::Confirm { prompt, .. }) = &app.modal else {
            panic!("expected confirm, got {:?}", app.modal);
        };
        assert!(prompt.contains("DRAIN"), "{prompt}");
        assert!(prompt.contains("on prod-2"), "{prompt}");
        let action = handle(&mut app, &press(KeyCode::Enter)).expect("action");
        assert_eq!(action, Action::StartDrain { node: 1 });

        // L: log-filter form, typed directive submits SetLogFilter.
        handle(&mut app, &press(KeyCode::Char('L')));
        assert!(matches!(app.modal, Some(Modal::Input(_))));
        for c in "warn".chars() {
            handle(&mut app, &press(KeyCode::Char(c)));
        }
        let action = handle(&mut app, &press(KeyCode::Enter)).expect("action");
        assert_eq!(
            action,
            Action::SetLogFilter {
                node: 1,
                directive: "warn".into()
            }
        );

        // Operator role can't touch System actions.
        app.update(Msg::RoleLearned {
            node: 1,
            role: Role::Operator,
        });
        assert!(handle(&mut app, &press(KeyCode::Char('H'))).is_none());
        assert!(app.modal.is_none());
        assert!(app.toasts.iter().any(|t| t.text.contains("prod-2")));
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

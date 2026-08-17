//! Keyboard dispatch: crossterm key events → [`App`] mutations.
//!
//! Kept apart from the draw code so the keymap is unit-testable and
//! the PR-2 action keys (`x`/`p`/`u`/`o`, DESIGN_SIGHTGLASS.md §5)
//! have one obvious home.

use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::model::{App, Tab};

pub fn handle(app: &mut App, event: &Event) {
    let Event::Key(key) = event else { return };
    if key.kind != KeyEventKind::Press {
        return;
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
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
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
        use ratatui::crossterm::event::KeyEvent;
        let mut app = App::new(vec!["a".into()], false);
        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        handle(&mut app, &Event::Key(release));
        assert!(!app.should_quit);
    }
}

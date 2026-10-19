use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Quit,
    BackOrQuit,
    ToggleSidebar,
    CycleFocus,
    StartSearch,
    SearchInput(char),
    SearchBackspace,
    CursorLeft,
    CursorRight,
    CursorDown,
    CursorUp,
    PageDown,
    PageUp,
    HalfPageDown,
    HalfPageUp,
    JumpTop,
    JumpBottom,
    NextHunk,
    PrevHunk,
    Select,
    Escape,
    ToggleFold,
    CollapseNode,
    ExpandNode,
    CollapseAll,
    ExpandAll,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingPrefix {
    G,
    Z,
}

#[derive(Debug, Default)]
pub struct InputState {
    pending: Option<PendingPrefix>,
    pending_ctrl_c_at: Option<Instant>,
}

impl InputState {
    pub fn resolve(
        &mut self,
        key: KeyEvent,
        search_active: bool,
        sidebar_focused: bool,
    ) -> Command {
        let now = Instant::now();
        self.resolve_at(key, search_active, sidebar_focused, now)
    }

    fn resolve_at(
        &mut self,
        key: KeyEvent,
        search_active: bool,
        sidebar_focused: bool,
        now: Instant,
    ) -> Command {
        if !matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
        ) {
            self.pending_ctrl_c_at = None;
        }

        if matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
        ) {
            return self.resolve_ctrl_c(sidebar_focused, now);
        }

        if search_active {
            return resolve_search_mode(key);
        }

        if let Some(pending) = self.pending {
            self.pending = None;
            if let Some(command) = resolve_prefixed(pending, key) {
                return command;
            }
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => Command::BackOrQuit,
            (KeyCode::Char('b'), KeyModifiers::NONE) => Command::ToggleSidebar,
            (KeyCode::Tab, _) => Command::CycleFocus,
            (KeyCode::Char('/'), KeyModifiers::NONE) => Command::StartSearch,
            (KeyCode::Enter, _) => Command::Select,
            (KeyCode::Esc, _) => Command::Escape,
            (KeyCode::Char('h'), KeyModifiers::NONE)
            | (KeyCode::Char('H'), KeyModifiers::SHIFT)
            | (KeyCode::Left, _) => Command::CursorLeft,
            (KeyCode::Char('l'), KeyModifiers::NONE)
            | (KeyCode::Char('L'), KeyModifiers::SHIFT)
            | (KeyCode::Right, _) => Command::CursorRight,
            (KeyCode::Char('j'), KeyModifiers::NONE) | (KeyCode::Down, _) => Command::CursorDown,
            (KeyCode::Char('k'), KeyModifiers::NONE) | (KeyCode::Up, _) => Command::CursorUp,
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => Command::HalfPageDown,
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => Command::HalfPageUp,
            (KeyCode::PageDown, _) => Command::PageDown,
            (KeyCode::PageUp, _) => Command::PageUp,
            (KeyCode::Char('g'), KeyModifiers::NONE) => {
                self.pending = Some(PendingPrefix::G);
                Command::None
            }
            (KeyCode::Char('G'), KeyModifiers::SHIFT) => Command::JumpBottom,
            (KeyCode::Char('n'), KeyModifiers::NONE) => Command::NextHunk,
            (KeyCode::Char('N'), KeyModifiers::SHIFT) => Command::PrevHunk,
            (KeyCode::Char('z'), KeyModifiers::NONE) => {
                self.pending = Some(PendingPrefix::Z);
                Command::None
            }
            _ => Command::None,
        }
    }

    fn resolve_ctrl_c(&mut self, sidebar_focused: bool, now: Instant) -> Command {
        const CTRL_C_WINDOW: Duration = Duration::from_secs(1);

        if sidebar_focused {
            self.pending_ctrl_c_at = None;
            return Command::Quit;
        }

        let should_quit = self
            .pending_ctrl_c_at
            .is_some_and(|last| now.saturating_duration_since(last) <= CTRL_C_WINDOW);
        self.pending_ctrl_c_at = Some(now);
        if should_quit {
            self.pending_ctrl_c_at = None;
            Command::Quit
        } else {
            Command::None
        }
    }
}

fn resolve_search_mode(key: KeyEvent) -> Command {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => Command::Escape,
        (KeyCode::Enter, _) => Command::Select,
        (KeyCode::Backspace, _) => Command::SearchBackspace,
        (KeyCode::Up, _) => Command::CursorUp,
        (KeyCode::Down, _) => Command::CursorDown,
        (KeyCode::PageDown, _) => Command::PageDown,
        (KeyCode::PageUp, _) => Command::PageUp,
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => Command::HalfPageDown,
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => Command::HalfPageUp,
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => Command::SearchInput(c),
        _ => Command::None,
    }
}

fn resolve_prefixed(pending: PendingPrefix, key: KeyEvent) -> Option<Command> {
    match (pending, key.code, key.modifiers) {
        (PendingPrefix::G, KeyCode::Char('g'), KeyModifiers::NONE) => Some(Command::JumpTop),
        (PendingPrefix::Z, KeyCode::Char('a'), KeyModifiers::NONE) => Some(Command::ToggleFold),
        (PendingPrefix::Z, KeyCode::Char('M'), KeyModifiers::SHIFT) => Some(Command::CollapseAll),
        (PendingPrefix::Z, KeyCode::Char('R'), KeyModifiers::SHIFT) => Some(Command::ExpandAll),
        (PendingPrefix::Z, KeyCode::Char('h'), KeyModifiers::NONE) => Some(Command::CollapseNode),
        (PendingPrefix::Z, KeyCode::Char('l'), KeyModifiers::NONE) => Some(Command::ExpandNode),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, InputState};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::{Duration, Instant};

    #[test]
    fn maps_vim_navigation_keys() {
        let mut input = InputState::default();

        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
                false,
                true,
            ),
            Command::CursorLeft
        );
        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
                false,
                true,
            ),
            Command::CursorDown
        );
        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
                false,
                true,
            ),
            Command::CursorUp
        );
        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
                false,
                true,
            ),
            Command::CursorRight
        );
        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT),
                false,
                true,
            ),
            Command::CursorLeft
        );
        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT),
                false,
                true,
            ),
            Command::CursorRight
        );
    }

    #[test]
    fn resolves_prefixed_commands() {
        let mut input = InputState::default();

        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
                false,
                true,
            ),
            Command::None
        );
        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
                false,
                true,
            ),
            Command::JumpTop
        );

        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
                false,
                true,
            ),
            Command::None
        );
        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
                false,
                true,
            ),
            Command::ToggleFold
        );
    }

    #[test]
    fn maps_documented_global_keys() {
        let mut input = InputState::default();

        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
                false,
                true,
            ),
            Command::ToggleSidebar
        );
        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT),
                false,
                true,
            ),
            Command::PrevHunk
        );
    }

    #[test]
    fn search_mode_captures_text_input() {
        let mut input = InputState::default();

        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                false,
                true,
            ),
            Command::StartSearch
        );
        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
                true,
                true,
            ),
            Command::SearchInput('m')
        );
        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                true,
                true,
            ),
            Command::SearchBackspace
        );
    }

    #[test]
    fn ctrl_c_quits_immediately_in_sidebar() {
        let mut input = InputState::default();

        assert_eq!(
            input.resolve(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false,
                true,
            ),
            Command::Quit
        );
    }

    #[test]
    fn ctrl_c_requires_double_press_outside_sidebar() {
        let mut input = InputState::default();
        let now = Instant::now();

        assert_eq!(
            input.resolve_at(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false,
                false,
                now,
            ),
            Command::None
        );
        assert_eq!(
            input.resolve_at(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false,
                false,
                now + Duration::from_millis(500),
            ),
            Command::Quit
        );
    }

    #[test]
    fn ctrl_c_window_expires_after_one_second() {
        let mut input = InputState::default();
        let now = Instant::now();

        assert_eq!(
            input.resolve_at(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false,
                false,
                now,
            ),
            Command::None
        );
        assert_eq!(
            input.resolve_at(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false,
                false,
                now + Duration::from_millis(1001),
            ),
            Command::None
        );
    }
}

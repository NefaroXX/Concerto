use iced::keyboard::{key::Named, Key, Modifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    NewTask,
    DiffViewer,
    Memory,
    ToolLog,
    Terminal,
    UndoRun,
    SubmitInput,
    CancelDialog,
    HelpOverlay,
    Screenshot,
    Editor,
    EditorRedo,
    EditorFind,
    EditorReplace,
    EditorGoto,
    EditorFindNext,
    EditorFindPrev,
}

/// Resolve a key event into a shortcut.
/// `text_focused` controls whether we steal typing keys — only Escape and
/// Ctrl+Enter bypass this check.
pub fn resolve(key: &Key, mods: Modifiers, text_focused: bool) -> Option<Shortcut> {
    match key {
        Key::Named(Named::Escape) => return Some(Shortcut::CancelDialog),
        Key::Named(Named::Enter) if mods.control() => return Some(Shortcut::SubmitInput),
        // Ctrl+S for screenshot — bypass text_focused since it's a global shortcut
        Key::Character(ch) if ch.as_str() == "s" && mods.control() && !mods.shift() => {
            return Some(Shortcut::Screenshot);
        }
        // Editor commands bypass text_focused: they must work while the
        // code editor or the find bar owns the keyboard.
        Key::Character(ch) if ch.as_str() == "f" && mods.control() => {
            return Some(Shortcut::EditorFind);
        }
        Key::Character(ch) if ch.as_str() == "h" && mods.control() => {
            return Some(Shortcut::EditorReplace);
        }
        Key::Character(ch) if ch.as_str() == "g" && mods.control() => {
            return Some(Shortcut::EditorGoto);
        }
        Key::Named(Named::F3) if !mods.shift() => return Some(Shortcut::EditorFindNext),
        Key::Named(Named::F3) if mods.shift() => return Some(Shortcut::EditorFindPrev),
        _ => {}
    }
    if text_focused {
        return None;
    }
    match key {
        Key::Character(ch) if ch.as_str() == "t" && mods.control() => Some(Shortcut::NewTask),
        Key::Character(ch) if ch.as_str() == "n" && mods.control() => Some(Shortcut::NewTask),
        Key::Character(ch) if ch.as_str() == "d" && mods.control() => Some(Shortcut::DiffViewer),
        Key::Character(ch) if ch.as_str() == "m" && mods.control() => Some(Shortcut::Memory),
        Key::Character(ch) if ch.as_str() == "l" && mods.control() => Some(Shortcut::ToolLog),
        Key::Character(ch) if ch.as_str() == "`" && mods.control() => Some(Shortcut::Terminal),
        Key::Character(ch) if ch.as_str() == "e" && mods.control() => Some(Shortcut::Editor),
        Key::Character(ch) if ch.as_str() == "z" && mods.control() && !mods.shift() => {
            Some(Shortcut::UndoRun)
        }
        Key::Character(ch) if ch.as_str() == "Z" && mods.control() && mods.shift() => {
            Some(Shortcut::EditorRedo)
        }
        Key::Character(ch) if ch.as_str() == "y" && mods.control() => Some(Shortcut::EditorRedo),
        Key::Character(ch) if ch.as_str() == "?" => Some(Shortcut::HelpOverlay),
        _ => None,
    }
}

/// Resolve shortcuts while the integrated terminal owns keyboard input.
///
/// Shell key combinations must not trigger application navigation. Only the
/// explicit terminal toggle remains global on this page.
pub fn resolve_terminal(key: &Key, mods: Modifiers) -> Option<Shortcut> {
    match key {
        Key::Character(ch) if ch.as_str() == "`" && mods.control() => Some(Shortcut::Terminal),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::key::Named;
    use iced::keyboard::{Key, Modifiers};

    // given: plain Enter without modifiers while text is focused
    // then: resolve returns None — the text_input's on_submit handles it
    #[test]
    fn plain_enter_with_text_focused_returns_none() {
        let result = resolve(&Key::Named(Named::Enter), Modifiers::empty(), true);
        assert_eq!(result, None);
    }

    // given: Ctrl+Enter while text is focused
    // then: resolve returns Some(SubmitInput) — bypasses text_focused check
    #[test]
    fn ctrl_enter_bypasses_text_focused() {
        let ctrl = Modifiers::CTRL;
        let result = resolve(&Key::Named(Named::Enter), ctrl, true);
        assert_eq!(result, Some(Shortcut::SubmitInput));
    }

    // given: Ctrl+Enter while text is NOT focused
    // then: resolve returns Some(SubmitInput)
    #[test]
    fn ctrl_enter_without_text_focused() {
        let ctrl = Modifiers::CTRL;
        let result = resolve(&Key::Named(Named::Enter), ctrl, false);
        assert_eq!(result, Some(Shortcut::SubmitInput));
    }

    #[test]
    fn terminal_panel_only_keeps_terminal_toggle_global() {
        assert_eq!(
            resolve_terminal(&Key::Character("`".into()), Modifiers::CTRL),
            Some(Shortcut::Terminal)
        );
        assert_eq!(resolve_terminal(&Key::Character("t".into()), Modifiers::CTRL), None);
        assert_eq!(resolve_terminal(&Key::Character("?".into()), Modifiers::empty()), None);
    }

    #[test]
    fn ctrl_n_new_task() {
        let result = resolve(&Key::Character("n".into()), Modifiers::CTRL, false);
        assert_eq!(result, Some(Shortcut::NewTask));
    }

    #[test]
    fn escape_returns_cancel_dialog() {
        let result = resolve(&Key::Named(Named::Escape), Modifiers::empty(), false);
        assert_eq!(result, Some(Shortcut::CancelDialog));
    }

    #[test]
    fn escape_bypasses_text_focused() {
        let result = resolve(&Key::Named(Named::Escape), Modifiers::empty(), true);
        assert_eq!(result, Some(Shortcut::CancelDialog));
    }

    #[test]
    fn ctrl_d_diff_viewer() {
        let result = resolve(&Key::Character("d".into()), Modifiers::CTRL, false);
        assert_eq!(result, Some(Shortcut::DiffViewer));
    }

    #[test]
    fn ctrl_l_tool_log() {
        let result = resolve(&Key::Character("l".into()), Modifiers::CTRL, false);
        assert_eq!(result, Some(Shortcut::ToolLog));
    }

    /// Tab with text focused returns None (input captures it).
    #[test]
    fn tab_with_text_focused_returns_none() {
        let result = resolve(&Key::Named(Named::Tab), Modifiers::empty(), true);
        assert_eq!(result, None);
    }

    /// Tab without text focused toggles the sidebar.
    #[test]
    fn tab_without_text_focused_toggles_sidebar() {
        // Alt+Tab is typically window-manager captured, but Tab alone
        // without text focus should be None (no binding for lone Tab).
        let result = resolve(&Key::Named(Named::Tab), Modifiers::empty(), false);
        assert_eq!(result, None, "Tab alone should not be bound");
    }
}

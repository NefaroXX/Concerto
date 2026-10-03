use iced::keyboard::{key::Named, Key, Modifiers};

/// One row of the shortcuts reference modal: the key chord and what it does.
/// Kept next to the resolver so the modal can never drift from the bindings it
/// documents (the modal lists every variant this module can produce).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutInfo {
    pub keys: &'static str,
    pub label: &'static str,
}

/// Every shortcut the desktop app binds, grouped for the modal. Text focus
/// gates most global bindings; `Ctrl+Enter`, `Esc`, the editor commands and
/// the two screenshot chords always apply. Keep this in sync with [`resolve`]
/// — the modal renders it verbatim.
pub const ALL: &[ShortcutInfo] = &[
    ShortcutInfo { keys: "Ctrl+Enter", label: "Send message" },
    ShortcutInfo { keys: "Ctrl+T / Ctrl+N", label: "New task" },
    ShortcutInfo { keys: "Ctrl+D", label: "Toggle Diff viewer" },
    ShortcutInfo { keys: "Ctrl+M", label: "Open Memory explorer" },
    ShortcutInfo { keys: "Ctrl+L", label: "Open Tool Log" },
    ShortcutInfo { keys: "Ctrl+R", label: "Open Runtime panels" },
    ShortcutInfo { keys: "Ctrl+`", label: "Toggle terminal panel" },
    ShortcutInfo { keys: "Ctrl+Z", label: "Undo last run (rollback)" },
    ShortcutInfo { keys: "Ctrl+E", label: "Open code editor" },
    ShortcutInfo { keys: "Ctrl+S", label: "Screenshot (Save in editor)" },
    ShortcutInfo { keys: "Ctrl+Shift+S", label: "Screenshot (all pages)" },
    ShortcutInfo { keys: "Ctrl+F", label: "Editor: find" },
    ShortcutInfo { keys: "Ctrl+H", label: "Editor: find & replace" },
    ShortcutInfo { keys: "Ctrl+G", label: "Editor: go to line" },
    ShortcutInfo { keys: "Ctrl+W", label: "Editor: close active tab" },
    ShortcutInfo { keys: "Ctrl+PageDown / Ctrl+PageUp", label: "Editor: next / previous tab" },
    ShortcutInfo { keys: "F3 / Shift+F3", label: "Editor: find next / previous" },
    ShortcutInfo { keys: "Ctrl+Shift+Z / Ctrl+Y", label: "Editor: redo" },
    ShortcutInfo { keys: "?", label: "Toggle this shortcuts panel" },
    ShortcutInfo { keys: "Esc", label: "Dismiss dialog / overlay" },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    NewTask,
    DiffViewer,
    Memory,
    ToolLog,
    Terminal,
    /// Open/close the per-session Runtime panels modal (the read-only
    /// Coordinator observability panels formerly in the Studio rail).
    RuntimePanels,
    UndoRun,
    SubmitInput,
    CancelDialog,
    HelpOverlay,
    /// `Ctrl+S` — screenshot everywhere *except* the Editor page with an open
    /// file, where it means Save (the editor's own binding).
    Screenshot,
    /// `Ctrl+Shift+S` — screenshot on every page, the Editor included. This
    /// is the chord that keeps a screenshot reachable where `Ctrl+S` is Save.
    ScreenshotAlways,
    Editor,
    EditorRedo,
    EditorFind,
    EditorReplace,
    EditorGoto,
    EditorCloseTab,
    EditorNextTab,
    EditorPreviousTab,
    EditorFindNext,
    EditorFindPrev,
}

/// Resolve a key event into a shortcut.
/// `text_focused` controls whether we steal typing keys — only Escape,
/// Ctrl+Enter and the editor/screenshot chords bypass this check.
pub fn resolve(key: &Key, mods: Modifiers, text_focused: bool) -> Option<Shortcut> {
    match key {
        Key::Named(Named::Escape) => return Some(Shortcut::CancelDialog),
        Key::Named(Named::Enter) if mods.control() => return Some(Shortcut::SubmitInput),
        // Ctrl+Shift+S for screenshot on every page — the Editor's Ctrl+S is
        // Save, so this is the chord that survives there. Case-insensitive:
        // some backends report the shifted character ("S"), others the base
        // one ("s"). Global: bypasses text_focused like Ctrl+S.
        Key::Character(ch)
            if ch.as_str().eq_ignore_ascii_case("s") && mods.control() && mods.shift() =>
        {
            return Some(Shortcut::ScreenshotAlways);
        }
        // Ctrl+S for screenshot — bypass text_focused since it's a global shortcut
        Key::Character(ch) if ch.as_str() == "s" && mods.control() && !mods.shift() => {
            return Some(Shortcut::Screenshot);
        }
        // Editor commands bypass text_focused: they must work while the
        // code editor or the find bar owns the keyboard.
        Key::Character(ch) if ch.as_str().eq_ignore_ascii_case("w") && mods.control() => {
            return Some(Shortcut::EditorCloseTab);
        }
        Key::Named(Named::PageDown) if mods.control() => return Some(Shortcut::EditorNextTab),
        Key::Named(Named::PageUp) if mods.control() => return Some(Shortcut::EditorPreviousTab),
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
        // Ctrl+R opens the per-session Runtime panels ("R" for Runtime). Free:
        // no existing binding uses "r" (Ctrl+S screenshot, Ctrl+T/N task,
        // Ctrl+D diff, Ctrl+M memory, Ctrl+L tool log, Ctrl+` terminal).
        Key::Character(ch) if ch.as_str() == "r" && mods.control() => Some(Shortcut::RuntimePanels),
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

    /// Editor tab commands remain available in text fields and do not consume unmodified typing keys.
    #[test]
    fn editor_tab_shortcuts_bypass_text_focus_only_with_control() {
        for focused in [false, true] {
            assert_eq!(
                resolve(&Key::Character("w".into()), Modifiers::CTRL, focused),
                Some(Shortcut::EditorCloseTab)
            );
            assert_eq!(
                resolve(&Key::Named(Named::PageDown), Modifiers::CTRL, focused),
                Some(Shortcut::EditorNextTab)
            );
            assert_eq!(
                resolve(&Key::Named(Named::PageUp), Modifiers::CTRL, focused),
                Some(Shortcut::EditorPreviousTab)
            );
            assert_eq!(resolve(&Key::Character("w".into()), Modifiers::empty(), focused), None);
            assert_eq!(resolve(&Key::Named(Named::PageDown), Modifiers::empty(), focused), None);
        }
    }
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

    #[test]
    fn ctrl_r_runtime_panels() {
        let result = resolve(&Key::Character("r".into()), Modifiers::CTRL, false);
        assert_eq!(result, Some(Shortcut::RuntimePanels));
    }

    /// Ctrl+Shift+S resolves to the always-screenshot binding regardless of
    /// text focus — it is the chord that stays available on the Editor page,
    /// where Ctrl+S is Save.
    #[test]
    fn ctrl_shift_s_is_the_global_screenshot() {
        let mods = Modifiers::CTRL | Modifiers::SHIFT;
        // Backends disagree on the shifted character; both must resolve.
        for key in ["s", "S"] {
            assert_eq!(
                resolve(&Key::Character(key.into()), mods, false),
                Some(Shortcut::ScreenshotAlways),
                "key {key:?} without text focus"
            );
            assert_eq!(
                resolve(&Key::Character(key.into()), mods, true),
                Some(Shortcut::ScreenshotAlways),
                "key {key:?} with text focus"
            );
        }
    }

    /// Ctrl+S keeps its own binding (Save in the editor, screenshot elsewhere)
    /// so the new chord never steals it.
    #[test]
    fn ctrl_s_keeps_its_own_binding() {
        assert_eq!(
            resolve(&Key::Character("s".into()), Modifiers::CTRL, false),
            Some(Shortcut::Screenshot)
        );
        assert_eq!(
            resolve(&Key::Character("s".into()), Modifiers::CTRL, true),
            Some(Shortcut::Screenshot)
        );
    }

    /// Ctrl+R must not fire while a text field owns the keyboard (same
    /// `text_focused` gate as Ctrl+D/Ctrl+L), so typing "r" in the composer is
    /// never hijacked.
    #[test]
    fn ctrl_r_is_gated_by_text_focus() {
        let result = resolve(&Key::Character("r".into()), Modifiers::CTRL, true);
        assert_eq!(result, None);
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

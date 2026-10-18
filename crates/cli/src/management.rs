//! Structured settings browser and multiline editor over validated config services.
use std::path::{Path, PathBuf};

use concerto_config::settings::{redact_settings, value_at, SettingsEditor, SettingsScope};
use concerto_config::{AppConfig, CustomAgentConfig};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use serde_json::Value;

use crate::theme::CliTheme;

#[derive(Default)]
pub(crate) struct Management {
    project: PathBuf,
    global: PathBuf,
    project_scope: bool,
    root: Value,
    path: Vec<String>,
    selected: usize,
    edit: Option<Draft>,
    pub(crate) status: String,
}

struct Draft {
    path: String,
    text: String,
    cursor: usize,
    plain_string: bool,
    discard: bool,
}

impl Draft {
    fn vertical(&mut self, down: bool) {
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |index| index + 1);
        let column = self.text[start..self.cursor].chars().count();
        let target = if down {
            let Some(end) = self.text[self.cursor..].find('\n') else { return; };
            self.cursor + end + 1
        } else {
            if start == 0 { return; }
            self.text[..start - 1].rfind('\n').map_or(0, |index| index + 1)
        };
        let line = self.text[target..].split('\n').next().unwrap_or("");
        self.cursor = target + line.char_indices().nth(column).map_or(line.len(), |(index, _)| index);
    }
}

fn editor_text(text: &str) -> String {
    text.chars().map(|c| match c {
        '\n' => "\n".into(), '\t' => "    ".into(),
        c if c.is_control() => "�".into(), c => c.to_string(),
    }).collect()
}

fn roster(config: &AppConfig) -> Vec<CustomAgentConfig> {
    if config.owns_agent_roster() {
        config.multi_agent.as_ref().map(|multi| multi.custom_agents.clone()).unwrap_or_default()
    } else {
        concerto_config::builtin_agent_seeds()
    }
}

fn document(global: &PathBuf, project: &Path, project_scope: bool) -> Result<Value, String> {
    let config = if project_scope {
        concerto_config::load_config(Some(global), Some(project))
    } else {
        concerto_config::load_global_config(Some(global))
    }.map_err(|e| e.to_string())?;
    let agents = roster(&config);
    let mut root = serde_json::to_value(config).map_err(|e| e.to_string())?;
    root["agents"] = serde_json::to_value(agents).map_err(|e| e.to_string())?;
    redact_settings(&mut root);
    Ok(root)
}

fn contains_redaction(value: &Value) -> bool {
    match value {
        Value::String(value) => value == "[REDACTED]",
        Value::Array(values) => values.iter().any(contains_redaction),
        Value::Object(values) => values.values().any(contains_redaction),
        _ => false,
    }
}

fn toml_literal(value: &Value) -> Result<String, String> {
    fn omit_null_fields(value: &Value) -> Value {
        match value {
            Value::Object(fields) => Value::Object(fields.iter().filter(|(_, value)| !value.is_null()).map(|(key, value)| (key.clone(), omit_null_fields(value))).collect()),
            Value::Array(values) => Value::Array(values.iter().map(omit_null_fields).collect()),
            value => value.clone(),
        }
    }
    toml::Value::try_from(omit_null_fields(value)).map(|value| value.to_string()).map_err(|e| e.to_string())
}

/// Validates and saves one edit. Roster edits use canonical agent files;
/// configuration edits preserve unrelated comments and keys atomically.
fn save(global: &PathBuf, project: &Path, project_scope: bool, expected: &Value, path: &str, value: Value) -> Result<(), String> {
    let current = document(global, project, project_scope)?;
    if &current != expected {
        return Err("Settings changed since opening this editor. Cancel the draft and refresh before retrying.".into());
    }
    if contains_redaction(&value) {
        return Err("This object contains hidden environment values. Edit an individual non-secret field, or use the CLI configuration commands.".into());
    }
    if let Some(path) = path.strip_prefix("agents.") {
        if project_scope { return Err("Agent files are global only. Switch to global scope.".into()); }
        let mut parts = path.splitn(2, '.');
        let index = parts.next().and_then(|s| s.parse::<usize>().ok()).ok_or("invalid agent selection")?;
        let field = parts.next();
        if field == Some("id") { return Err("Agent identity is immutable; use agents clone.".into()); }
        let config = concerto_config::load_global_config(Some(global)).map_err(|e| e.to_string())?;
        let mut agents = roster(&config);
        let original = agents.get(index).ok_or("agent no longer exists")?;
        let edited: CustomAgentConfig = if let Some(field) = field {
            let raw = toml::to_string_pretty(original).map_err(|e| e.to_string())?;
            let literal = if value.is_null() { None } else { Some(toml_literal(&value)?) };
            let edited = concerto_config::settings::edit_document(&raw, field, literal.as_deref()).map_err(|e| e.to_string())?;
            let supplied: toml::Value = toml::from_str(&edited).map_err(|e| e.to_string())?;
            let typed: CustomAgentConfig = toml::from_str(&edited).map_err(|e| e.to_string())?;
            concerto_config::settings::reject_unknown_keys(&serde_json::to_value(supplied).map_err(|e| e.to_string())?, &serde_json::to_value(&typed).map_err(|e| e.to_string())?, "agent").map_err(|e| e.to_string())?;
            typed
        } else {
            let typed: CustomAgentConfig = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
            concerto_config::settings::reject_unknown_keys(&value, &serde_json::to_value(&typed).map_err(|e| e.to_string())?, "agent").map_err(|e| e.to_string())?;
            typed
        };
        if edited.id != original.id { return Err("Agent identity is immutable; use agents clone.".into()); }
        agents[index] = edited;
        return concerto_config::settings::save_settings_roster(global, &agents).map_err(|e| e.to_string());
    }
    if path == "agents" { return Err("Select an agent or field to edit. Use agents import/clone/remove for roster membership.".into()); }
    let literal = if value.is_null() { None } else { Some(toml_literal(&value)?) };
    SettingsEditor { global_path: global.clone(), project_root: project.to_owned(), scope: if project_scope { SettingsScope::Project } else { SettingsScope::Global } }
        .edit(path, literal.as_deref(), false).map_err(|e| e.to_string())?;
    Ok(())
}


impl Management {
    pub(crate) fn open(&mut self, project: PathBuf) {
        self.project = project;
        self.global = concerto_config::default_config_path().unwrap_or_default();
        self.path.clear();
        self.selected = 0;
        self.edit = None;
        self.refresh();
    }

    fn refresh(&mut self) {
        if self.global.as_os_str().is_empty() { self.status = "Cannot locate the config directory.".into(); return; }
        match document(&self.global, &self.project, self.project_scope) {
            Ok(root) => { self.root = root; self.status = "Applies to the next run; higher-priority overrides still apply.".into(); }
            Err(error) => { self.root = Value::Null; self.status = error; }
        }
        self.selected = self.selected.min(self.rows().len().saturating_sub(1));
    }

    fn rows(&self) -> Vec<(String, &Value)> {
        let node = if self.path.is_empty() { Some(&self.root) } else { value_at(&self.root, &self.path.join(".")) };
        match node {
            Some(Value::Object(values)) => values.iter().map(|(key, value)| (key.clone(), value)).collect(),
            Some(Value::Array(values)) => values.iter().enumerate().map(|(index, value)| (index.to_string(), value)).collect(),
            _ => Vec::new(),
        }
    }

    fn selected(&self) -> Option<(String, Value)> {
        let rows = self.rows();
        let (key, value) = rows.get(self.selected)?;
        let mut path = self.path.clone();
        path.push(key.clone());
        Some((path.join("."), (*value).clone()))
    }

    fn start_edit(&mut self) {
        let Some((path, value)) = self.selected() else { return; };
        if contains_redaction(&value) || path.split('.').any(|part| part == "env") {
            self.status = "Environment values are hidden; use CLI commands to change credential references.".into();
            return;
        }
        let plain_string = value.is_string();
        let text = value.as_str().map(str::to_string).unwrap_or_else(|| serde_json::to_string_pretty(&value).unwrap_or_default());
        self.edit = Some(Draft { path, cursor: text.len(), text, plain_string, discard: false });
        self.status = "Ctrl+S validates and saves. Esc asks before discarding.".into();
    }

    pub(crate) fn paste(&mut self, text: &str) {
        if let Some(draft) = &mut self.edit {
            if draft.discard { return; }
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            let clean: String = text.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect();
            draft.text.insert_str(draft.cursor, &clean);
            draft.cursor += clean.len();
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> bool {
        if self.edit.as_ref().is_some_and(|draft| draft.discard) {
            match key.code {
                KeyCode::Char('y') => { self.edit = None; self.status = "Draft discarded; no settings saved.".into(); }
                KeyCode::Char('n') | KeyCode::Esc => { if let Some(draft) = &mut self.edit { draft.discard = false; } }
                _ => {}
            }
            return false;
        }
        if let Some(draft) = &self.edit {
            if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
                let value = if draft.plain_string { Ok(Value::String(draft.text.clone())) } else { serde_json::from_str(&draft.text).map_err(|e| e.to_string()) };
                let result = value.and_then(|value| save(&self.global, &self.project, self.project_scope, &self.root, &draft.path, value));
                match result {
                    Ok(()) => { self.edit = None; self.refresh(); self.status = "Saved. Applies to the next run.".into(); }
                    Err(error) => self.status = error,
                }
                return false;
            }
            if let Some(draft) = &mut self.edit {
                if key.code == KeyCode::Esc || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
                    draft.discard = true;
                    return false;
                }
                match key.code {
                    KeyCode::Up => draft.vertical(false),
                    KeyCode::Down => draft.vertical(true),
                    KeyCode::Left => draft.cursor = draft.text[..draft.cursor].char_indices().last().map_or(0, |(index, _)| index),
                    KeyCode::Right => draft.cursor += draft.text[draft.cursor..].chars().next().map_or(0, char::len_utf8),
                    KeyCode::Home => draft.cursor = draft.text[..draft.cursor].rfind('\n').map_or(0, |index| index + 1),
                    KeyCode::End => draft.cursor += draft.text[draft.cursor..].find('\n').unwrap_or(draft.text.len() - draft.cursor),
                    KeyCode::Backspace if draft.cursor > 0 => {
                        let start = draft.text[..draft.cursor].char_indices().last().map_or(0, |(index, _)| index);
                        draft.text.replace_range(start..draft.cursor, ""); draft.cursor = start;
                    }
                    KeyCode::Delete if draft.cursor < draft.text.len() => { let end = draft.cursor + draft.text[draft.cursor..].chars().next().map_or(0, char::len_utf8); draft.text.replace_range(draft.cursor..end, ""); }
                    KeyCode::Enter => self.paste("\n"),
                    KeyCode::Tab => self.paste("    "),
                    KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => self.paste(&c.to_string()),
                    _ => {}
                }
            }
            return false;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Backspace if !self.path.is_empty() => { self.path.pop(); self.selected = 0; }
            KeyCode::Esc | KeyCode::Char('q') => return true,
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(self.rows().len().saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.rows().len().saturating_sub(1),
            KeyCode::Enter | KeyCode::Right => {
                if let Some((path, value)) = self.selected() {
                    if value.as_object().is_some_and(|v| !v.is_empty()) || value.as_array().is_some_and(|v| !v.is_empty()) {
                        self.path = path.split('.').map(str::to_string).collect(); self.selected = 0;
                    } else { self.start_edit(); }
                }
            }
            KeyCode::Char('e') => self.start_edit(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('g') => { self.project_scope = !self.project_scope; self.path.clear(); self.selected = 0; self.refresh(); }
            _ => {}
        }
        false
    }

    pub(crate) fn draw(&self, frame: &mut Frame, theme: &CliTheme) {
        let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(" Manage settings ").border_style(Style::default().fg(theme.border));
        let inner = block.inner(frame.area());
        frame.render_widget(block, frame.area());
        let areas = Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(2), Constraint::Length(2)]).split(inner);
        let scope = if self.project_scope { "Project (effective values)" } else { "Global" };
        let path = self.edit.as_ref().map(|draft| draft.path.clone()).unwrap_or_else(|| self.path.join("."));
        frame.render_widget(Paragraph::new(format!("{scope}: {path}")).style(Style::default().fg(theme.user)), areas[0]);
        if let Some(draft) = &self.edit {
            if draft.discard {
                frame.render_widget(Paragraph::new("Discard this draft? y discard | n keep editing").wrap(Wrap { trim: false }).style(Style::default().fg(theme.warning)), areas[1]);
            } else {
                let width = areas[1].width.max(1);
                let prefix = editor_text(&draft.text[..draft.cursor]);
                let rows = prefix.split('\n').count().saturating_sub(1);
                let column = ratatui::text::Line::from(prefix.rsplit('\n').next().unwrap_or("")).width();
                let horizontal = column.saturating_sub(width.saturating_sub(1) as usize);
                let scroll = rows.saturating_sub(areas[1].height.saturating_sub(1) as usize);
                let scroll = u16::try_from(scroll).unwrap_or(u16::MAX);
                frame.render_widget(Paragraph::new(editor_text(&draft.text)).scroll((scroll, u16::try_from(horizontal).unwrap_or(u16::MAX))), areas[1]);
                if areas[1].height > 0 && areas[1].width > 0 {
                    frame.set_cursor_position((areas[1].x + column.saturating_sub(horizontal) as u16, areas[1].y + (rows.saturating_sub(scroll as usize) as u16).min(areas[1].height - 1)));
                }
            }
        } else {
            let rows = self.rows();
            let items: Vec<ListItem> = rows.iter().map(|(key, value)| {
                let preview = match value {
                    Value::Object(map) => format!("{{{} fields}}", map.len()),
                    Value::Array(array) => format!("[{} items]", array.len()),
                    _ => value.to_string().chars().take(100).collect(),
                };
                ListItem::new(format!("{key}: {preview}"))
            }).collect();
            let mut state = ListState::default().with_selected(Some(self.selected));
            frame.render_stateful_widget(List::new(items).highlight_symbol("› ").highlight_style(Style::default().fg(theme.user).add_modifier(Modifier::BOLD)), areas[1], &mut state);
        }
        let status: String = self.status.chars().filter(|c| !c.is_control() || *c == '\n').collect();
        frame.render_widget(Paragraph::new(status).wrap(Wrap { trim: false }).style(Style::default().fg(theme.warning)), areas[2]);
        let help = if self.edit.is_some() { "Ctrl+S save | Esc cancel\nStrings: plain text; other values: JSON" } else { "Enter open | e edit | Esc back\ng global/project | r refresh" };
        frame.render_widget(Paragraph::new(help).style(Style::default().fg(theme.muted)), areas[3]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::{Backend, TestBackend}, Terminal};

    fn key(code: KeyCode) -> KeyEvent { KeyEvent::new(code, KeyModifiers::NONE) }

    #[test]
    fn saves_validate_preserve_comments_and_reject_stale_documents() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("config.toml");
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(&global, "# keep this comment\n[memory]\nenabled = false\n").unwrap();
        let original = document(&global, &project, false).unwrap();
        assert!(save(&global, &project, false, &original, "memory.enabled", Value::String("not a boolean".into())).is_err());
        assert!(std::fs::read_to_string(&global).unwrap().contains("enabled = false"));
        save(&global, &project, false, &original, "memory.enabled", Value::Bool(true)).unwrap();
        assert!(std::fs::read_to_string(&global).unwrap().contains("# keep this comment"));
        assert!(save(&global, &project, false, &original, "memory.enabled", Value::Bool(false)).unwrap_err().contains("changed"));
        let effective = document(&global, &project, true).unwrap();
        save(&global, &project, true, &effective, "memory.enabled", Value::Bool(false)).unwrap();
        assert_eq!(document(&global, &project, true).unwrap()["memory"]["enabled"], false);
        assert_eq!(document(&global, &project, false).unwrap()["memory"]["enabled"], true);
        let literal = toml_literal(&serde_json::json!({"name":"test", "optional":null, "nested":{"enabled":true, "absent":null}})).unwrap();
        assert!(!literal.contains("optional"));
        assert!(!literal.contains("absent"));
    }

    #[test]
    fn canonical_agent_edits_preserve_multiline_prompts_and_identity() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("config.toml");
        let original = document(&global, dir.path(), false).unwrap();
        let agent = &original["agents"][0];
        let field = agent.as_object().unwrap().iter().find(|(key, value)| key.contains("prompt") && value.is_string()).unwrap().0;
        let path = format!("agents.0.{field}");
        let prompt = Value::String("First line\nUnicode 世界\nThird line".into());
        save(&global, dir.path(), false, &original, &path, prompt.clone()).unwrap();
        let saved = document(&global, dir.path(), false).unwrap();
        assert_eq!(value_at(&saved, &path), Some(&prompt));
        assert_eq!(saved["agents"][0]["id"], original["agents"][0]["id"]);
        assert!(save(&global, dir.path(), false, &saved, "agents.0.id", Value::String("changed".into())).is_err());
        let effective = document(&global, dir.path(), true).unwrap();
        assert!(save(&global, dir.path(), true, &effective, &path, prompt).is_err());
    }

    #[test]
    fn editor_keeps_unicode_and_pasted_newlines_until_explicit_save() {
        let mut screen = Management { root: serde_json::json!({"prompt":"世界"}), ..Default::default() };
        screen.key(key(KeyCode::Enter));
        screen.key(key(KeyCode::Left));
        screen.paste("hi\r\nthere\u{1b}");
        assert_eq!(screen.edit.as_ref().unwrap().text, "世hi\nthere界");
        screen.key(key(KeyCode::Delete));
        assert_eq!(screen.edit.as_ref().unwrap().text, "世hi\nthere");
        screen.key(key(KeyCode::Esc));
        assert!(screen.edit.as_ref().unwrap().discard);
        screen.key(key(KeyCode::Char('n')));
        assert!(!screen.edit.as_ref().unwrap().discard);
        screen.key(key(KeyCode::Esc));
        screen.key(key(KeyCode::Char('y')));
        assert!(screen.edit.is_none());
        assert_eq!(screen.root["prompt"], "世界");
    }

    #[test]
    fn browser_and_multiline_editor_render_in_small_terminals() {
        for (width, height) in [(24, 8), (40, 12), (80, 24), (120, 40)] {
            let mut screen = Management { root: serde_json::json!({"prompt":"line\n".repeat(80)}), ..Default::default() };
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| screen.draw(frame, &CliTheme::by_name("Midnight"))).unwrap();
            screen.key(key(KeyCode::Enter));
            terminal.draw(|frame| screen.draw(frame, &CliTheme::by_name("Midnight"))).unwrap();
            let position = terminal.backend_mut().get_cursor_position().unwrap();
            assert!(position.x < width && position.y < height);
            screen.key(key(KeyCode::Esc));
            terminal.draw(|frame| screen.draw(frame, &CliTheme::by_name("Midnight"))).unwrap();
        }
        assert!(contains_redaction(&serde_json::json!({"env":{"KEY":"[REDACTED]"}})));
    }
}

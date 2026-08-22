//! Hierarchical file-tree widget for the code editor sidebar.
//!
//! Renders a collapsible directory tree rooted at a given path. Directories
//! can be expanded/collapsed by clicking their chevron. Files emit
//! `FileSelected(path)` when clicked. The tree is built from the real
//! filesystem, filtered to skip common VCS/build directories.

use camino::Utf8PathBuf;
use iced::widget::{button, column, row, scrollable, text};
use iced::{Alignment, Element};

/// A single node in the file tree.
#[derive(Debug, Clone)]
pub enum TreeNode {
    Dir { name: String, path: Utf8PathBuf, children: Vec<TreeNode>, expanded: bool },
    File { name: String, path: Utf8PathBuf, lang: &'static str },
}

impl TreeNode {
    /// Build a tree from the filesystem, starting at `root`.
    pub fn from_disk(root: &camino::Utf8Path) -> Self {
        let name = root.file_name().unwrap_or(".").to_string();
        let mut node =
            TreeNode::Dir { name, path: root.to_path_buf(), children: Vec::new(), expanded: true };
        node.populate_children(root);
        node
    }

    fn populate_children(&mut self, dir: &camino::Utf8Path) {
        let children = match self {
            TreeNode::Dir { children, .. } => children,
            TreeNode::File { .. } => return,
        };

        let entries = match std::fs::read_dir(dir.as_std_path()) {
            Ok(e) => e,
            Err(_) => return,
        };

        let mut dirs: Vec<TreeNode> = Vec::new();
        let mut files: Vec<TreeNode> = Vec::new();

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let entry_name = match file_name.to_str() {
                Some(n) => n,
                None => continue,
            };
            if should_skip(entry_name) {
                continue;
            }

            let path = dir.join(entry_name);
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };

            if file_type.is_dir() {
                let mut child = TreeNode::Dir {
                    name: entry_name.to_string(),
                    path: path.clone(),
                    children: Vec::new(),
                    expanded: false,
                };
                child.populate_children(&path);
                dirs.push(child);
            } else if file_type.is_file() {
                let lang = lang_for_file(&path);
                files.push(TreeNode::File { name: entry_name.to_string(), path, lang });
            }
        }

        dirs.sort_by_key(|a| a.name());
        files.sort_by_key(|a| a.name());

        children.extend(dirs);
        children.extend(files);
    }

    /// Toggle expansion of this directory node (if it is one).
    pub fn toggle(&mut self) {
        if let TreeNode::Dir { expanded, .. } = self {
            *expanded = !*expanded;
        }
    }

    pub fn name(&self) -> String {
        match self {
            TreeNode::Dir { name, .. } => name.clone(),
            TreeNode::File { name, .. } => name.clone(),
        }
    }

    pub fn path(&self) -> &camino::Utf8Path {
        match self {
            TreeNode::Dir { path, .. } => path,
            TreeNode::File { path, .. } => path,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, TreeNode::Dir { .. })
    }

    /// Recursively find a node by path and expand all ancestors.
    pub fn expand_to(&mut self, target: &camino::Utf8Path) {
        match self {
            TreeNode::Dir { path, expanded, children, .. } => {
                if path.as_path() != target {
                    *expanded = true;
                    for child in children.iter_mut() {
                        child.expand_to(target);
                    }
                }
            }
            TreeNode::File { .. } => {}
        }
    }

    /// Recursively find a node by path and toggle its expansion.
    pub fn toggle_path(&mut self, target: &camino::Utf8Path) {
        match self {
            TreeNode::Dir { path, expanded, children, .. } => {
                if path.as_path() == target {
                    *expanded = !*expanded;
                } else {
                    for child in children.iter_mut() {
                        child.toggle_path(target);
                    }
                }
            }
            TreeNode::File { .. } => {}
        }
    }
}

/// Directories and file patterns to skip in the tree.
const SKIP_NAMES: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    ".bzr",
    ".idea",
    ".vscode",
    ".cargo",
    ".rustup",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".gradle",
    ".m2",
    ".cache",
    "venv",
    ".venv",
    "__pycache__",
    ".DS_Store",
    "Cargo.lock",
];

fn should_skip(name: &str) -> bool {
    SKIP_NAMES.contains(&name)
}

/// Determine the language token for syntax highlighting based on file extension.
pub fn lang_for_file(path: &camino::Utf8Path) -> &'static str {
    match path.extension() {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("js") | Some("jsx") | Some("mjs") => "javascript",
        Some("ts") | Some("tsx") => "typescript",
        Some("go") => "go",
        Some("c") => "c",
        Some("h") => "c",
        Some("cpp") | Some("cc") | Some("cxx") => "cpp",
        Some("hpp") | Some("hh") => "cpp",
        Some("java") => "java",
        Some("kt") => "kotlin",
        Some("swift") => "swift",
        Some("rb") => "ruby",
        Some("php") => "php",
        Some("cs") => "csharp",
        Some("lua") => "lua",
        Some("sh") | Some("bash") | Some("zsh") => "bash",
        Some("ps1") => "powershell",
        Some("sql") => "sql",
        Some("html") | Some("htm") => "html",
        Some("css") => "css",
        Some("scss") | Some("sass") => "scss",
        Some("less") => "less",
        Some("json") => "json",
        Some("yaml") | Some("yml") => "yaml",
        Some("toml") => "toml",
        Some("xml") => "xml",
        Some("md") | Some("mdx") => "markdown",
        Some("ini") | Some("cfg") => "ini",
        Some("csv") => "csv",
        Some("txt") => "plain",
        _ => "plain",
    }
}

/// Messages emitted by the file tree widget.
#[derive(Debug, Clone)]
pub enum TreeMessage {
    /// A file was selected for opening.
    FileSelected(Utf8PathBuf),
    /// A directory node was toggled.
    DirToggled(Utf8PathBuf),
}

/// Render the file tree as a scrollable widget.
///
/// `root` is the tree to render.
/// `active` is the currently open file path (for highlighting).
pub fn view<'a>(
    root: &'a TreeNode,
    active: Option<&'a camino::Utf8Path>,
) -> Element<'a, TreeMessage> {
    let content = render_node(root, active, 0);
    scrollable(column![content].spacing(2).padding(8)).into()
}

fn render_node<'a>(
    node: &'a TreeNode,
    active: Option<&'a camino::Utf8Path>,
    depth: usize,
) -> Element<'a, TreeMessage> {
    let indent: Vec<Element<'a, TreeMessage>> =
        (0..depth).map(|_| text("  ").size(10).into()).collect();
    let is_active = active.is_some_and(|a| a == node.path());
    // A directory strictly on the active file's path is "in scope": it gets
    // a dim fill, distinct from the solid fill of the selected leaf (#91).
    let is_ancestor = active.is_some_and(|a| a != node.path() && a.starts_with(node.path()));

    match node {
        TreeNode::Dir { name, path, children, expanded } => {
            let chevron = if *expanded { "▼" } else { "▶" };
            let mut row_widgets: Vec<Element<'a, TreeMessage>> = indent;
            let dir_btn = button(
                row![text(chevron).size(10), text(name).size(13),]
                    .spacing(4)
                    .align_y(Alignment::Center),
            )
            .padding(2)
            .style(if is_ancestor {
                // Dimmed "in scope" fill for ancestors of the open file.
                move |_theme: &iced::Theme, _status| {
                    let palette = _theme.extended_palette();
                    iced::widget::button::Style {
                        background: Some(
                            iced::Color { a: 0.35, ..palette.primary.base.color }.into(),
                        ),
                        ..iced::widget::button::Style::default()
                    }
                }
            } else {
                button::secondary
            });
            row_widgets.push(dir_btn.on_press(TreeMessage::DirToggled(path.clone())).into());
            let mut col: Vec<Element<'a, TreeMessage>> = vec![row(row_widgets).spacing(4).into()];

            if *expanded {
                let child_col = column(
                    children.iter().map(|c| render_node(c, active, depth + 1)).collect::<Vec<_>>(),
                )
                .spacing(1);
                col.push(child_col.into());
            }

            column(col).into()
        }
        TreeNode::File { name, path, lang: _ } => {
            let icon = file_icon(path);
            let mut row_widgets: Vec<Element<'a, TreeMessage>> = indent;
            let btn = button(
                row![text(icon).size(11), text(name).size(12),]
                    .spacing(4)
                    .align_y(Alignment::Center),
            )
            .padding(2)
            .style(if is_active {
                move |_theme: &iced::Theme, _status| {
                    let palette = _theme.extended_palette();
                    iced::widget::button::Style {
                        background: Some(palette.primary.base.color.into()),
                        text_color: palette.background.base.color,
                        ..iced::widget::button::Style::default()
                    }
                }
            } else {
                button::secondary
            });

            row_widgets.push(btn.on_press(TreeMessage::FileSelected(path.clone())).into());
            row(row_widgets).spacing(4).into()
        }
    }
}

/// Return an emoji icon for the file extension.
///
/// The icons are emoji, so their colors are the vendor's fixed glyph colors:
/// the apparent color variance between icons is incidental and carries no
/// meaning (decision recorded for #91 — no legend needed).
fn file_icon(path: &camino::Utf8Path) -> &'static str {
    match path.extension() {
        Some("rs") => "🦀",
        Some("py") => "🐍",
        Some("js") | Some("jsx") | Some("mjs") => "📜",
        Some("ts") | Some("tsx") => "📘",
        Some("go") => "🐹",
        Some("c") | Some("h") | Some("cpp") | Some("hpp") => "⚙",
        Some("java") => "☕",
        Some("kt") => "🎯",
        Some("swift") => "🦉",
        Some("rb") => "💎",
        Some("php") => "🐘",
        Some("cs") => "♯",
        Some("lua") => "🌙",
        Some("sh") | Some("bash") | Some("zsh") => "🐚",
        Some("sql") => "🗄",
        Some("html") | Some("htm") => "🌐",
        Some("css") => "🎨",
        Some("scss") | Some("sass") => "🎨",
        Some("json") => "📋",
        Some("yaml") | Some("yml") => "📝",
        Some("toml") => "⚙",
        Some("xml") => "📄",
        Some("md") | Some("mdx") => "📝",
        Some("lock") => "🔒",
        Some("txt") => "📄",
        _ => "📄",
    }
}

/// Re-export for use in diff view (backward compatibility).
pub fn flat_view<'a>(
    files: &'a [Utf8PathBuf],
    active: Option<&'a camino::Utf8Path>,
) -> Element<'a, crate::views::diff::Message> {
    use iced::widget::text;
    let mut children: Vec<Element<'_, crate::views::diff::Message>> =
        Vec::with_capacity(files.len());
    for path in files {
        let label = path.file_name().unwrap_or_else(|| path.as_str());
        let mut btn = button(text(label));
        if Some(path.as_path()) == active {
            btn = btn.style(move |theme: &iced::Theme, _status| {
                let palette = theme.extended_palette();
                iced::widget::button::Style {
                    background: Some(palette.primary.base.color.into()),
                    text_color: palette.background.base.color,
                    ..iced::widget::button::Style::default()
                }
            });
        }
        let path_clone = path.clone();
        children.push(btn.on_press(crate::views::diff::Message::FileSelected(path_clone)).into());
    }
    scrollable(column(children)).spacing(2).into()
}

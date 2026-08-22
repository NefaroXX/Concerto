use crate::widgets::file_tree;
use camino::Utf8PathBuf;
use iced::widget::pane_grid;
use iced::widget::text_editor;

use super::{CompletionItem, Diagnostic};

/// Messages emitted by the editor view.
#[derive(Debug, Clone)]
pub enum Message {
    /// A file was selected in the tree.
    FileSelected(Utf8PathBuf),
    /// A directory node was toggled.
    DirToggled(Utf8PathBuf),
    /// Text editor action (typing, cursor movement, etc.).
    Edit(text_editor::Action),
    /// Save the current file.
    Save,
    /// Create a new file (prompts for name).
    NewFile,
    /// New file name entered.
    NewFileName(String),
    /// Delete the current file.
    DeleteFile,
    /// The user confirmed the pending delete (armed by `DeleteFile`).
    DeleteConfirmed,
    /// The user cancelled the pending delete.
    DeleteCancelled,
    /// Refresh the file tree from disk.
    RefreshTree,
    /// LSP hover result received.
    LspHover(String),
    /// LSP diagnostics received.
    LspDiagnostics(Vec<Diagnostic>),
    /// Toggle the diagnostics panel.
    ToggleDiagnostics,
    /// Clear the hover tooltip.
    ClearHover,
    /// LSP request completed (open file, etc.).
    LspReady,
    /// LSP error occurred.
    LspError(String),
    /// Messages from the file tree widget.
    FileTree(file_tree::TreeMessage),
    /// Undo the last edit batch.
    Undo,
    /// Redo the last undone edit batch.
    Redo,
    /// Enter was pressed — continue comments/indentation intelligently.
    SmartEnter,
    /// Insert spaces per the active `TabMode`.
    InsertSpaces,
    /// Indent the current line/selection (Tab in `Tabs` mode).
    IndentSelection,
    /// Unindent the current line/selection (Shift+Tab).
    UnindentSelection,
    /// Cycle the Tab behavior (Tabs → Spaces:2 → Spaces:4 → Spaces:8).
    CycleTabMode,
    /// Toggle trimming trailing whitespace on save.
    ToggleTrimTrailing,
    /// Open the find bar (Ctrl+F). Pre-fills from the current selection.
    OpenFind,
    /// Open find + replace rows (Ctrl+H).
    OpenReplace,
    /// Close the find/replace bar.
    CloseFind,
    /// Find query edited.
    FindQueryChanged(String),
    /// Jump to the next match (Enter / F3).
    FindNext,
    /// Jump to the previous match (Shift+F3).
    FindPrev,
    /// Toggle case-sensitive matching.
    ToggleFindCase,
    /// Replacement text edited.
    ReplaceQueryChanged(String),
    /// Replace the current match and advance.
    ReplaceCurrent,
    /// Replace every match in the buffer.
    ReplaceAll,
    /// Open the go-to-line bar (Ctrl+G).
    OpenGoto,
    /// Go-to-line input edited.
    GotoInputChanged(String),
    /// Confirm go-to-line.
    GotoSubmit,
    /// Close the go-to-line bar.
    CloseGoto,
    /// Fold all top-level regions (Ctrl+Shift+-).
    FoldAll,
    /// Expand every folded region (Ctrl+Shift+=).
    UnfoldAll,
    /// Toggle the fold at a display line (gutter chevron click).
    ToggleFold(usize),
    /// Request LSP completions at the current cursor (Ctrl+Space).
    CompletionRequest,
    /// Completions received from the LSP server.
    CompletionReceived(Vec<CompletionItem>),
    /// Select the next completion item.
    CompletionNext,
    /// Select the previous completion item.
    CompletionPrev,
    /// Accept the selected completion (Enter/Tab).
    CompletionAccept,
    /// Close the completion popup.
    CompletionClose,
    /// Pick a completion item by index (mouse click).
    CompletionPick(usize),
    /// Request go-to-definition at the current cursor (F12).
    DefinitionRequest,
    /// Definition received from the LSP server.
    DefinitionReceived(Option<(Utf8PathBuf, usize, usize)>),
    /// Request hover info at the current cursor (Ctrl+I).
    HoverRequest,
    /// A pane divider of the tree | editor split was dragged.
    PaneResized(pane_grid::ResizeEvent),
}

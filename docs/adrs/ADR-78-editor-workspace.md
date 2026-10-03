# ADR-78: Native editor workspace

Status: Proposed

## Decision

Keep the desktop editor in Iced and reuse its existing text editor, LSP client,
folding, search, and VirtualFs. Arrange the workspace as a tab strip above an
Explorer/editor split, with breadcrumbs, staged review, floating search and
completion, a bottom Problems panel, and a compact status bar.

Open documents live in memory for the lifetime of the editor state. Switching
documents preserves the actual text editor content, cursor, history, folds,
diagnostics, indentation mode, and dirty flag. Closing a dirty tab requires an
explicit discard decision. No session format or cross-crate ownership changes.

Async LSP responses carry a document revision and, for positional requests,
the originating cursor. Replies from a different document, an earlier revision,
or an earlier cursor cannot mutate the active document.

Review controls operate on the selected VirtualFs entry only. Acceptance uses
VirtualFs materialization and removes the accepted entry only after success.
Discard removes the staged overlay without writing the original to disk.
Unsaved user edits block both operations. Existing policy and agent write gates
retain their ownership; these are explicit user review actions.

## Consequences

No webview or editor dependency is introduced. Open buffers are session-local;
crash recovery and persisted tabs are separate work. Diagnostics cover open
documents whose diagnostics have been received, not a claimed workspace-wide
scan. The current LSP manager starts rust-analyzer; the status must report
availability rather than imply language-server support for every file type.
Iced text_editor does not expose a cursor rectangle or diagnostic decoration
API, so completion is floated in the editor and diagnostic details appear at
the cursor in a separate annotation row; exact cursor anchoring and squiggles
require a future editor widget change.

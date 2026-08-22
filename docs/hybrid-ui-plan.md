# Hybrid Chat-Centric UI Plan

**Status:** Minimal scope merged on `dev` (PR #49, 2026-07-24); medium scope
merged on `dev` (PR #97, 2026-08-03); full scope pending
(see [TODO.md](TODO.md))
**Author:** AI-assessed 2026-07-24; status refreshed 2026-08-03  
**Target:** Pre-release or post-1.0 polish

This document captures the architectural shift from flat page-based navigation to a chat-centric layout where supporting views become inline panels, modals, and sidebar sections. Minimal scope (SubView overlays for Diff, Agent Graph, Tool Log) is merged on `dev` via PR #49 (commit `9bfac84`, 2026-07-24). Medium scope is merged on `dev` via PR #97 (merge `19cf7e2`, 2026-08-03) — terminal bottom panel with drag resize, Memory Explorer as a compact quick-panel section, glass modals and overlay/panel animations, chat timestamps with transcript format v2, blinking streaming cursor. Full scope remains pending and is tracked in [TODO.md](TODO.md).

---

## 1. Problem

The current UI uses a `Page` enum to switch between 10 full-screen views. This forces the user to leave their conversation context for every supporting task (reviewing diffs, checking agent state, browsing memory, etc.). Users experience:

- Frequent context switching (page load, scroll position loss)
- Empty/flat feeling when navigating away from Chat
- No way to see auxiliary information alongside the conversation

---

## 2. Target Architecture

```
┌──────────────────────────────────────────────────────┐
│  Left Sidebar    │  Main Canvas    │  Right Panel     │
│  (always there)  │  (Chat-centric) │  (collapsible)   │
│                  │                 │       280px      │
│  > Chat        │  ┌───────────┐  │ ● Idle          │
│  > Agents      │  │ Messages  │  │ [model pick]    │
│  > Diff        │  │           │  │ ─────────       │
│  > Memory      │  │           │  │ Agent: Coder    │
│  > Terminal    │  ├───────────┤  │ Agent: Revie…   │
│                 │  │ Composer   │  │ ─────────       │
│  ─────────     │  └───────────┘  │ Git: main       │
│  Settings      │                  │ 3 changed       │
│  Studio        │  [Modal: Diff /  │ ─────────       │
│                 │   Agent Graph /  │ View Chat│Edit  │
│                 │   Tool Log /    │                  │
│                 │   Help overlay] │                  │
└──────────────────────────────────────────────────────┘
                      │ Bottom Panel (toggleable) │
                      │ Terminal                  │
                      └───────────────────────────┘
```

### Key principles

| Principle | Description |
|-----------|-------------|
| **Chat is home** | The main area is always the conversation. Everything else layers on top. |
| **Modals for context** | Diff, Agent Graph, Tool Log, Help appear as centered overlays (like the existing capability dialog pattern). |
| **Panels for glanceable info** | Quick panel sections for agent list, memory search, git status, tool log. |
| **Full pages for complex UIs** | Settings, Editor, Orchestration Studio remain navigable pages — they're too heavy for panels or modals (the Dashboard page was removed; spend lives in the status-bar chip and the Spend Log modal). |
| **Toggleable bottom panel** | Terminal is the primary candidate for a resizable bottom area (VS Code style). |

---

## 3. Scope Levels

### 🟢 Minimal (~20–30h) — Recommended for pre-release

| Status | Change | Files | Effort |
|--------|--------|-------|--------|
| ✅ | Make Chat the default root layout (it already is) with a new `AppLayout` wrapper | `app.rs` | 2h |
| ✅ | Add a `SubView` enum to `chat::State`: `Main`, `Diff`, `AgentGraph`, `ToolLog` | `views/chat.rs` | 3h |
| ✅ | Route Diff view into the Chat canvas as a centered modal when `SubView::Diff` | `views/chat.rs`, `app.rs` | 4h |
| ✅ | Route Agent Graph into Chat as a modal overlay when `SubView::AgentGraph` | `views/chat.rs`, `app.rs` | 4h |
| ✅ | Route Tool Log into Chat as a full-width modal overlay when `SubView::ToolLog` | `views/chat.rs`, `views/tool_log.rs`, `app.rs` | 5h |
| ✅ | Keep Terminal, Memory as full pages (Dashboard page removed — recent sessions and spend now live in Chat's empty state and the status bar) | — | 0h |
| ✅ | Keep Settings, Editor, OrchestrationStudio as full pages | — | 0h |
| ✅ | Update keyboard shortcuts for new layout | `shortcuts.rs`, `app.rs` | 3h |

### 🟡 Medium (~60–90h) — Full hybrid

All of Minimal, plus:

| Change | Files | Effort |
|--------|-------|--------|
| Memory Explorer as right panel searchable section | `views/quick_panel.rs`, `views/memory.rs` | 8h |
| Terminal as toggleable bottom panel (with resize handle) | `views/terminal.rs`, `app.rs` | 15h |
| SubView routing fully replaces `Page` for Chat-adjacent views | `app.rs` | 8h |
| Animation: panel slide/overlay transitions | `app.rs`, `views/` | 6h |
| State lifecycle: lazy init for infrequently used views | `app.rs` | 8h |

### 🔴 Full (~150–220h) — Post-1.0

All of Medium, plus split Settings into tabbed sub-views, convert Studio to split pane, add drag-and-drop agent assignment, animated panels, focus-trap system.

---

## 4. Implementation Guide (Minimal Scope)

### Step 1 — Add `AppLayout` wrapper in `app.rs`

Create a layout function that composes the three zones instead of routing purely by `Page`:

```rust
// In app.rs, around line 2148
pub fn view(&self) -> Element<'_, Message> {
    let sidebar = views::nav::sidebar_view(self);
    
    let content: Element<'_, Message> = match self.page {
        Page::Chat => self.chat.view_with_subview(...),  // new method
        Page::Settings => self.settings.view(&self.current_theme).map(Message::Settings),
        Page::OrchestrationStudio => self.orchestration_studio.view(&self.current_theme),
        Page::Editor => self.editor.view(&self.current_theme).map(Message::Editor),
        // Page::Dashboard — removed: recent sessions and spend now live in
        // Chat's empty state, the status-bar chip, and the Spend Log modal
        // These are now routed through chat's SubView instead of Page:
        // Page::DiffViewer -> chat.sub_view = SubView::Diff
        // Page::AgentGraph  -> chat.sub_view = SubView::AgentGraph
        // Page::ToolLog     -> chat.sub_view = SubView::ToolLog (full-width modal)
        // Page::MemoryExplorer -> quick panel section
        // Page::Terminal    -> bottom panel
        _ => unreachable!(), // no longer direct pages
    };
    
    // Compose: sidebar | (content + modals) | quick panel
    // With optional bottom panel
}
```

### Step 2 — Add `SubView` enum to `views/chat.rs`

```rust
pub enum SubView {
    Main,
    Diff,
    AgentGraph,
    ToolLog,
}

impl State {
    pub fn view_with_subview<'a>(
        &'a self,
        theme: &'a AppTheme,
        multi_agent: bool,
        active_model: &'a str,
        model_names: &'a [String],
        mode: AgentMode,
        model_source: &'a str,
        recent_sessions: &'a [SessionRow],
        agent_graph: &'a agent_graph::State,
        has_agent_assignments: bool,
        sub_view: &SubView,
    ) -> Element<'a, Message> {
        match sub_view {
            SubView::Main => self.view(...), // existing, unchanged
            SubView::Diff => self.render_diff_overlay(...),
            SubView::AgentGraph => self.render_agent_graph_overlay(...),
            SubView::ToolLog => self.render_tool_log_modal(app_state),
        }
    }
}
```

The overlay methods render the sub-view content inside a `stack!` with a backdrop, mirroring the existing capability dialog pattern (see `app.rs` lines ~2229-2264).

### Step 3 — Adapt message routing

The current `Page::DiffViewer` → `Message::Navigate(Page::DiffViewer)` → diff page. After the change, hitting the Diff shortcut sets `chat.sub_view = SubView::Diff` instead:

```rust
// In app.rs update() or handle_shortcut()
Page::DiffViewer => {
    self.page = Page::Chat;
    // No state change needed — diff state is already in self.diff
    // The chat view will read it from app via callback/reference
}
```

The view then reads `sub_view` and renders the overlay. The overlay's close action returns to `SubView::Main`.

### Step 4 — Add lightweight sections to quick panel

The quick panel is **280px wide** (`PANEL_WIDTH: u16 = 280`). That's enough for compact controls but **not** for data-rich views.

**Does NOT fit** (keep as full pages or modals):
- **Dashboard** (removed as a page — ADR-41) — its cost summary row had 3
  items at size 18 and its session table 3 `Length::Fill` columns, which
  wrap/truncate at 280px; spend and recent sessions now live in the status-bar
  chip, the Spend Log modal, and Chat's recent-sessions list
- **Tool Log** (modal) — each row has timestamp(80px) + icon(20px) + tool name(120px) + duration(60px) = 280px with no room for the input summary

**Could fit** (add if useful):
- **Memory search**: compact search bar + short results list
- **Recent sessions**: condensed list (session ID + date, no cost/tokens columns)

Keep the quick panel focused on glanceable info: agent list, model picker, git status, view switcher.

### Step 5 — Tool Log modal

The Tool Log (`views/tool_log.rs`) renders a filterable, scrollable list of tool call events. Each row shows timestamp, verdict icon, tool name, input summary, and duration — needing full canvas width.

Render it as a centered overlay modal (same `stack![base, backdrop]` pattern as Step 6):

```
┌─────────────────────────────────────────────────┐
│  ← Tool Log                         [×] close   │
├─────────────────────────────────────────────────┤
│  [All ▼]  [Allowed]  [Denied]  [Running]        │
├─────────────────────────────────────────────────┤
│  10:23:45  ✓  read_file   /path/to/file  142ms  │
│  10:23:46  ✓  edit        main.rs         89ms  │
│  10:23:47  ✗  rm_file     /etc/passwd      3ms  │
│  ... (scrollable)                                │
└─────────────────────────────────────────────────┘
```

The modal gets a clean max-width (~90vw) with a backdrop. Pressing Escape or clicking the close button or backdrop returns to `SubView::Main`.

Tool Log state stays owned by `App` (as it is now), passed by reference to the chat overlay method. This keeps the modal lightweight — no state duplication.

### Step 6 — Update keyboard shortcuts

In `shortcuts.rs`, change navigation shortcuts for Diff, AgentGraph, ToolLog, Memory to set the chat sub-view instead of navigating to a Page:

```rust
Shortcut::DiffViewer => Message::Chat(chat::Message::SetSubView(SubView::Diff)),
Shortcut::MemoryExplorer => Message::Chat(chat::Message::SetSubView(SubView::Memory)),
```

Terminal becomes `Chat(Message::ToggleBottomPanel)` instead of `Navigate(Page::Terminal)`.

### Step 7 — Wire up modals

Use the existing `stack![base, backdrop]` pattern from `app.rs:2229-2264`:

```rust
// For Diff overlay:
if matches!(chat.sub_view, SubView::Diff) {
    let overlay = container(diff_view)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(/* backdrop: semi-transparent */);
    stack![base, overlay]
} else {
    base
}
```

Reuse `ui/container::modal` style for the overlay card. Wire a close action (clicking backdrop, pressing Escape) back to `SubView::Main`.

The Tool Log modal follows the same pattern, but with a `max_width` constraint (~90vw) instead of full canvas, so the chat behind it remains partially visible.

---

## 5. Existing Patterns to Reuse

| Pattern | File | How to reuse |
|---------|------|-------------|
| `card_style` | `theme.rs` | Modal card surfaces, panel sections |
| `ui/container::modal` | `ui/container.rs` | Modal overlay styling |
| `ui/button::*` | `ui/button.rs` | Close buttons, action buttons |
| `capability_dialog` | `widgets/capability_dialog.rs` | Modal backdrop + stack pattern |
| `confirm_modal` | `widgets/confirm_modal.rs` | Simple confirmation overlays |
| `empty_state` | `ui/empty_state.rs` | Panel empty states |
| `quick_panel` | `views/quick_panel.rs` | Right sidebar infrastructure |

---

## 6. Files Affected (Minimal Scope)

| File | Change type | Lines touched |
|------|-------------|---------------|
| `app.rs` | Moderate | ~60 lines (view layout, message routing) |
| `views/chat.rs` | Moderate | ~80 lines (SubView enum, overlay methods) |
| `views/quick_panel.rs` | Minor | ~20 lines (Memory search section, if added) |
| `views/nav.rs` | Minor | ~10 lines (remove Tool Log from sidebar — now a modal) |
| `shortcuts.rs` | Minor | ~10 lines (reroute shortcuts) |
| `views/tool_log.rs` | Minor | ~5 lines (expose a `modal_view()` that takes a close callback) |
| `ui/container.rs` | Minor | ~5 lines (if modal backdrop style is needed) |

---

## 7. Test Strategy

- Each sub-view overlay renders without panic (add to `tests/ui_quality.rs`)
- Chat view renders correctly in `SubView::Main`, `SubView::Diff`, `SubView::AgentGraph`, `SubView::ToolLog`
- Backdrop click closes the overlay
- Keyboard shortcut `Ctrl+D` opens Diff overlay when on Chat, navigates to Page::Diff when not
- Quick panel sections collapse/expand correctly
- Existing page-based navigation still works for Settings, Editor, Studio

---

## 8. Migration Path

The change is backwards-compatible at the `App` field level — all view states remain owned by `App`. No data migration needed. The `Page` enum keeps all existing variants (some become unreachable aliases for backward compat).

The commit order:

1. Add `SubView` enum + overlay rendering (no behavioral change yet)
2. Add `SubView::ToolLog` with its modal overlay rendering
3. Update `view()` layout to compose sidebar / content / right panel / bottom panel
4. Reroute shortcuts and navigation messages to use `SubView` instead of `Page`
5. Remove `ToolLog` from sidebar nav (now accessible via shortcut or from within Chat)
6. Remove unused `Page` variants
7. Add Terminal as toggleable bottom panel
8. Polish: animations, transitions, backdrop close

---

## 9. Status (merged on `dev` via PR #97)

**2026-08-03** — Implemented on `feat/ui-depth-improvements` and merged into
`dev` via PR #97 (merge `19cf7e2`):

- `1c916b4` — Move Memory Explorer into quick panel as compact section
- `4a12839` — Terminal as toggleable bottom panel with drag resize
- `ade0c7b` — Glass modals, centered diff/graph cards, overlay+panel animations
- `3d691a2` — Timestamp chat entries and bump transcript format to v2
- `f8c7b42` — Blinking cursor on streaming assistant entry

Minimal + Medium scope items from this plan are complete in code; Full scope (tabbed Settings, Studio split pane, focus trap, state lazy-init) is explicitly deferred to post-1.0.

Verification: fmt/clippy/tests/hardcoded-colors green; transcript v2 format is backward-compatible (legacy v1 files load and upgrade on next save).

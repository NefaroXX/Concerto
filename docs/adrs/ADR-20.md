# ADR-20: Rich Iced Desktop UI — Architecture, Theming & Accessibility

**Status:** Accepted  
**Date:** 2026-06-26  
**Deciders:** Concerto architecture  
**Phase:** 6  

> **Current implementation note (2026-07-19):** Concerto now uses Iced 0.14
> and has eight routed pages, including Terminal. The accessibility statements
> below are design targets and partial test contracts, not a completed WCAG
> certification. Fixed shortcuts remain in use. See `crates/desktop/src/app.rs`
> and `docs/STATUS.md` for the current surface.

## Context

Phase 6 upgrades the Phase 3 MVP Iced desktop shell from a single-chat-view
prototype to a full-featured production UI. This ADR documents the architecture
decisions behind that upgrade. It covers:

- **Page routing and delegation** — how the app shell manages navigation and
  fans messages out to per-view handlers.
- **Custom canvas widgets** — agent graph renderer and side-by-side diff viewer.
- **Theming** — three themes (Midnight, Slate, Chalk), design tokens,
  contrast gating, and persistence.
- **Keyboard control** — global shortcuts and a help overlay.
- **Accessibility** — WCAG 2.2 AA compliance, focus management, screen-reader
  announcements.

The decisions here are settled for Phase 6. The next major UI iteration
(Phase 8) may revisit keyboard shortcut remapping and live tool-call replay.

## Decisions

### 1. Page-routed single-window shell

**Chosen:** A single `App` struct with a `Page` enum and namespaced messages
(`Message::Chat(chat::Message)`, `Message::Memory(memory::Message)`, etc.).
Each view owns a `State` struct and an `update`/`view` pair.

**Rationale:**
- Iced 0.14 does not natively support multi-window on all platforms. A
  single-window shell with routing is the idiomatic Iced pattern.
- Namespaced messages prevent name collisions across views and keep
  `app.rs` `update` a thin dispatch layer.
- Each view can be developed and tested independently.

**Trade-off:** All views are resident in memory simultaneously. For Phase 6
with at most 7 views this is acceptable (~hundreds of KB each, not MB).

### 2. Canvas-based custom widgets (agent graph + diff viewer)

**Chosen:** Both the agent graph renderer and the side-by-side diff viewer
use `iced::widget::canvas::Program` with `canvas::Cache` for static geometry.

**Rationale:**
- Canvas gives full control over rendering (graph edges, synchronized
  scrolling, pan/zoom) that built-in widgets cannot provide.
- `canvas::Cache` caches the static layer (node shapes, text labels,
  highlighted code) and invalidates only on model change. Pan/zoom is
  applied as a canvas transform, not a cache invalidation — keeping the
  expensive path off the 60fps hot path.
- Bar, donut, and scatter charts on the dashboard are also canvas-based
  rather than pulling in `plotters-iced`. This removes a version-lock
  dependency and the charts P6 needs are simple enough for direct canvas rendering.

### 3. AppTheme wrapping iced::Theme with custom Palette

**Chosen:** `AppTheme` wraps `iced::Theme` and adds a `Palette` struct with
all semantic colors (background, surface, primary, success, warning, danger,
text, text_muted, border, accent, focus_ring, agent_role colors).

```rust
pub struct AppTheme {
    pub name: &'static str,
    pub iced: iced::Theme,
    pub palette: Palette,
    pub font_stack: FontStack,
}
```

**Rationale:**
- Iced 0.14's `Theme` only provides a `Palette` with 5 fields (bg, text,
  primary, success, danger). A full-featured desktop UI needs ~15 semantic
  colors, including surface variants, muted text, borders, focus rings, and
  per-agent-role colors.
- Wrapping rather than replacing `iced::Theme` lets us use Iced's built-in
  widget styling while injecting our own palette for custom widgets and
  styling overrides.
- `AppTheme::by_name("Midnight")` returns the theme; `FontStack` carries
  the family and persisted base size.

**Three themes:**
- **Midnight** — dark theme with deep blue-gray background.
- **Slate** — medium-contrast warm-gray theme.
- **Chalk** — light theme with warm off-white background.

### 4. Contrast gated by unit test (CI automation)

**Chosen:** A `contrast_ratio(fg, bg) -> f32` function computes WCAG 2.x
relative luminance. A `#[test]` iterates over all three themes and asserts:

- Body text on background/surface ≥ 4.5:1 (AA normal text)
- Borders and focus rings on surface ≥ 3:1 (AA large text / UI components)

**Rationale:** This converts "verify contrast" from a manual chore into a
CI gate. Picking palette colors becomes an iterate-until-green loop. The
test lives in `theme/contrast.rs` and requires no external dependencies.

### 5. Theme and font-size persistence via UserPrefsStore

**Chosen:** Theme name and font size are stored in `UserPrefsStore`
(crates/memory/src/prefs.rs) under keys `ui_theme` and `ui_font_size`.
On load the theme is looked up by name; font size is parsed and clamped
to 12–20px.

**Rationale:** `UserPrefsStore` already exists (Phase 3) with JSON-file
persistence. Adding two string keys avoids introducing a new storage
mechanism. The `PrefKey::UiTheme` variant already exists.

### 6. Keyboard shortcuts via global iced subscription

**Chosen:** A `shortcuts::resolve(key, modifiers, text_focused) -> Option<Shortcut>`
function is called from an `iced::keyboard::on_key_press` subscription. The
result is dispatched as `Message::Shortcut(shortcut)`.

**Rationale:**
- Iced 0.14 provides `keyboard::on_key_press` which fires on every keypress.
- The `text_focused` flag prevents stealing typing when a text input is
  focused — only Escape and Ctrl+Enter pass through regardless.
- Fixed shortcuts for Phase 6 (Ctrl+T new task, Ctrl+S session list,
  Ctrl+D diff viewer, Ctrl+M memory explorer, Ctrl+L tool log,
  Ctrl+Z undo run, ? help overlay). Remappable shortcuts are deferred
  to Phase 8.

### 7. Fixed (not remappable) shortcuts for Phase 6

**Chosen:** Ship with fixed keyboard shortcuts. User remapping is a Phase 8
feature.

**Rationale:** A remapping UI (keybinding editor, conflict detection,
serialisation) is a significant feature in its own right. Fixed shortcuts
get us keyboard-driven workflow in Phase 6 with minimal complexity.
The `Shortcut` enum and handler architecture make remapping a localised
change later.

### 8. Accessibility principles

**Chosen:** WCAG 2.2 AA compliance targets:

1. **Accessible names** on every interactive element via Iced `Widget::on_accessibility`.
2. **Visible focus ring** using `palette.focus_ring` on all focusable elements.
3. **Minimum 24×24px touch targets** (WCAG 2.5.8).
4. **Status conveyed by icon + text + color**, never color alone — this
   applies to agent state indicators, tool verdicts, and all status badges.
5. **Agent status announcements** via `iced::widget::Text::w focusable`
   or `canvas::Cache` accessibility helpers.
6. **Logical tab order** following natural reading order.
7. **Contrast already gated** by the §4 unit test above.

**Rationale:** WCAG 2.2 AA is the current standard. The three-theme system
with automated contrast testing guarantees AA compliance regardless of
which theme the user selects.

### 9. Hunk state as UI overlay, reconciled on commit

**Chosen:** The diff viewer keeps hunk decisions (accepted/rejected/pending)
in a `HashMap<(PathBuf, HunkId), HunkDecision>` owned by the view state.
On "commit", these decisions are applied to `VirtualFs` in batch.

**Rationale:** VirtualFs is the source of truth for file state. The UI
should not mutate VirtualFs on every accept/reject click — that would
make undo harder and create coupling. Instead, the UI builds a decision
map and applies it atomically on commit. This also makes "Undo entire
run" trivial: just discard the overlay.

### 10. Row virtualization for long diffs and memory lists

**Chosen:** Both the diff viewer and the memory explorer use row
virtualization — render only the visible window plus a small buffer,
not the entire list. The diff viewer renders visible lines by computing
which line range falls in the viewport from the scroll offset.

**Rationale:** Full-list rendering of 1000+ line diffs or 10,000+ memory
entries causes measurable frame drops. Row virtualization is a standard
technique for this problem and avoids platform-specific workarounds.

## Consequences

### Positive

- Single-window architecture is simple, testable, and well-supported by Iced.
- Canvas-based widgets give full rendering control with good performance
  characteristics (cache + transform pattern).
- Automated contrast checking in CI eliminates manual theme QA.
- Each view is independently developable — team members can work on
  chat, memory, and settings simultaneously.
- Fixed shortcuts provide keyboard-driven workflow with minimal complexity.
- UI overlay pattern for hunk decisions keeps VirtualFs authority intact.
- Row virtualization ensures smooth scrolling at large data sizes.

### Negative

- All views resident in memory simultaneously (~tens of MB for worst case).
  Mitigate by deferring expensive state (full memory store) to on-demand loading.
- Canvas-based charts are more code than `plotters-iced` would be.
  Acceptable for the simple chart types P6 needs.
- Fixed shortcuts mean no user customization until Phase 8.
  Mitigated by the help overlay (`?`) listing all shortcuts.

### Risks

- **R-01** (iced 0.14 upgrade breaks desktop code): Impact **High**.
  Mitigation: Stay on 0.13 unless an accessibility spike proves
  screen-reader announcements are impossible on 0.13. If forced to
  upgrade, do it as the first code change with a rollback branch.
  (Resolved: Concerto now runs Iced 0.14 — see implementation note above.)
- **R-02** (canvas performance at max data): Impact **High**.
  Mitigation: `canvas::Cache` for static geometry; pan/zoom as
  transform; benchmark at 20 nodes/40 edges for graph and 1000-line
  diff.
- **R-03** (diff viewer jank on Linux/Wayland): Impact **Medium**.
  Mitigation: Row virtualization from day one; test on X11 and Wayland.
- **R-04** (hardcoded colors leak back in): Impact **Low**.
  Mitigation: Contrast unit test + CI grep for `Color::` outside `theme/`.

## References

- [Current architecture](../architecture.md)
- [ADR-08](ADR-08.md): Desktop UI (Iced)
- [ADR-19](ADR-19.md): Multi-agent orchestration
- [ADR-27](ADR-27.md): Integrated terminal lifecycle
- [ADR-30](ADR-30.md): Unified shell selection
- WCAG 2.2: https://www.w3.org/TR/WCAG22/

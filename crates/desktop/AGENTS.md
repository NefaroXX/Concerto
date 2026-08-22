# OVERVIEW
Iced desktop frontend with custom theming, views, widgets and keyboard shortcuts.

## STRUCTURE
```
desktop/
├── lib.rs          # Iced application builder and run()
├── main.rs         # binary entry point
├── app.rs          # Iced Application implementation
├── runtime.rs      # async runtime helpers
├── shortcuts.rs    # keyboard shortcut definitions
├── theme.rs        # theming system
├── theme/          # theme variants and contrast palettes
├── views/          # chat, agents, memory, tools, diff, terminal, settings
├── services/       # plugin approval, screenshot, session handlers
├── ui/             # shared forms, cards, layout, feedback and navigation
└── widgets/        # custom Iced widgets
```

## WHERE TO LOOK
| Concern | File(s) |
|--------|---------|
| App entry | `main.rs`, `app.rs` |
| Async runtime | `runtime.rs` |
| Theme definitions | `theme.rs`, `theme/` |
| View components | `views/` (chat, diff, memory, nav, quick_panel, settings, terminal, tool_log, agent_graph, status_bar) |
| Shared UI | `ui/` |
| Desktop services | `services/` |
| Custom widgets | `widgets/` (agent_graph, capability_dialog, charts, code_block, confirm_modal, diff_viewer, file_tree, highlight, markdown) |
| Keyboard shortcuts | `shortcuts.rs` |
| Re-exports | `lib.rs` |

## CONVENTIONS
- **Iced version** - 0.14 via `iced::application(App::new, App::update, App::view)`
- **Theme** - defined in `theme.rs` using `Palette` and `ExtendedPalette`; avoid hard-coded colors
- **Views** - keep rendering pure where possible; state/update pairs are routed
  by `App`
- **Widgets** - custom widgets live in `widgets/` and implement `iced::Widget`
- **Shortcuts** - centralized in `shortcuts.rs` using `iced::keyboard::HotKey`
- **Async** - all long-running work uses `tokio` with `CancellationToken`
- **Error handling** - propagate via `anyhow` in binary, `thiserror` in library

## BUILD & CHECKS (must pass before any push)
- `cargo fmt --all -- --check` — non-negotiable, matches CI toolchain (see repo-root AGENTS.md for pinned version)
- `cargo clippy -p concerto-desktop -- -D warnings` — warnings are errors, no exceptions
- `cargo test -p concerto-desktop` — unit + integration suite must be green
- These three commands run in sequence before any push or PR creation. The repo-root AGENTS.md documents the full-workspace CI workflow for reference.

## UI REWORK STATUS (hybrid chat-centric layout)
Tracked in `docs/hybrid-ui-plan.md`. Minimal + Medium scope from the plan is feature-complete on `feat/ui-depth-improvements`, pending review/merge:

| Status | Item |
|--------|------|
| ✅ | SubView enum (Main/Diff/AgentGraph/ToolLog) + keyboard shortcuts toggle overlays |
| ✅ | Diff overlay — full-size backdrop over chat canvas (Ctrl+D) |
| ✅ | Agent Graph overlay — full-size backdrop (Ctrl+◈ from chat) |
| ✅ | Tool Log modal — centered ~900px max-width overlay (Ctrl+L) |
| ✅ | Backdrop dismiss via close button; Esc reserved for help overlay |
| ✅ | Sidebar nav: Tool Log removed (now a modal) |
| ✅ | Memory Explorer as compact quick-panel section (search + inline confirm, page removed) |
| ✅ | Terminal as toggleable bottom panel (drag-resize handle, Ctrl+`) |
| ✅ | Animated panel slide / overlay transitions (shared 16ms tick, ease-out-cubic) |
| ✅ | Chat entries timestamped (created_at/finished_at), thinking duration labels (⏱ Ns) |
| ✅ | Blinking streaming cursor (▌, 500ms while assistant is streaming) |
| ⬜ | State lifecycle: lazy init for infrequently used views |
| ⬜ | Full scope: tabbed Settings, Studio split pane, focus trap |

## ANTI-PATTERNS
- Do not use `iced::window::Settings` directly in views; configure in `app.rs`
- Avoid blocking the UI thread; offload to `tokio::task::spawn_blocking`
- Never hard-code colors; use theme palette
- Do not mix `iced::widget::Container` and custom layout logic in the same view
- Refrain from using `unwrap` in widget rendering; handle `Option`/`Result` gracefully
- View state structs may own local state; cross-view/runtime state belongs in
  `App`
- Skip manual `iced::Event` handling; use `Subscription` where possible

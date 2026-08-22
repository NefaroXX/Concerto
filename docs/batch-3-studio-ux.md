# UX Specification: Orchestration Studio Blueprint Editor (P4 Batch 3)

> **Source:** designer UX pass for ADR-59 P4 Batch 3
> (`docs/adrs/ADR-59-studio-blueprint-editor.md`)
> **Date:** 2026-08-14
> **Consumed by:** implementation slices 2–4

## 1. Splash / Get-Started Flow

- Trigger: the Studio opens and detects that the `[orchestration]` table is absent in `config.toml`.
- Layout: center-aligned, minimalist empty state.
- Copy: "Orchestration Blueprint not initialized. Initialize the standard blueprint to enable advanced multi-agent orchestration features."
- Affordance: a single, prominent primary action button: [Initialize Blueprint].
- Interaction: button triggers the atomic two-write initialization flow (Batch 2a `initialize_blueprint`). On success the UI refreshes to the Studio view.

## 2. Stage Card Layout + Interactions

- Component: vertical stack of cards (`StageDef`).
- Interaction: click-to-edit inline. Keyboard navigation (Tab, Enter, Esc) is mandatory.
- Layout components:
  - Header: Tag (bold, `font.bold`), Label (editable text field).
  - Kind picker: segmented control or dropdown restricted to the 6 closed kinds.
  - Staffing: chips displaying agent IDs; add/remove (x icon) buttons per chip.
  - Mask flags: toggle switches (`fs_write`, `shell`) enabled only for relevant kinds.
  - Advanced (collapsible): feed (dropdown), condition (dropdown), max cycles (numeric input, >0), fallback persona (link to card).
- Heuristics: visual feedback for focus (`theme.palette.focus`) and disabled states (`theme.palette.text.disabled`). Palette colors only.

## 3. Relationship Rows (RelationshipDef)

- Layout: a separate, table-like surface beneath the stage list.
- Row pattern: [From Stage (dropdown)] -- [Kind (dropdown)] --> [To Stage (dropdown)].
- Visuals: distinct semantic icons per kind — Shield (ApprovalGate), Flow Arrow (ContextFlow), Hierarchy/Chain (Delegation).
- Interactions: row-level trash icon for deletion; "Add Relationship" button below the table.

## 4. Fallback Persona Card

- Placement: embedded within the stage card (conditionally expandable) to maintain context.
- Consistency: reuses the stage card's input components and section headers.
- Components: ID (read-only for sentinel), Label, System Instructions (text area), Capabilities (toggles, same as stage mask).

## 5. Structured Error Mapping (ADR-59 Decision 5)

- Badge: persistent toolbar badge (`theme.palette.danger`); displays error count + alert icon (never color alone).
- Detail bar: inline error panel showing the list of validation failures.
- Field mapping: failed fields get a 1px danger border (`theme.palette.danger`) plus an alert icon to the right of the field.
- Tooltip: on hover/focus, a clear, concise rule-violation message.
- Validation: real-time via `validate_blueprint`; save actions are blocked and the [Save] button disabled while errors exist.

## 6. Startup-Fallback Toast

- Wording: "Orchestration config fallback: loaded defaults due to load failure."
- Behavior: high-severity toast, persistent until dismissed (consistent with the app's existing toast system).

## 7. IA: Settings → Relationships

- Logic: Settings view retrieves the orchestration status; explicitly hide the "Relationships" menu item when `[orchestration]` is present (requires plumbing the flag — app.rs currently passes no config to the settings view).

## 8. Prioritized UX Defects (legacy Studio UI worth fixing during migration)

| Severity | Defect | Root cause | Recommended fix |
| 4 | Silent load/validation failure | legacy model lacks structural validation | integrate `validate_blueprint` + badge surfacing |
| 3 | Indistinguishable unsaved state | no feedback on modifications | "Modified" status indicator next to [Save] |
| 2 | Dense/confusing legacy tables | legacy multi-agent tables lack semantic grouping | replace with structured stage cards / relationship rows |
| 1 | Inconsistent button styles | hardcoded/unthemed button colors | use `theme.palette.*` for all action surfaces |

## 9. Open Questions / Product Decisions

- Stage reordering: drag-and-drop vs explicit Up/Down buttons. Recommendation: explicit buttons (Iced state-model stability).
- External include-file conflict: how the UI responds if the include file changes while Studio is open. Recommendation: `apply_reloaded_config` short-circuit (Batch 2a) handles it.
- Design tokens: confirm whether custom StageFlags widgets are needed or standard Checkbox + Row suffices.

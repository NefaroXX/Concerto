# Text Presentation & Animation Recommendations

> **Reference doc, not an ADR.** Design guidance for how text appears and
> animates in Concerto's desktop (Iced) and CLI (ratatui) frontends.
> Date: 2026-09-03.

---

## Starting Points

### Desktop (Iced)

- **MarkdownDoc** for rendered markdown content (headings, lists, code blocks).
- **CircuitBackground** behind the chat pane — subtle, non-distracting texture.
- **Typewriter cadence**: 8 characters at 16 ms intervals. This is the
  baseline pacing all desktop animations reference.

### CLI (ratatui)

- **Flat lines** — ratatui is immediate-mode; no rich markdown layout.
- Same 8-ch / 16 ms cadence for parity with the desktop feel.

---

## Hard Constraints

| Constraint | Detail |
|---|---|
| **Iced 0.14 alpha-only** | No alpha channel in `Color::from_rgb` family; use palette RGBA or transparent variants only. |
| **ratatui immediate-mode** | Every frame is a full redraw; animation state must be held externally (animation timer or frame counter), not in widget state. |
| **Palette rule** | All colors from `theme.palette.*`. No `Color::from_rgb`, `Color::BLACK`/`WHITE`/`TRANSPARENT`, or hex literals in views/ui code. `widgets/` and `theme/` are exempt. |
| **Parity cadence** | Desktop and CLI share the same timing constants (8 ch / 16 ms). Divergence requires an ADR. |

---

## Three Directions

### 1. Score

Typewriter reveal with musical-score timing. Characters appear in rhythm,
with subtle weight variation (bold on beat, lighter off-beat). Feels
intentional and crafted.

### 2. Baton

Handoff animation — text streams in from a cursor point, then "passes" to
the next block. Emphasizes flow and continuity between assistant turns.
Good for long multi-paragraph responses.

### 3. Blueprint

Technical, wireframe aesthetic. Text appears with a faint grid/scan-line
overlay. Feels precise and engineering-oriented. Pairs well with
CircuitBackground.

---

## Recommended Pick

**Score identity + Blueprint texture + Baton seasoning.**

Combine the best of all three:

- **Score identity**: Typewriter rhythm with weight variation gives the
  text a crafted, intentional feel. This is the primary personality.
- **Blueprint texture**: A faint scan-line or grid overlay on the
  chat area adds visual depth without competing with content. Keeps the
  engineering tone.
- **Baton seasoning**: For multi-paragraph responses, use a subtle
  handoff cue (brief cursor blink or fade) between paragraphs to signal
  "continuing" rather than "starting fresh."

### Signature Details

- **Signature text**: First token of each assistant turn renders slightly
  larger or bolder for ~384 ms (24 ticks × 16 ms), then settles. Signals
  "new turn" without a full animation. Lengthened from the initial ~200 ms
  design: the 8 ch/16 ms reveal (~500 chars/s) outraces typical provider
  streaming, so the first freshly rendered lines are the ones a user reads
  first — a longer emphasis makes the turn start register as new.
- **Signature animation**: A brief horizontal line wipe (left-to-right,
  ~128 ms) at the top of each new assistant turn. Subtle enough to miss if
  you're not looking; present enough to create rhythm. After the animation
  settles, the rule persists as a muted full-width hairline on the finished
  reply (`chat.rs::line_wipe_settled`) — the durable "new turn" marker that
  survives the run instead of the text reverting to the old plain look.

---

## Prototype List

Ordered by implementation dependency. Each has a fallback if blocked.

| # | Prototype | Depends on | Fallback |
|---|---|---|---|
| 1 | Typewriter timer (8 ch / 16 ms) | Animation clock | Instant reveal (no animation) |
| 2 | Weight-variation render (bold/light) | Iced text spans | Uniform weight |
| 3 | CircuitBackground behind chat | Iced custom widget | Solid palette background |
| 4 | Scan-line / grid overlay | Iced opacity layer | None (skip texture) |
| 5 | Paragraph handoff cue | Multi-paragraph detection | No cue (all-at-once) |
| 6 | First-token emphasis (size/bold) | Token boundary detection | No emphasis |
| 7 | Horizontal line wipe | Iced canvas or animated width | Fade-in (simpler) |

---

## Anti-Patterns

1. **Per-character color cycling** — flashy, distracting, and
   palette-breaking. Never do this.
2. **Randomized timing** — undermines the Score rhythm. All timing is
   deterministic and repeatable.
3. **Animation on every re-render** — ratatui immediate-mode means
   animation state must be frame-counted or time-stamped, not
   triggered by widget diff.
4. **Palette bypass for "special" animations** — no hex colors, no
   `Color::from_rgb`, no `Color::BLACK`. Every animation color comes
   from `theme.palette.*`. No exceptions.

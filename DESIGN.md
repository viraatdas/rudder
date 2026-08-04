# Design

Recorded from the built site (`site/`), not from intention. Update it when the
built world changes.

## Theme

Light, single mode. The scene that forced it: a developer at a desk in daylight
with a terminal already full-screen, glancing at a page to decide whether this
tool is worth installing. The dark terminal landing is what this category always
ships; the surface it actually belongs to is the printed working document that
sits next to the terminal.

The world is a **studio track sheet**: the ruled paper form where each track of a
recording is logged on its own row, captured in isolation, and nothing is summed
until the mixdown. That maps to the product exactly, so the page is drawn as a
form rather than as a marketing page with a form on it.

Direction seed: `9b5209e6` (candidate 4 of the derived list). The contract lives
in the opening HTML comment of `site/index.html`.

## Color

Strategy: **restrained**. Neutrals plus one working ink; state color is semantic
and never decorative. All values are OKLCH.

| Token | Value | Role |
| --- | --- | --- |
| `--stock` | `oklch(99.2% 0 0)` | The sheet. True white at chroma 0. |
| `--stock-band` | `oklch(97.2% 0.003 235)` | Ruled band, code blocks, row hover. |
| `--stock-sink` | `oklch(94.8% 0.004 235)` | Recessed fill. |
| `--ink` | `oklch(19% 0.014 250)` | Headings and primary text. |
| `--ink-2` | `oklch(41% 0.012 250)` | Body copy. |
| `--ink-3` | `oklch(52% 0.011 250)` | Labels and secondary. Held at 52% so it still clears 4.5:1 on stock. |
| `--rule` | `oklch(88% 0.005 240)` | The hairline the form is drawn with. |
| `--rule-strong` | `oklch(30% 0.010 245)` | Section and table opening rules. |
| `--work` | `oklch(47.5% 0.078 202)` | The working ink. Preserved brand teal (`#0c6a72`). |

**The ground is deliberately not cream.** Warm near-white plus a serif is the
house style of generated pages in this category; a working form printed on stock
is plain, not warm. Neutrals lean very slightly cool, toward the ink, never warm.

State colors carry meaning only, and every one is paired with its word in the
markup so nothing is conveyed by color alone:

| Token | Meaning | Contrast on stock | Rendered today |
| --- | --- | --- | --- |
| `--st-running` | running | 6.40:1 | yes |
| `--st-review` | done, ready to merge | 5.52:1 | yes |
| `--st-merged` | merged | 6.08:1 | yes |
| `--st-conflict` | conflict | 6.36:1 | defined, unused |
| `--st-idle` | idle | 4.94:1 | defined, unused |

`.state` renders at 0.7rem, which WCAG counts as small text, so every one of these
needs 4.5:1 rather than 3:1. `--st-idle` was 62% lightness (3.55:1) and would have
failed the moment it was used; it is held at 54%. Measured values are OKLCH →
OKLab → linear sRGB → WCAG relative luminance, against `--stock` and re-checked
against `--stock-band` since rows tint on hover.

Copy note: the words in the Meaning column must match shipped statuses
(`running / done / merged / failed / stopped / paused / orphaned / migrated`).
An earlier draft used "needs review", which is not a state Rudder has and implied
an approval gate that does not exist.

## Typography

Two families on a contrast axis, no third.

- **Archivo** (`--font-form`), weights 400/500/600 — a grotesque drawn for forms
  and high-performance printing, which is the track sheet's own lettering. All
  headings and UI.
- **JetBrains Mono** (`--font-mono`), 400/500 — everything the machine says:
  commands, paths, states, field labels, keys. It is what the terminal renders
  in, so it is measurement rather than costume.

Scale: `h1` `clamp(2.4rem, 5.4vw, 4.15rem)` at `-0.032em`; `h2`
`clamp(1.6rem, 2.9vw, 2.3rem)`; `h3` `1.06rem`. Body 16px/1.55. Measure capped at
`--measure: 68ch`. `text-wrap: balance` on headings, `pretty` on prose.

## Layout

The page is one continuous ruled form. Blocks are separated by **rules and
space**, never by cards with shadows — there are no box shadows in the system at
all, and `--radius` is 2px.

- Shell: `.page`, max 1240px, fluid side padding.
- Anything tabular in life is a real `<table>`: the track sheet, the command
  model, the docs reference.
- Docs: three columns (`15rem` nav / content / `12.5rem` on-this-page), collapsing
  to two at 1080px and one at 760px.
- **Tables stack rather than scroll on narrow screens.** Below 720px each row
  becomes a labelled block via `data-label` and `td::before`. Horizontal scroll
  hid the state column on a phone, which is the column the sheet exists to show.

## Motion

One authored moment, in `site/motion.css`: the track rows ink in with a short
stagger and the sum-bus rule draws itself left to right, once, on load. That is
the form's native motion — a sheet is filled in a row at a time. Nothing animates
on scroll.

The only loop is the running-state mark, which pulses because that row is live.

Animations run **from** the final state, so a headless render, a background tab
or a failed script still ships a complete page. Everything is inside
`@media (prefers-reduced-motion: no-preference)`.

Easing: `--ease-expo` / `--ease-quint`. No bounce.

## Components

- `.sheet-head` — the sheet's title plus the run's stamped facts (`<dl>`).
- `.tracks` — the track sheet. One row per agent, closed by `.bus`.
- `.state` — a stamp: a mark plus its word. `.state--merged` uses a clip-path
  cross to read as resolved.
- `.install` — the one primary action, a bordered command with a copy button that
  reports failure honestly rather than claiming success.
- `.movements`, `.desk`, `.options` — ruled grids, not card grids.
- `.note` (docs) — a ruled aside, top and bottom rules, no tint and no colored
  side border.
- `kbd` — real keys, bordered, in the machine face.

## Accessibility

WCAG AA. Body and label colors chosen against `--stock` to clear 4.5:1. Skip link
on both surfaces. Visible focus ring (`2px solid var(--work)`, offset 2). State is
never color-only. Wide content scrolls inside `.sheet-wrap` / `.table-wrap` so the
body never scrolls horizontally. Reduced-motion renders the final state.

## Build

`site/` is static, no framework. The docs are generated so the sidebar cannot
drift between pages:

```sh
node site/build-docs.mjs
```

Content lives in the `PAGES` table in that file. Deploy is `vercel --prod` from
`site/`.

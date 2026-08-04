# Product

## Register

brand

## Users

Developers working on **one codebase with several changes in flight at once**. They
are not running a single agent in a single chat; they have three or four things they
want done to the same repo simultaneously, and the thing standing in their way is not
model quality, it is collision: stashing, branch juggling, half-finished edits in the
working tree, and agents overwriting each other's files.

Their context is a real repository with real merge pressure, a terminal they live in,
and a goal they can describe but do not want to babysit. They think in tasks and
dependencies. They care about staying in control of what reaches main.

## Product Purpose

Rudder is a terminal orchestrator for coding agents. Every agent runs in its own jj
workspace, so parallel work on the same repository genuinely cannot touch other work
until the user merges it. You type a task and get one isolated worker; you type
`/plan` and get an orchestrator that breaks a goal into a DAG and runs it; you type
`/main` when you actually want an agent in your real checkout. Review and merge happen
in the same dashboard.

The landing page exists so a developer immediately understands the model — isolate,
run in parallel, review, merge — and believes it will hold up on their repo. Success:
they install it, or they remember it as "the one where parallel agents don't collide."

## Brand Personality

A precise instrument in daylight. The voice of a workbench, not a hype deck: a ship's
helm, a mixing console, an air-traffic display, seen under working light rather than
in a darkened room. Confident and technical but warm. It shows the machine honestly —
monospace, node IDs, real keys, real diffs — without costuming itself as a generic dev
tool. Three words: precise, steering, alive.

## Anti-references

- **White SaaS template.** Stripe-clone white page, icon-heading-text feature-card
  grids repeated down the page, soft drop shadows, rounded everything. Being light
  must not collapse into this; the design is light because a workbench is lit, not
  because SaaS pages are white.
- **Generic dark dev-tool landing.** GitHub-dark background, neon glow, the CLI-tool
  template everyone ships.
- **Competitor mimicry.** superset.sh is a direct competitor with a near-identical
  category pitch ("run 10+ parallel coding agents"). Its documentation information
  architecture is a fair reference; its landing page voice and volume-first framing
  are not. Rudder's claim is isolation, not agent count.
- **Editorial-magazine affectation.** Display-serif, italics and drop caps on a product
  that is not a magazine.

## Design Principles

- **Show the product's actual object.** The task DAG, the panes, and a real diff are
  the heroes. Animate the real model (isolate, parallel, review, merge); do not
  decorate around it.
- **Light as a working surface.** Paper-white ground, near-black ink, hairline rules.
  Anything the machine says is monospace. No card shadows standing in for hierarchy.
- **Lead with isolation, not volume.** The pitch is several changes to one repo that
  never touch each other until you say so. Throughput is a consequence, not the claim.
- **Color is semantics, not decoration.** Reserved for state: merged, running, needs
  review, conflict. Never tint for mood.
- **Honest, plain copy.** No em dashes, no "massively parallel", no hedging. Only
  claims that hold on a real repo today.

## Accessibility & Inclusion

WCAG AA on all text (body >=4.5:1, large >=3:1). Every animation has a
`prefers-reduced-motion` fallback that renders the final state statically, and content
is never gated behind a reveal transition. State is never conveyed by color alone; a
status label always accompanies the color. Keyboard-focusable interactive elements with
visible focus rings. Wide content scrolls inside its own container so the page body
never scrolls horizontally.

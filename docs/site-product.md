# Product

## Register

brand

## Users

Developers who run coding agents (Claude Code, Codex) and want more than a single
chat transcript. They work in the terminal, think in tasks and dependencies, and
care about staying in control of what an agent changes. Their context: a real repo,
real merge pressure, and a goal they can describe but don't want to babysit.

## Product Purpose

Rudder is a terminal orchestrator for coding agents. You describe a goal; it plans
the work into a task DAG you can edit before it runs, executes Claude Code and Codex
agents in parallel across isolated jj workspaces, then keeps planning, review, and
merge in one dashboard. The landing page exists to make a developer immediately
understand the model (plan, run in parallel, review, merge) and feel that Rudder is
precise and in control, not another chat wrapper. Success: the visitor installs it,
or at least remembers it as "the one with the live plan you steer".

## Brand Personality

Precise, orchestral, in control. The voice of an instrument, not a hype deck: a
ship's helm, a mixing console, an air-traffic display. Confident and technical but
warm; it shows the machine honestly (monospace, node IDs, real keys) without
costuming itself as a generic dev tool. Three words: precise, steering, alive.

## Anti-references

- Generic dark dev-tool landing: GitHub-dark background, neon glow, Space Grotesk,
  the same CLI-tool template everyone ships.
- White SaaS template: Stripe-clone white page with identical icon-heading-text
  feature-card grids.
- Editorial-magazine affectation: display-serif + italic + drop caps on a product
  that isn't a magazine.

## Design Principles

- Show the product's actual object. The task DAG is the hero; animate the real
  model (plan, parallel, review, merge), don't decorate around it.
- Two voices, two surfaces. The human voice is light and editorial; the machine
  voice lives in a dark instrument "screen". The contrast carries the brand.
- Restraint with one big swing. Refined and quiet overall, with a single ambitious
  page-load choreography that earns its place.
- Color is semantics, not decoration. Cyan = orchestrate, green = merged/done,
  amber = review, magenta = control. Never tint for mood alone.
- Honest, plain copy. No em dashes, no "massively parallel" hype, no hedging.

## Accessibility & Inclusion

WCAG AA contrast on all text (body >=4.5:1, large >=3:1), including on the dark
screen band. Every animation has a `prefers-reduced-motion` fallback that renders
the final state statically. DAG state is never conveyed by color alone; status
labels accompany the color. Keyboard-focusable interactive elements with visible
focus rings.

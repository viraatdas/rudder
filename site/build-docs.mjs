#!/usr/bin/env node
// Generates site/docs/*.html from the PAGES table below.
//
// The docs are static HTML with no framework, but the sidebar appears on every
// page. Hand-maintaining six copies of it guarantees they drift, so the nav and
// the shell live here once and the pages are emitted. Run after editing:
//
//   node site/build-docs.mjs
//
// Content is prose written by hand; this file is a stamper, not a CMS.

import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const outDir = path.join(here, "docs");

const NAV = [
  {
    title: "get started",
    items: [
      ["", "Overview"],
      ["install", "Install"],
      ["first-agent", "Your first agent"],
    ],
  },
  {
    title: "core",
    items: [
      ["workspaces", "Workspaces"],
      ["plans", "Plans and the DAG"],
      ["gam", "Adversarial pairs (GAM)"],
    ],
  },
  {
    title: "reference",
    items: [
      ["commands", "Commands and keys"],
      ["faq", "FAQ"],
    ],
  },
];

const ORDER = NAV.flatMap((section) => section.items.map(([slug]) => slug));

const PAGES = {
  "": {
    title: "Overview",
    lede: "Run coding agents in parallel on one repo. Each works in its own copy, so they cannot collide.",
    toc: [
      ["start", "Start in 60 seconds"],
      ["the-loop", "The whole loop"],
      ["what-it-is", "What it is"],
      ["the-problem", "Why isolation"],
      ["requirements", "Requirements"],
    ],
    body: `
<h2 id="start">Start in 60 seconds</h2>
<pre><code><span class="prompt">$ </span>npm install -g @viraatdas/rudder@latest
<span class="prompt">$ </span>cd ~/code/your-project
<span class="prompt">$ </span>rudder</code></pre>
<p>Type a task into the box at the bottom and press Enter:</p>
<pre><code>fix the login redirect losing the query string</code></pre>
<p>That is it. One agent is now working in its own copy of the repo. Type another
task and a second one starts beside it, unable to see or overwrite the first.</p>
<p>When a row says <span class="state state--review">done</span>, press
<kbd>m</kbd>. Rudder shows you the diff. Press <kbd>m</kbd> again and it lands.</p>

<figure class="docs-shot">
  <div class="screen">
    <div class="screen-chrome"><span class="mono">rudder</span><span class="mono">~/code/api</span></div>
    <div class="frame frame-static">
      <div class="pane pane-list">
        <div class="pane-label">agents</div>
        <div class="row-group">/main<span>1 agent</span></div>
        <div class="row"><span class="row-task">tag the release</span><span class="state state--running">running</span></div>
        <div class="row-group">workspaces<span>3 agents</span></div>
        <div class="row is-selected"><span class="row-task">rate-limit the public API</span><span class="state state--running">running</span></div>
        <div class="row"><span class="row-task">port the settings screen</span><span class="state state--review">done</span></div>
        <div class="row"><span class="row-task">drop the legacy session table</span><span class="state state--merged">merged</span></div>
      </div>
      <div class="pane pane-main">
        <div class="pane-label">rate-limit the public API</div>
        <pre class="tty"><span class="dim">workspace</span> .rudder-workspaces/rate-limit

<span class="ok">●</span> Read src/server/middleware.ts
<span class="ok">●</span> Edit src/server/rate-limit.ts <span class="add">+64</span>
<span class="ok">●</span> Bash npm test <span class="dim">· 41 passed</span>

<span class="run">▌</span> writing the burst-window test…</pre>
      </div>
    </div>
  </div>
  <figcaption>
    Three agents in their own workspaces, one in your checkout, and the selected
    agent's own terminal beside them.
  </figcaption>
</figure>

<h2 id="the-loop">The whole loop</h2>
<table class="docs-table">
  <thead><tr><th>you do</th><th>what happens</th></tr></thead>
  <tbody>
    <tr><td>type a task</td><td>One isolated agent starts in its own workspace.</td></tr>
    <tr><td>Option-1 / 2 / 3</td><td>Agents list · the agent's terminal · the task box.</td></tr>
    <tr><td>j / k</td><td>Move between agents.</td></tr>
    <tr><td>m</td><td>Show me the diff. Then: land it.</td></tr>
    <tr><td>u</td><td>Undo that.</td></tr>
    <tr><td>x</td><td>Stop an agent.</td></tr>
  </tbody>
</table>
<p>Everything else in these docs is detail on top of those six lines.</p>

<div class="note">
  <span class="field-label">the one thing to know</span>
  <p><strong>Nothing lands that you have not been shown.</strong> The first
  <kbd>m</kbd> opens the diff; the second one lands it. On a repo with a GitHub
  remote, landing means a draft pull request rather than a local merge - and
  Rudder asks once, naming the remote, before it ever pushes.</p>
</div>

<h2 id="what-it-is">What it is</h2>
<p>Rudder is a terminal dashboard that starts, watches, reviews and merges coding
agents. It drives <strong>Claude Code</strong>, <strong>Codex</strong> and
<strong>opencode</strong>: it does not replace them. Each agent runs in its own real
terminal, with its own native prompts, inside a pane you can talk to.</p>
<p>What Rudder adds is everything around the agent: where it works, what it can
touch, how you see what it did, and how that reaches your main branch.</p>

<h2 id="the-problem">Why isolation</h2>
<p>Running one agent is easy. Running four on the same repository is where it falls
apart: they stash over each other, leave half-finished edits in your working tree,
and you lose track of which branch holds what.</p>
<p>Rudder's answer is a <strong>jj workspace</strong> per agent: a private copy of the
repo, gitignored, beside your checkout and never in it. Four agents editing the same
file are four separate trees. Nothing merges until you say so, and your own checkout
stays exactly where you left it the whole time.</p>

<h2 id="requirements">Requirements</h2>
<ul>
  <li>macOS or Linux</li>
  <li>Node 18 or newer</li>
  <li>A git repository</li>
  <li><a href="https://jj-vcs.github.io/jj/latest/">jj</a>, installed for you on
  first install if it is missing</li>
  <li>At least one of the Claude Code, Codex or opencode CLIs, signed in</li>
</ul>
<p>Next: <a href="/docs/first-agent">walk through your first agent</a>, or read
<a href="/docs/workspaces">how the isolation works</a>.</p>
`,
  },

  install: {
    title: "Install",
    lede: "One global npm install. Rudder checks for jj on the way in and installs it if you do not have it.",
    toc: [
      ["install-it", "Install it"],
      ["jj", "About jj"],
      ["agents", "Agent CLIs"],
      ["update", "Updating"],
    ],
    body: `
<h2 id="install-it">Install it</h2>
<pre><code><span class="prompt">$ </span>npm install -g @viraatdas/rudder@latest</code></pre>
<p>Then, from inside any git repository:</p>
<pre><code><span class="prompt">$ </span>cd ~/code/your-project
<span class="prompt">$ </span>rudder</code></pre>
<p>That opens the dashboard. On a repository Rudder has not seen before, it sets up
the jj colocation it needs and leaves your git history alone.</p>

<h2 id="jj">About jj</h2>
<p>Rudder uses <a href="https://jj-vcs.github.io/jj/latest/">jj (Jujutsu)</a> as its
isolation mechanism. jj sits <strong>alongside</strong> git in the same repository,
so your git remotes, branches and history keep working exactly as before, and your
teammates never need to know it is there.</p>
<p>The install script checks for jj and installs it if it is missing. It never fails
the install: if it cannot, it prints how to install jj yourself and exits cleanly.</p>

<h2 id="agents">Agent CLIs</h2>
<p>Rudder drives agent CLIs you have already installed and signed into. Install
whichever you want to use:</p>
<table class="docs-table">
  <thead><tr><th>backend</th><th>what to install</th></tr></thead>
  <tbody>
    <tr><td>claude</td><td>Claude Code</td></tr>
    <tr><td>codex</td><td>OpenAI Codex CLI</td></tr>
    <tr><td>opencode</td><td>opencode</td></tr>
  </tbody>
</table>
<p>You do not need all three. Rudder detects what is present and offers only those.</p>

<h2 id="update">Updating</h2>
<pre><code><span class="prompt">$ </span>npm install -g @viraatdas/rudder@latest</code></pre>
<p>Rudder tells you in the dashboard when a newer version is published.</p>
`,
  },

  "first-agent": {
    title: "Your first agent",
    lede: "Type a task, watch it work in its own tree, read the diff, merge it. This is the whole loop.",
    toc: [
      ["start", "Start one"],
      ["watch", "Watch it"],
      ["review", "Review the diff"],
      ["merge", "Merge it"],
      ["more", "Then do four at once"],
    ],
    body: `
<h2 id="start">Start one</h2>
<p>Open Rudder in your repository and type what you want, in plain language, into
the task box at the bottom:</p>
<pre><code>fix the login redirect losing the query string</code></pre>
<p>Press Enter. That is the default path and it means one thing: <strong>one isolated
agent</strong>. Rudder creates a workspace for it, launches your chosen backend
inside, and adds a row to the agents list.</p>

<div class="note">
  <span class="field-label">why plain text is the safe default</span>
  <p>Typing a task can never touch your working tree. If you actually want an agent
  in your real checkout, that is <code>/main</code>, and you have to ask for it.</p>
</div>

<figure class="docs-shot">
  <div class="screen">
    <div class="screen-chrome"><span class="mono">rudder</span><span class="mono">~/code/api</span></div>
    <div class="frame frame-static">
      <div class="pane pane-list">
        <div class="pane-label">agents</div>
        <div class="row-group">/main<span>1 agent</span></div>
        <div class="row"><span class="row-task">tag the release</span><span class="state state--running">running</span></div>
        <div class="row-group">workspaces<span>3 agents</span></div>
        <div class="row is-selected"><span class="row-task">rate-limit the public API</span><span class="state state--running">running</span></div>
        <div class="row"><span class="row-task">port the settings screen</span><span class="state state--review">done</span></div>
        <div class="row"><span class="row-task">drop the legacy session table</span><span class="state state--merged">merged</span></div>
      </div>
      <div class="pane pane-main">
        <div class="pane-label">rate-limit the public API</div>
        <pre class="tty"><span class="dim">workspace</span> .rudder-workspaces/rate-limit

<span class="ok">●</span> Read src/server/middleware.ts
<span class="ok">●</span> Edit src/server/rate-limit.ts <span class="add">+64</span>
<span class="ok">●</span> Bash npm test <span class="dim">· 41 passed</span>

<span class="run">▌</span> writing the burst-window test…</pre>
      </div>
    </div>
  </div>
  <figcaption>
    Three agents in their own workspaces, one in your checkout, and the selected
    agent's own terminal beside them.
  </figcaption>
</figure>

<h2 id="watch">Watch it</h2>
<p>Press <kbd>Option-2</kbd> for the worker pane. That is the agent's own terminal,
unmodified: its prompts, its output, its permission questions. You can talk to it
directly. <kbd>Option-1</kbd> goes back to the agents list.</p>
<p>With several agents running, <kbd>Option-[</kbd> and <kbd>Option-]</kbd> step
between them from any pane.</p>

<h2 id="review">Review the diff</h2>
<p>When the row says <span class="state state--review">done</span>, select it
and press <kbd>v</kbd>. That is the live <code>jj diff</code> of that agent's
workspace: exactly what it changed, and nothing of it is in your checkout yet.</p>

<h2 id="merge">Merge it</h2>
<p>Press <kbd>m</kbd>. Rudder confirms, then merges that workspace into your branch.
The row becomes <span class="state state--merged">merged</span>.</p>
<p>Changed your mind? <kbd>u</kbd> undoes the merge, using jj's operation log, and
puts the change back where it was.</p>

<h2 id="more">Then do four at once</h2>
<p>Type another task. And another. Each gets its own workspace and its own row, all
running at the same time, none of them able to see each other's edits. Merge them in
whatever order they finish.</p>
<p>When the work is big enough to need ordering, that is a <a href="/docs/plans">plan</a>.</p>
`,
  },

  workspaces: {
    title: "Workspaces",
    lede: "The isolation mechanism. Every agent works in its own jj workspace, so parallel work on one repository genuinely cannot collide.",
    toc: [
      ["what", "What a workspace is"],
      ["where", "Where they live"],
      ["merging", "How merging works"],
      ["conflicts", "Conflicts"],
      ["main", "The main checkout"],
    ],
    body: `
<h2 id="what">What a workspace is</h2>
<p>A jj workspace is a second working copy of the same repository, with its own
working-copy commit. Edits in one are invisible to the others until they are merged.
Rudder gives every agent one.</p>
<p>This is the reason several agents can work on the same files at the same time. They
are not coordinating, taking turns, or locking anything. They cannot reach
each other.</p>

<h2 id="where">Where they live</h2>
<pre><code>your-repo/
  .rudder-workspaces/
    fix-login-redirect/
    port-settings-form/
    drop-sessions-table/</code></pre>
<p>They sit beside your checkout, never nested inside it, so your editor, your build
and your git status are unaffected by what the agents are doing.</p>

<h2 id="merging">How merging works</h2>
<p>Merging a row runs <code>jj new</code> against your branch and that agent's change.
It is a real merge commit, not a squash of the working copy, so the agent's history
stays intact and the merge is a normal thing to inspect or undo.</p>
<p><kbd>m</kbd> merges the selected row. <kbd>M</kbd> merges everything that is ready.
<kbd>u</kbd> undoes a merge through jj's operation log.</p>

<div class="note">
  <span class="field-label">merged is local</span>
  <p>Merging integrates into your <strong>local</strong> git only. Rudder never
  pushes to a remote and never deploys. Nothing is live until you push.</p>
</div>

<h2 id="conflicts">Conflicts</h2>
<p>jj records conflicts <em>in the change</em> rather than refusing the merge. A
conflicted merge still completes; the conflict is then a thing you can see, resolve,
or undo, instead of a wall you hit mid-operation.</p>
<p>This is what lets Rudder integrate several overlapping agents without stopping the
whole session on the first collision.</p>
<p>Rudder settles the conflicts that need no judgment on its own: its own coordination
files union their entries, <code>package.json</code> deep-merges its dependency maps,
lock files take the most complete side. Source code is never merged mechanically -
guessing at a real disagreement is worse than stopping. What is left starts a resolver
agent in the row that already owns the work, rather than a prompt waiting for a
keypress. An unanswered prompt used to park finished nodes in review and leave
everything downstream of them unable to launch.</p>

<h2 id="main">The main checkout</h2>
<p>Some jobs are genuinely about your working tree: tagging a release, running a
deploy, poking at something by hand. Those are <code>/main</code>, and they run in
your real checkout with nothing to merge.</p>
<p>Several <code>/main</code> agents can run at once. Rudder allows it because it is
sometimes what you want; managing what they do to each other is yours.</p>
`,
  },

  plans: {
    title: "Plans and the DAG",
    lede: "When a goal needs several steps in a particular order, /plan gives you an orchestrator: it proposes a task graph, you edit it, then it runs and merges the nodes as their dependencies land. Several plans can run at once, each in its own pane.",
    toc: [
      ["start", "Starting a plan"],
      ["hardening", "Hardening"],
      ["edit", "Editing before it runs"],
      ["deps", "Hard and soft edges"],
      ["steer", "Steering a running plan"],
    ],
    body: `
<h2 id="start">Starting a plan</h2>
<pre><code>/plan add billing with Stripe, including the webhook handler</code></pre>
<p>A read-only planner inspects the repository, asks a round of questions if the goal
is ambiguous, and proposes a graph of tasks. <strong>Nothing is written to your code
until you approve it.</strong></p>
<p>Run <code>/plan</code> again for a different goal and you get a
<strong>second orchestrator beside the first</strong>, with its own graph, its own
pane, and its own workspaces. The two do not share state and merge independently; if
they touch the same files, that is an ordinary merge conflict, handled the same way
as any other. A plan that is finished - nothing queued, nothing running, nothing at
the gate - is reused rather than piling up, so repeatedly planning does not leave a
row per past goal.</p>

<h2 id="hardening">Hardening</h2>
<p>Workers report follow-ups when they finish - a test worth adding, a rough edge
worth smoothing. Rudder collects those into a backlog instead of scheduling each one
as its own node, and runs them <strong>clumped by file</strong> once the current phase
has merged.</p>
<p>The reason is that hardening findings land on the same files by nature. One node
per finding meant several agents racing to edit the same file and conflicting with
each other - manufacturing the very conflicts they then had to resolve. As one clump
they are edits in a single workspace, and one agent fixes them together with the whole
file in view instead of several half-fixing it.</p>

<h2 id="edit">Editing before it runs</h2>
<p>The plan is a draft, not a verdict. Press <kbd>v</kbd> on the orchestrator to open
plan review and edit any node in place: its title, goal, success criteria,
dependencies, and the exact prompt the agent will run.</p>
<table class="docs-table">
  <thead><tr><th>key</th><th>in plan review</th></tr></thead>
  <tbody>
    <tr><td>v</td><td>open plan review on the orchestrator</td></tr>
    <tr><td>Tab</td><td>move between fields on a node</td></tr>
    <tr><td>Ctrl-S</td><td>save your edits to the graph</td></tr>
    <tr><td>Ctrl-Enter</td><td>approve and launch</td></tr>
    <tr><td>Esc</td><td>back to the orchestrator</td></tr>
  </tbody>
</table>

<h2 id="deps">Hard and soft edges</h2>
<p>A <strong>hard</strong> edge means a node waits for its parent to merge. A
<strong>soft</strong> edge is context only and never gates a launch. Work that can run
in parallel does, and the plan blocks only where one task truly depends on another.</p>
<p>As each node merges, its children unblock and launch on their own.</p>

<h2 id="steer">Steering a running plan</h2>
<p>Talk to the orchestrator's own pane. With several plans running, the pane is what
says which plan you mean, so a message reaches that plan and no other. Select the orchestrator row, type into it, and
it will add tasks, re-plan structurally, stop or re-goal specific workers, or explain
what is happening.</p>

<div class="note">
  <span class="field-label">the task box is not the orchestrator</span>
  <p>Typing into the task box always starts a new standalone agent, even while a plan
  is running. To change a plan, talk to that plan's pane.</p>
</div>
`,
  },

  gam: {
    title: "Generative Adversarial Model (GAM)",
    lede: "GAM puts two models on one task: a generator that writes the code, and an adversarial reviewer from a different provider that can only argue with it. The reviewer cannot edit a single file. You start one with /gam.",
    toc: [
      ["start", "Starting a pair"],
      ["steering", "How the reviewer steers the work"],
      ["why", "Why two models"],
      ["round", "What a round is"],
      ["verdicts", "The three verdicts"],
      ["disagree", "When the generator disagrees"],
      ["stops", "When it stops"],
      ["why-it-matters", "Why this is worth the second model"],
    ],
    body: `
<h2 id="start">Starting a pair</h2>
<pre><code>/gam rewrite the retry logic to use exponential backoff</code></pre>
<p>Two panes open side by side. The left half is the generator, running your current
model. The right half is the adversarial reviewer, which defaults to a
<strong>different provider</strong> so it is not the same model marking its own
homework: a Claude generator pairs with Codex, and a Codex or opencode generator
pairs with Claude.</p>

<figure class="docs-shot">
  <div class="screen">
    <div class="screen-chrome"><span class="mono">rudder</span><span class="mono">~/code/api</span></div>
    <div class="frame frame-gam frame-static">
      <div class="pane pane-gen">
        <div class="pane-label">gen &middot; claude sonnet &middot; revision 1 of 3</div>
        <pre class="tty"><span class="dim">workspace</span> .rudder-workspaces/retry

<span class="ok">&#9679;</span> Edit src/net/retry.ts <span class="add">+58</span>
<span class="ok">&#9679;</span> Bash npm test <span class="dim">&middot; 41 passed</span>

<span class="run">&#9612;</span> writing the jitter test&hellip;</pre>
      </div>
      <div class="pane pane-adv">
        <div class="pane-label">adv &middot; codex gpt-5.5 &middot; objected &middot; waiting</div>
        <pre class="tty"><span class="dim">read-only &middot; cannot edit any file</span>

<span class="ok">&#9679;</span> Read src/net/retry.ts
<span class="ok">&#9679;</span> Bash npm test -- jitter <span class="dim">&middot; 0 matched</span>

<span class="dim">41 passing tests do not touch the branch this</span>
<span class="dim">task exists for.</span></pre>
      </div>
    </div>
  </div>
  <figcaption>
    The generator on the left writes. The reviewer on the right reads, runs its own
    checks, and objects.
  </figcaption>
</figure>

<p>You can name the reviewer instead of taking the default, and run the pair in your
real checkout rather than an isolated workspace:</p>
<table class="docs-table">
  <thead><tr><th>you type</th><th>what you get</th></tr></thead>
  <tbody>
    <tr><td>/gam &lt;task&gt;</td><td>Reviewer picked for you, from the other provider.</td></tr>
    <tr><td>/gam codex &lt;task&gt;</td><td>Codex reviews, on its default model.</td></tr>
    <tr><td>/gam codex gpt-5.5 &lt;task&gt;</td><td>A named provider and model.</td></tr>
    <tr><td>/gam fable &lt;task&gt;</td><td>A bare model name works when Rudder recognises it.</td></tr>
    <tr><td>/gam main &lt;task&gt;</td><td>The pair runs in your checkout instead of a workspace.</td></tr>
  </tbody>
</table>
<p>Ordinary task words are never mistaken for a model. <code>/gam fix the auth
bug</code> keeps every one of those words as the task, because <code>fix</code> is
not a model name.</p>

<h2 id="steering">How the reviewer steers the work</h2>
<p>The reviewer is not a gate at the end. It reads the diff after every turn the
generator takes, while the work is still in motion, and what it sends back becomes the
generator's next instruction. A pair is a correction loop, not an inspection.</p>
<p>That is what the second model is for. Left alone, a coding agent drifts in ways it
cannot see from the inside: it settles on the first approach that compiles, treats the
happy path as the whole problem, marks a task done because the code it wrote runs, and
never revisits the framing it chose in its first thirty seconds. None of that shows up
as an error. It shows up as a confident summary.</p>
<p>So the reviewer is prompted to do the opposite of agreeing. Its instructions tell it
to refute first: to hunt for what is wrong, missing, oversimplified or untested, and to
check the generator's claims against the actual files rather than against its
narrative. Concretely, each round it is asked to establish</p>
<table class="docs-table">
  <thead><tr><th>what it checks</th><th>the failure it is looking for</th></tr></thead>
  <tbody>
    <tr><td>Does this do what was asked?</td><td>A subset delivered as the whole thing. It is handed your ORIGINAL task every round, so this is measured against what you wanted, not against what the last message was about.</td></tr>
    <tr><td>Was it actually verified?</td><td>A check described as run. The reviewer runs the tests itself rather than believing the transcript.</td></tr>
    <tr><td>What was not considered?</td><td>The edge the generator never looked at: the error path, the empty case, the concurrent one, the migration that has to happen first.</td></tr>
    <tr><td>Is the approach right?</td><td>A solution that works and should not survive. Cheaper to say in round one than after the diff has grown around it.</td></tr>
  </tbody>
</table>
<p>Objections come back as concrete, actionable requests naming a file and a symptom,
and they arrive as the generator's next prompt. The generator does not have to be
started over or re-briefed; it is mid-task, holding all its context, and it gets a
specific correction at the moment it can still act on it cheaply. That is the whole
mechanism: steering while the work is happening, rather than judging it once it is
finished.</p>

<h2 id="why">Why two models</h2>
<p>A model reviewing its own work agrees with itself. It has already decided the
approach was reasonable, and asking it to check that decision gets you a summary of
the decision rather than a test of it.</p>
<p>So the two halves never share a conversation. Each keeps its own session, and the
only things that cross between them are the original task, the diff, the reviewer's
objections, and the generator's replies. The reviewer never sees the reasoning that
produced the code, because a reviewer that has read the argument for a change is no
longer independent of it.</p>
<p>The asymmetry is the other half of the idea. The reviewer has no write access at
all, which means it cannot quietly fix what it dislikes and call the disagreement
settled. It has to make its case in words, and the generator has to be persuaded.</p>

<h2 id="round">What a round is</h2>
<p>A round begins when the generator finishes a turn. Rudder then hands the reviewer
a packet containing:</p>
<table class="docs-table">
  <thead><tr><th>what is in the packet</th><th>why</th></tr></thead>
  <tbody>
    <tr><td>the original task</td><td>Every round is anchored on what <em>you</em> typed, never on the last thing the reviewer asked for, so a long argument cannot drift off the ask.</td></tr>
    <tr><td>the current diff</td><td>The reviewer judges the code, not the generator's account of it. Very large diffs are truncated with a visible marker, and the reviewer is told not to object to what it cannot see.</td></tr>
    <tr><td>the generator's last reply</td><td>Only when the generator pushed back, so the argument carries forward.</td></tr>
  </tbody>
</table>
<p>The reviewer is told to refute first: to hunt for what is wrong, missing,
oversimplified or untested, and to check claims against the actual files rather than
the transcript. It ends its turn with a verdict.</p>

<h2 id="verdicts">The three verdicts</h2>
<table class="docs-table">
  <thead><tr><th>verdict</th><th>what happens next</th></tr></thead>
  <tbody>
    <tr><td>accept</td><td>The pair settles. Reserved for work that plainly satisfies the original task with no blocking defect the reviewer can demonstrate.</td></tr>
    <tr><td>revise</td><td>The objection is delivered to the generator as a message and a new round starts.</td></tr>
    <tr><td>escalate</td><td>The disagreement needs you: a scope dispute, two irreconcilable approaches, or a correct objection the generator keeps ignoring.</td></tr>
  </tbody>
</table>

<h2 id="disagree">When the generator disagrees</h2>
<p>The generator is not required to obey. It is told explicitly not to comply
silently with an objection it thinks is wrong, but to state why in one short
paragraph and keep implementing. Rudder lifts that reply out and puts it in front of
the reviewer on the next round.</p>
<p>This matters more than it sounds. A reviewer that gets its way automatically turns
every weak objection into a code change, and the work drifts toward whatever the
reviewer happened to notice. Giving the generator a way to win the argument is what
keeps the pair converging instead of wandering.</p>

<div class="note">
  <span class="field-label">a use worth knowing</span>
  <p>When one model refuses a task or quietly does a subset of it, the other one
  often does not. The reviewer is reading the diff against the original ask, so it
  is well placed to notice a job reported as done that was not, and to say so.</p>
</div>

<h2 id="stops">When it stops</h2>
<p>A pair runs at most <strong>four rounds</strong>. It ends early when the reviewer
accepts. It stops and asks for you when the reviewer escalates, when four rounds pass
without acceptance, or when the reviewer produces no readable verdict at all.</p>
<p>In every one of those cases <strong>both panes stay live</strong> and Rudder names
the reason. Nothing is discarded and nothing is merged behind your back: the
generator's work sits in its workspace exactly like any other agent's, and it lands
when you press <kbd>m</kbd>.</p>

<div class="note">
  <span class="field-label">cost</span>
  <p>A pair spends roughly twice what the same task costs alone, and takes longer in
  wall-clock time, because the reviewer reads a finished diff and the two halves
  cannot run at the same time. It earns that on work where being wrong is expensive,
  not on a rename.</p>
</div>

<h2 id="why-it-matters">Why this is worth the second model</h2>
<p>The ceiling on a single coding agent is not model quality, and it moves less with
each release than people expect. It is that a model cannot reliably audit its own
work. The failure is rarely a crash: it is quiet. A subset delivered as the whole
thing. A check described as run. A refusal dressed up as a completion. All three read
identically to a summary, and all three are caught by reading the diff, which is the
part you were hoping to do less of.</p>
<p>Rudder's answer everywhere else is isolation: work that cannot touch other work
until you say so. A pair is that same idea pointed at judgment rather than at files.
The reviewer cannot edit, comes from a different vendor, and is anchored on the task
you actually typed, so its agreement is worth something. A model that could quietly
fix what it disliked would never have to make its case, and you would never see the
disagreement.</p>
<p>What it does not do is remove you from the loop. It costs about twice, it runs
serially, and it is not a substitute for reading the diff before you press
<kbd>m</kbd>. What it changes is how much you have to catch unaided: the obvious
failures are argued out before they reach you, and the ones that survive arrive with
a reviewer's objection and a generator's answer attached.</p>
`,
  },

  commands: {
    title: "Commands and keys",
    lede: "Everything you can type into the task box, and every key the dashboard listens for.",
    toc: [
      ["typing", "What you type"],
      ["commands", "Commands"],
      ["keys", "Keys"],
      ["panes", "Panes"],
    ],
    body: `
<h2 id="typing">What you type</h2>
<table class="docs-table">
  <thead><tr><th>input</th><th>result</th></tr></thead>
  <tbody>
    <tr><td>plain text</td><td>One isolated agent in its own workspace. The default, and the only thing that cannot touch your checkout.</td></tr>
    <tr><td>/plan &lt;goal&gt;</td><td>An orchestrator that plans a task graph, then runs and merges it. Several can run at once, each in its own pane.</td></tr>
    <tr><td>/gam &lt;task&gt;</td><td>A Generative Adversarial Model pair: a generator that writes, and a read-only adversarial reviewer that argues with it. See <a href="/docs/gam">GAM</a>.</td></tr>
    <tr><td>/main &lt;task&gt;</td><td>An agent in your real checkout. Nothing to merge. <code>/m</code> is the same thing.</td></tr>
  </tbody>
</table>

<h2 id="commands">Commands</h2>
<table class="docs-table">
  <thead><tr><th>command</th><th>what it does</th></tr></thead>
  <tbody>
    <tr><td>/model</td><td>Set the backend, model and reasoning effort for the next agent. Effort runs low, medium, high, xhigh, max; xhigh is the default where a model offers it, and opencode takes none. A running agent keeps what it launched with.</td></tr>
    <tr><td>/fast</td><td>Toggle faster output on supported models.</td></tr>
    <tr><td>/gam</td><td>Start a Generative Adversarial Model (GAM) pair: a generator plus an adversarial reviewer. The reviewer defaults to the other provider; name one with <code>/gam codex gpt-5.5 &lt;task&gt;</code>, or add <code>main</code> to run the pair in your checkout.</td></tr>
    <tr><td>/resume</td><td>Continue an existing Claude, Codex or opencode chat as an agent. The picker lists this repo's recent chats and says which directory each one ran in. It lands in an isolated workspace like plain text does; add <code>--here</code> to continue it in your real checkout instead.</td></tr>
    <tr><td>/restore</td><td>Reopen a specific session id in a new pane.</td></tr>
    <tr><td>/share</td><td>Durable local context every agent reads. For tokens, URLs, env details.</td></tr>
    <tr><td>/goal</td><td>Set or change the session's overall goal.</td></tr>
    <tr><td>/usage</td><td>Token and cost usage for this session.</td></tr>
    <tr><td>/cloud</td><td>Move the fleet onto Rudder Cloud.</td></tr>
    <tr><td>/web</td><td>Open the web board for this project.</td></tr>
    <tr><td>/feedback</td><td>Send a report, with the local copy written first.</td></tr>
    <tr><td>/sound, /color</td><td>Notification sound and colour preferences.</td></tr>
  </tbody>
</table>

<h2 id="keys">Keys</h2>
<table class="docs-table">
  <thead><tr><th>key</th><th>action</th></tr></thead>
  <tbody>
    <tr><td>j / k</td><td>Move down and up the agents list.</td></tr>
    <tr><td>Enter</td><td>Focus the selected agent's pane.</td></tr>
    <tr><td>Option-[ / Option-]</td><td>Step to the previous or next agent from any pane.</td></tr>
    <tr><td>Option-h</td><td>Full-screen the focused pane; press again to bring the others back.</td></tr>
    <tr><td>v</td><td>Live jj diff of the selected agent.</td></tr>
    <tr><td>m</td><td>Merge the selected agent.</td></tr>
    <tr><td>M</td><td>Merge everything ready.</td></tr>
    <tr><td>u</td><td>Undo a merge.</td></tr>
    <tr><td>R</td><td>Review all.</td></tr>
    <tr><td>x</td><td>Stop the selected agent.</td></tr>
    <tr><td>b</td><td>Branch a new chat from this agent's session.</td></tr>
    <tr><td>g</td><td>Nest work under the selected node.</td></tr>
    <tr><td>o</td><td>Open the web board.</td></tr>
    <tr><td>P</td><td>Model picker for the selected row.</td></tr>
    <tr><td>dd</td><td>Delete the selected agent.</td></tr>
    <tr><td>cc</td><td>Clear merged rows.</td></tr>
  </tbody>
</table>

<h2 id="panes">Panes</h2>
<p><kbd>Option-1</kbd>, <kbd>Option-2</kbd> and <kbd>Option-3</kbd> always mean Agents,
Worker and Task.</p>
<p><kbd>Option-h</kbd> gives the focused pane the whole screen: no sidebar, no task
line, no gutters. Press it again to bring them back. Reaching for it from the task
line hands the screen to the worker, since the task input is no longer drawn, and
you land back on the task line when the panes return.</p>
<p>The worker pane belongs to the agent, so its keystrokes go to Claude or Codex, not
to Rudder. To send a dashboard key from inside it, use the <kbd>Ctrl-W</kbd> leader:
<kbd>Ctrl-W v</kbd> reviews, <kbd>Ctrl-W m</kbd> merges, <kbd>Ctrl-W 1/2/3</kbd>
switches panes.</p>
`,
  },

  faq: {
    title: "FAQ",
    lede: "The questions that come up in the first hour.",
    toc: [
      ["git", "Does this change my git repo?"],
      ["push", "Does it push or deploy?"],
      ["teammates", "Do my teammates need jj?"],
      ["cost", "What does it cost to run?"],
      ["crash", "What if it crashes?"],
    ],
    body: `
<h2 id="git">Does this change my git repository?</h2>
<p>jj is colocated with git in the same repository. Your git history, branches and
remotes keep working exactly as they did. Rudder adds workspaces under
<code>.rudder-workspaces/</code> and merges into your local branch; it does not rewrite
your history behind you.</p>

<h2 id="push">Does it push or deploy anything?</h2>
<p>It never deploys. Whether it pushes depends on the repo, and it tells you which
before it does anything.</p>
<p>With no GitHub remote, or without a signed-in <code>gh</code>, <strong>merged means
merged into local git</strong> and nothing leaves your machine. Where Rudder can open
pull requests, <code>m</code> on a reviewed row pushes a fresh branch and opens a
<strong>draft</strong> PR instead of merging locally. The first time in a given repo it
asks first, naming the remote and the branch; after you accept, it stops asking for that
repo. It never pushes your default branch, and there is only ever one route to main for
a given row: a PR or a local merge, never both.</p>

<h2 id="teammates">Do my teammates need jj?</h2>
<p>No. What they see is ordinary git commits on ordinary branches. jj is a local tool
for how <em>you</em> work; nothing about it reaches the remote.</p>

<h2 id="cost">What does it cost to run?</h2>
<p>Rudder itself is free and open source. You pay for whatever the agent CLIs cost
you, exactly as you would running them by hand. <code>/usage</code> shows tokens and
cost for the session.</p>
<p>The dashboard is a native Rust TUI: about 27 MB resident, and asleep until an agent
writes.</p>

<h2 id="crash">What happens if it crashes?</h2>
<p>Agent sessions belong to the backend CLIs, not to Rudder, so they survive. Rudder
records each row's session id as soon as it exists and derives state on restart from
jj, the filesystem and the running processes rather than from anything it remembered.
Reopen it and use <code>/resume</code> for anything that lost its pane.</p>
`,
  },
};

function navHtml(current) {
  return NAV.map((section) => {
    const items = section.items
      .map(([slug, label]) => {
        const href = slug ? `/docs/${slug}` : "/docs";
        const currentAttr = slug === current ? ' aria-current="page"' : "";
        return `          <li><a href="${href}"${currentAttr}>${label}</a></li>`;
      })
      .join("\n");
    return `        <h2>${section.title}</h2>\n        <ul>\n${items}\n        </ul>`;
  }).join("\n");
}

function tocHtml(toc) {
  if (!toc?.length) return "";
  const items = toc
    .map(([id, label]) => `            <li><a href="#${id}">${label}</a></li>`)
    .join("\n");
  return `      <aside class="docs-toc">
        <h2>on this page</h2>
        <ul>
${items}
        </ul>
      </aside>`;
}

function pagerHtml(slug) {
  const index = ORDER.indexOf(slug);
  const link = (target, dir, label) => {
    const href = target ? `/docs/${target}` : "/docs";
    return `        <a class="${dir}" href="${href}"><span class="field-label">${label}</span>${PAGES[target].title}</a>`;
  };
  const parts = [];
  if (index > 0) parts.push(link(ORDER[index - 1], "prev", "previous"));
  if (index < ORDER.length - 1) parts.push(link(ORDER[index + 1], "next", "next"));
  if (!parts.length) return "";
  return `      <nav class="docs-pager" aria-label="Pagination">\n${parts.join("\n")}\n      </nav>`;
}

function render(slug, page) {
  const canonical = slug ? `/docs/${slug}` : "/docs";
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>${page.title} · Rudder docs</title>
    <meta name="description" content="${page.lede.replace(/"/g, "&quot;")}" />
    <link rel="canonical" href="https://rudder.viraat.dev${canonical}" />
    <link rel="icon" href="/favicon.svg" type="image/svg+xml" />
    <meta name="theme-color" content="#ffffff" />
    <link rel="preconnect" href="https://fonts.googleapis.com" />
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin />
    <link
      href="https://fonts.googleapis.com/css2?family=Archivo:wght@400;500;600&family=JetBrains+Mono:wght@400;500&display=swap"
      rel="stylesheet"
    />
    <link rel="stylesheet" href="/docs.css" />
  </head>
  <body>
    <a class="skip-link" href="#main">Skip to content</a>
    <div class="page">
      <header class="site-head">
        <a class="brand" href="/">
          <img src="/favicon.svg" alt="" />
          <span>Rudder</span>
        </a>
        <nav class="site-nav" aria-label="Primary">
          <a href="/docs" aria-current="page">Docs</a>
          <a href="https://github.com/viraatdas/rudder">GitHub</a>
          <a href="https://www.npmjs.com/package/@viraatdas/rudder">npm</a>
          <a href="/login">Sign in</a>
        </nav>
      </header>
    </div>

    <div class="docs">
      <nav class="docs-nav" aria-label="Documentation">
${navHtml(slug)}
      </nav>

      <main class="docs-main" id="main">
        <h1>${page.title}</h1>
        <p class="docs-lede">${page.lede}</p>
${page.body.trim()}
${pagerHtml(slug)}
        <div class="docs-foot">
          <a href="/">Rudder home</a>
          <a href="https://github.com/viraatdas/rudder">GitHub</a>
          <a href="https://www.npmjs.com/package/@viraatdas/rudder">npm</a>
          <a href="https://github.com/viraatdas/rudder/issues">Report an issue</a>
        </div>
      </main>

${tocHtml(page.toc)}
    </div>
  </body>
</html>
`;
}

await mkdir(outDir, { recursive: true });
for (const [slug, page] of Object.entries(PAGES)) {
  const file = slug ? path.join(outDir, `${slug}.html`) : path.join(outDir, "index.html");
  await writeFile(file, render(slug, page), "utf8");
  console.log(`wrote ${path.relative(here, file)}`);
}

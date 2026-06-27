const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

/* ----- scroll reveals: IntersectionObserver + class-driven final state -----
   The .in class persists, so content can never get stuck mid-animation. */
if (!reduceMotion && "IntersectionObserver" in window) {
  document.documentElement.classList.add("motion-ready");

  const revealIO = new IntersectionObserver((entries) => {
    entries.forEach((entry) => {
      if (!entry.isIntersecting) return;
      const el = entry.target;
      if (el.hasAttribute("data-reveal-group")) {
        Array.from(el.children).forEach((child, i) => {
          child.style.transitionDelay = i * 70 + "ms";
        });
      }
      el.classList.add("in");
      revealIO.unobserve(el);
    });
  }, { rootMargin: "0px 0px -10% 0px" });

  document
    .querySelectorAll("[data-reveal], [data-reveal-group]")
    .forEach((el) => revealIO.observe(el));

  // hero headline underline draw
  const steer = document.querySelector("h1 .steer");
  if (steer) setTimeout(() => steer.classList.add("drawn"), 600);
}

/* ----- live DAG choreography ----- */
(function dag() {
  const svg = document.querySelector("[data-dag]");
  if (!svg) return;

  const screen = svg.closest(".screen");
  const STATE = { planned: "planned", running: "running", review: "review", merged: "merged" };
  const node = (id) => svg.querySelector(`.node[data-node="${id}"]`);
  const edge = (key) => svg.querySelector(`.edge[data-edge="${key}"]`);

  function setStatus(id, status) {
    const n = node(id);
    if (!n) return;
    const prev = n.getAttribute("data-status");
    n.setAttribute("data-status", status);
    const label = n.querySelector(".node-state");
    if (label) label.textContent = status;
    if (!reduceMotion && status === STATE.merged && prev !== STATE.merged) ring(n);
  }

  function ring(n) {
    const ns = "http://www.w3.org/2000/svg";
    const c = document.createElementNS(ns, "circle");
    c.setAttribute("class", "merge-ring");
    c.setAttribute("cx", "70");
    c.setAttribute("cy", "30");
    c.setAttribute("r", "30");
    n.appendChild(c);
    c.animate(
      [
        { opacity: 0.9, transform: "scale(0.6)" },
        { opacity: 0, transform: "scale(1.7)" },
      ],
      { duration: 600, easing: "cubic-bezier(0.16,1,0.3,1)" }
    );
    setTimeout(() => c.remove(), 700);
  }

  function reveal(id) {
    node(id)?.classList.add("shown");
  }

  function drawEdge(key) {
    const e = edge(key);
    if (!e) return;
    e.style.strokeDashoffset = "0";
    setTimeout(() => {
      e.style.strokeDasharray = "";
      e.style.strokeDashoffset = "";
      e.style.transition = "";
    }, 760);
  }

  function flow(key, on) {
    edge(key)?.classList.toggle("flow", on);
    pulse(key, on);
  }

  function pulse(key, on) {
    const existing = svg.querySelector(`.pulse[data-pulse="${key}"]`);
    if (!on) {
      existing?.remove();
      return;
    }
    if (existing || reduceMotion) return;
    const e = edge(key);
    const pathData = e?.getAttribute("d");
    if (!pathData) return;
    const ns = "http://www.w3.org/2000/svg";
    const dot = document.createElementNS(ns, "circle");
    dot.setAttribute("class", "pulse");
    dot.setAttribute("data-pulse", key);
    dot.setAttribute("r", "4");
    const motion = document.createElementNS(ns, "animateMotion");
    motion.setAttribute("dur", "1.4s");
    motion.setAttribute("repeatCount", "indefinite");
    motion.setAttribute("path", pathData);
    dot.appendChild(motion);
    svg.querySelector(".dag-edges")?.appendChild(dot);
  }

  // reduced motion / no orchestration: keep the authored steady state.
  if (reduceMotion) return;

  // prep: hide nodes (via .choreo) and "lift" the edges so they can draw in.
  const nodes = ["01", "02", "03", "04", "05"];
  const edges = ["01-02", "01-03", "02-03", "02-04", "03-04", "04-05"];
  svg.classList.add("choreo");
  edges.forEach((key) => {
    const e = edge(key);
    if (!e) return;
    const L = e.getTotalLength();
    e.style.strokeDasharray = String(L);
    e.style.strokeDashoffset = String(L);
    e.style.transition = "stroke-dashoffset 0.7s cubic-bezier(0.22,1,0.36,1)";
  });
  nodes.forEach((id) => setStatus(id, STATE.planned));

  // build the graph, then settle into a believable in-flight state:
  // 01 merged, 02 in review, 03 running, 04 + 05 still planned.
  const T = [
    [120, () => { reveal("01"); setStatus("01", STATE.running); }],
    [620, () => { drawEdge("01-02"); drawEdge("01-03"); drawEdge("02-03"); }],
    [1000, () => { reveal("02"); reveal("03"); }],
    [1320, () => { drawEdge("02-04"); drawEdge("03-04"); }],
    [1560, () => { reveal("04"); }],
    [1800, () => { drawEdge("04-05"); reveal("05"); }],
    [2400, () => { setStatus("01", STATE.merged); }],
    [2800, () => { setStatus("02", STATE.running); setStatus("03", STATE.running); flow("01-02", true); flow("01-03", true); }],
    [4600, () => { setStatus("02", STATE.review); flow("01-02", false); }],
  ];

  let started = false;
  const run = () => {
    if (started) return;
    started = true;
    screen?.classList.add("scanning");
    setTimeout(() => screen?.classList.remove("scanning"), 1500);
    T.forEach(([t, fn]) => setTimeout(fn, t));
  };

  // start when the screen scrolls into view (it's near the top, so this fires fast)
  if ("IntersectionObserver" in window) {
    const io = new IntersectionObserver((entries) => {
      entries.forEach((en) => {
        if (en.isIntersecting) { run(); io.disconnect(); }
      });
    }, { threshold: 0.35 });
    io.observe(svg);
  } else {
    run();
  }
})();

/* ----- task-pane typewriter ----- */
(function typer() {
  const target = document.querySelector("[data-type-text]");
  if (!target || reduceMotion) return;
  const phrases = [
    "build a spotify top-tracks dashboard",
    "answer the planner's questions first",
    "edit node 04, then approve the plan",
    "/model codex gpt-5.5",
  ];
  let pi = 0, ci = 0, deleting = false;
  const tick = () => {
    const p = phrases[pi];
    target.textContent = p.slice(0, ci);
    if (!deleting && ci < p.length) { ci++; return void setTimeout(tick, 46); }
    if (!deleting) { deleting = true; return void setTimeout(tick, 1100); }
    if (ci > 0) { ci--; return void setTimeout(tick, 24); }
    deleting = false; pi = (pi + 1) % phrases.length; setTimeout(tick, 260);
  };
  tick();
})();

/* ----- copy buttons ----- */
document.querySelectorAll("[data-copy]").forEach((button) => {
  button.addEventListener("click", async () => {
    const value = button.dataset.copyValue
      || document.getElementById(button.dataset.copy)?.textContent.replace(/^\$\s*/, "").trim()
      || "";
    const original = button.textContent;
    let ok = true;
    try { await navigator.clipboard.writeText(value); } catch { ok = false; }
    button.textContent = ok ? "Copied" : "Copy failed";
    if (!reduceMotion)
      button.animate(
        [{ transform: "scale(1)" }, { transform: "scale(0.94)" }, { transform: "scale(1)" }],
        { duration: 300, easing: "ease-out" }
      );
    setTimeout(() => { button.textContent = original; }, 1200);
  });
});

const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

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

const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

/* Copy the install command. The button reports what actually happened: a failed
 * clipboard write says so rather than claiming success, because the user's next
 * move is a paste that would otherwise silently produce nothing. */
document.querySelectorAll("[data-copy]").forEach((button) => {
  const original = button.textContent.trim();
  button.addEventListener("click", async () => {
    const value =
      button.dataset.copyValue ||
      document
        .getElementById(button.dataset.copy)
        ?.textContent.replace(/^\$\s*/, "")
        .trim() ||
      "";

    let ok = true;
    try {
      await navigator.clipboard.writeText(value);
    } catch {
      ok = false;
    }

    button.textContent = ok ? "Copied" : "Press ⌘C";
    button.dataset.copied = String(ok);

    if (ok && !reduceMotion) {
      button.animate(
        [{ opacity: 0.55 }, { opacity: 1 }],
        { duration: 260, easing: "cubic-bezier(0.16, 1, 0.3, 1)" },
      );
    }

    setTimeout(() => {
      button.textContent = original;
      delete button.dataset.copied;
    }, 1400);
  });
});

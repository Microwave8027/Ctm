// Keep the run log pinned to the bottom while new lines stream in, unless
// the reader has scrolled up.
(function () {
  let stick = true;
  document.addEventListener("scroll", () => {
    stick = window.innerHeight + window.scrollY >= document.body.scrollHeight - 80;
  }, { passive: true });
  document.body.addEventListener("htmx:sseMessage", (e) => {
    if (e.detail.type === "line" && stick) window.scrollTo(0, document.body.scrollHeight);
  });
})();

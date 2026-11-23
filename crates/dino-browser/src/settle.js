// Resolves once the document has loaded and the DOM has been still for 500 ms,
// or after 15 s whatever it is doing. Network idle alone misreads pages that poll.
new Promise((resolve) => {
  const started = Date.now();
  let last = Date.now();
  const observer = new MutationObserver(() => { last = Date.now(); });
  const watch = () => observer.observe(document.documentElement || document, { childList: true, subtree: true, attributes: true, characterData: true });
  watch();
  const tick = () => {
    const loaded = document.readyState === "complete";
    if ((loaded && Date.now() - last >= 500) || Date.now() - started >= 15000) {
      observer.disconnect();
      resolve(true);
    } else {
      setTimeout(tick, 100);
    }
  };
  tick();
})

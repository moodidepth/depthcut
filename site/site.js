// Point the download button at the newest release's installer. The
// /releases/latest API skips pre-releases, so list releases instead. Without
// JS or on any error the button keeps linking to the releases page.
(async () => {
  const btn = document.getElementById("dl-btn");
  const meta = document.getElementById("dl-meta");
  try {
    const r = await fetch("https://api.github.com/repos/moodidepth/depthcut/releases?per_page=10", {
      headers: { Accept: "application/vnd.github+json" },
    });
    if (!r.ok) return;
    const rel = (await r.json()).find((x) => !x.draft);
    if (!rel) return;
    const setup = rel.assets.find((a) => /-win64-setup\.exe$/i.test(a.name));
    const zip = rel.assets.find((a) => /-win64-portable\.zip$/i.test(a.name));
    if (!setup) return;
    btn.href = setup.browser_download_url;
    const mb = (n) => `${Math.round(n / 1048576)} MB`;
    meta.textContent = "";
    meta.append(`${rel.tag_name} · installer, ${mb(setup.size)}`);
    if (zip) {
      const a = document.createElement("a");
      a.href = zip.browser_download_url;
      a.textContent = `portable zip (${mb(zip.size)})`;
      meta.append(" · or ", a);
    }
    const notes = document.createElement("a");
    notes.href = rel.html_url;
    notes.textContent = "release notes";
    meta.append(" · ", notes);
  } catch {
    /* keep the static fallback */
  }
})();

// Screenshot lightbox.
const viewer = document.getElementById("viewer");
document.querySelectorAll(".zoom").forEach((b) =>
  b.addEventListener("click", () => {
    const img = viewer.querySelector("img");
    img.src = b.dataset.full;
    img.alt = b.querySelector("img").alt;
    viewer.showModal();
  }),
);
viewer.addEventListener("click", (e) => {
  if (e.target === viewer) viewer.close();
});

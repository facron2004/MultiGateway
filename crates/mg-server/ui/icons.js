// Line icons adapted from Lucide 0.468.0 (ISC), with custom gear, key and
// media glyphs. See licenses/LUCIDE-ICONS-LICENSE.
const paths = {
  settings: '<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.38a2 2 0 0 0-.73-2.73l-.15-.09a2 2 0 0 1-1-1.74v-.51a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2Z"/><circle cx="12" cy="12" r="3"/>',
  sliders: '<path d="M4 7h5M15 7h5M4 12h10M18 12h2M4 17h2M10 17h10"/><circle cx="12" cy="7" r="2.25"/><circle cx="16" cy="12" r="2.25"/><circle cx="8" cy="17" r="2.25"/>',
  database: '<ellipse cx="12" cy="5.75" rx="7.75" ry="2.75"/><path d="M4.25 5.75v12.5c0 1.52 3.47 2.75 7.75 2.75s7.75-1.23 7.75-2.75V5.75M4.25 12c0 1.52 3.47 2.75 7.75 2.75s7.75-1.23 7.75-2.75"/>',
  network: '<circle cx="12" cy="12" r="8.5"/><path d="M3.5 12h17M12 3.5c2.2 2.3 3.3 5.13 3.3 8.5s-1.1 6.2-3.3 8.5c-2.2-2.3-3.3-5.13-3.3-8.5S9.8 5.8 12 3.5Z"/>',
  back: '<path d="M19 12H5M11 6l-6 6 6 6"/>',
  stats: '<path d="M4 20h16M7 16v4M12 11v9M17 5v15"/>',
  accounts: '<circle cx="12" cy="8" r="3.5"/><path d="M5 20v-1a7 7 0 0 1 14 0v1"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  chevron: '<path d="m7 9.5 5 5 5-5"/>',
  "chevron-left": '<path d="m14.5 6.5-5.5 5.5 5.5 5.5"/>',
  "chevron-right": '<path d="m9.5 6.5 5.5 5.5-5.5 5.5"/>',
  "chevrons-left": '<path d="m11 7-5 5 5 5M17 7l-5 5 5 5"/>',
  "chevrons-right": '<path d="m7 7 5 5-5 5M13 7l5 5-5 5"/>',
  info: '<circle cx="12" cy="12" r="8.5"/><path d="M12 11v5"/><circle cx="12" cy="7.75" r=".9" fill="currentColor" stroke="none"/>',
  refresh: '<path d="M20 7.5A8.5 8.5 0 0 0 5.2 6.1L3.5 8M3.5 3.75V8H7.75M4 16.5a8.5 8.5 0 0 0 14.8 1.4l1.7-1.9M20.5 20.25V16H16.25"/>',
  checkin: '<rect x="3.75" y="5" width="16.5" height="15.25" rx="2.25"/><path d="M8 3v4M16 3v4M3.75 9.5h16.5m-11.6 5.15 2.15 2.15 4.45-4.5"/>',
  check: '<path d="m5 12.5 4.25 4.25L19 7"/>',
  close: '<path d="m6.5 6.5 11 11m0-11-11 11"/>',
  copy: '<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/>',
  folder: '<path d="M3.75 7.75A1.75 1.75 0 0 1 5.5 6h4l1.75 2h7.25a1.75 1.75 0 0 1 1.75 1.75v7.5A1.75 1.75 0 0 1 18.5 19h-13a1.75 1.75 0 0 1-1.75-1.75Z"/>',
  import: '<path d="M12 3.5v10m-4-4 4 4 4-4M4 15.5V18a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2.5"/>',
  search: '<circle cx="10.75" cy="10.75" r="6.75"/><path d="m15.75 15.75 4.25 4.25"/>',
  delete: '<path d="M4 6.5h16M9 6.5V4.75A1.25 1.25 0 0 1 10.25 3.5h3.5A1.25 1.25 0 0 1 15 4.75V6.5M6.5 6.5l.75 12A1.75 1.75 0 0 0 9 20.25h6a1.75 1.75 0 0 0 1.75-1.75l.75-12M10 10.5v6M14 10.5v6"/>',
  key: '<circle cx="7.75" cy="12" r="3.75"/><path d="M11.5 12h9M16.5 12v3M19.5 12v2"/>',
  play: '<path d="M8 5.75v12.5c0 .8.88 1.28 1.55.85l9.1-6.25a1 1 0 0 0 0-1.7l-9.1-6.25A1.01 1.01 0 0 0 8 5.75Z" fill="currentColor" stroke="none"/>',
  pause: '<rect x="6.5" y="5" width="3.5" height="14" rx="1.25" fill="currentColor" stroke="none"/><rect x="14" y="5" width="3.5" height="14" rx="1.25" fill="currentColor" stroke="none"/>',
  more: '<circle cx="5.5" cy="12" r="1.5" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="1.5" fill="currentColor" stroke="none"/><circle cx="18.5" cy="12" r="1.5" fill="currentColor" stroke="none"/>',
};

export function icon(name) {
  const body = paths[name] || paths.accounts;
  return `<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
}
const DARK_MARKS = new Set(["qoder"]);

function isDark() {
  return document.documentElement.dataset.theme === "dark";
}

export const logo = (name) => `./icons/${name}${isDark() && DARK_MARKS.has(name) ? "-dark" : ""}.svg`;

export function refreshLogos() {
  document.querySelectorAll("img").forEach((img) => {
    const src = img.getAttribute("src") || "";
    const match = src.match(/\/icons\/([a-z0-9-]+?)(?:-dark)?\.svg$/);
    if (!match || !DARK_MARKS.has(match[1])) return;
    const next = logo(match[1]);
    if (!src.endsWith(next.slice(1))) img.src = next;
  });
}

export function mountIcons() {
  document.querySelectorAll("[data-icon]").forEach((node) => {
    node.innerHTML = icon(node.dataset.icon);
  });
}

// Provider marks are plain <img> files, so a theme switch has to swap the
// source instead of relying on currentColor. Observe the root attribute that
// appearance.js writes and keep every logo in sync.
new MutationObserver(refreshLogos).observe(document.documentElement, {
  attributes: true,
  attributeFilter: ["data-theme"],
});
queueMicrotask(refreshLogos);

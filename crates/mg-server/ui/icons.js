const paths = {
  sliders: '<path d="M4 6h4m4 0h8M4 12h10m4 0h2M4 18h3m4 0h9M8 3v6m6 0v6M7 15v6"/>',
  database: '<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14c0 1.7 4 3 9 3s9-1.3 9-3V5M3 12c0 1.7 4 3 9 3s9-1.3 9-3"/>',
  network: '<circle cx="12" cy="12" r="9"/><ellipse cx="12" cy="12" rx="4" ry="9"/><path d="M3 12h18"/>',
  back: '<path d="M19 12H5M12 19l-7-7 7-7"/>',
  stats: '<path d="M4 3v18h17M8 17v-6M13 17V7M18 17v-9"/>',
  accounts:
    '<circle cx="9" cy="8" r="3"/><path d="M3 20v-2a6 6 0 0 1 12 0v2M16 4a3 3 0 0 1 0 6M17 14a5 5 0 0 1 4 5"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  chevron: '<path d="m6 9 6 6 6-6"/>',
  "chevron-left": '<path d="m15 18-6-6 6-6"/>',
  "chevron-right": '<path d="m9 18 6-6-6-6"/>',
  "chevrons-left": '<path d="m11 17-5-5 5-5M18 17l-5-5 5-5"/>',
  "chevrons-right": '<path d="m6 17 5-5-5-5M13 17l5-5-5-5"/>',
  info: '<circle cx="12" cy="12" r="10"/><path d="M12 16v-4M12 8h.01"/>',
  more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  refresh:
    '<path d="M20 7v5h-5M4 17v-5h5M19 11a7 7 0 0 0-12-5L4 9m1 4a7 7 0 0 0 12 5l3-3"/>',
  play: '<path d="m7 4 14 8-14 8V4Z"/>',
  pause: '<path d="M7 5v14M17 5v14"/>',
  checkin:
    '<rect x="3" y="5" width="18" height="16" rx="2"/><path d="M7 3v4M17 3v4M3 10h18m-13 6 3 3 5-5"/>',
  check: '<path d="m5 12 4 4L19 6"/>',
  close: '<path d="m6 6 12 12M6 18 18 6"/>',
  copy: '<rect x="8" y="8" width="12" height="13" rx="2"/><path d="M15 8V3H3v13h5"/>',
  settings:
    '<path d="M12.22 2h-.44a2 2 0 0 0-2 1.72l-.17 1.3a2 2 0 0 1-1 1.47l-.54.31a2 2 0 0 1-1.77.1l-1.24-.5a2 2 0 0 0-2.58.91l-.22.38a2 2 0 0 0 .51 2.69l1.04.8a2 2 0 0 1 .75 1.69v.62a2 2 0 0 1-.75 1.69l-1.04.8a2 2 0 0 0-.51 2.69l.22.38a2 2 0 0 0 2.58.91l1.24-.5a2 2 0 0 1 1.77.1l.54.31a2 2 0 0 1 1 1.47l.17 1.3a2 2 0 0 0 2 1.72h.44a2 2 0 0 0 2-1.72l.17-1.3a2 2 0 0 1 1-1.47l.54-.31a2 2 0 0 1 1.77-.1l1.24.5a2 2 0 0 0 2.58-.91l.22-.38a2 2 0 0 0-.51-2.69l-1.04-.8a2 2 0 0 1-.75-1.69v-.62a2 2 0 0 1 .75-1.69l1.04-.8a2 2 0 0 0 .51-2.69l-.22-.38a2 2 0 0 0-2.58-.91l-1.24.5a2 2 0 0 1-1.77-.1l-.54-.31a2 2 0 0 1-1-1.47l-.17-1.3A2 2 0 0 0 12.22 2z"/><circle cx="12" cy="12" r="3"/>',
  folder: '<path d="M3 6h6l2 2h10v12H3V6Z"/>',
  import: '<path d="M12 3v12m-5-5 5 5 5-5M4 15v6h16v-6"/>',
  search: '<circle cx="10.5" cy="10.5" r="7"/><path d="m16 16 5 5"/>',
  delete: '<path d="M4 6h16M9 6V3h6v3M6 6l1 15h10l1-15M10 10v7M14 10v7"/>',
  key: '<circle cx="7.5" cy="15.5" r="4.5"/><path d="m10.7 12.3 8.3-8.3M15 7l2 2m2-4 2 2"/>',
};
export function icon(name) {
  return `<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${paths[name] || paths.accounts}</svg>`;
}
export const logo = (name) => `/gateway/ui/icons/${name}.svg`;
export function mountIcons() {
  document.querySelectorAll("[data-icon]").forEach((node) => {
    node.innerHTML = icon(node.dataset.icon);
  });
}

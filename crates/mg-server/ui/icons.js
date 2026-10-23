const paths = {
  // Six broad teeth. A fine cog turns into a gray blob at 20px.
  settings:
    '<path fill="currentColor" stroke="none" fill-rule="evenodd" d="M9.03 6.85 9.74 3.55h4.52l.72 3.3 3.21-1.04 2.26 3.93L17.95 12l2.5 2.26-2.26 3.93-3.21-1.04-.72 3.3H9.74l-.71-3.3-3.22 1.04L3.55 14.26 6.05 12 3.55 9.74l2.26-3.93 3.22 1.04ZM12 9.15a2.85 2.85 0 1 0 0 5.7 2.85 2.85 0 0 0 0-5.7Z"/>',
  sliders:
    '<path d="M4 7h16M4 12h16M4 17h16"/><circle cx="9" cy="7" r="2.2" fill="currentColor" stroke="none"/><circle cx="15" cy="12" r="2.2" fill="currentColor" stroke="none"/><circle cx="7" cy="17" r="2.2" fill="currentColor" stroke="none"/>',
  database:
    '<ellipse cx="12" cy="6.5" rx="7.5" ry="2.7"/><path d="M4.5 6.5v11c0 1.5 3.4 2.7 7.5 2.7s7.5-1.2 7.5-2.7v-11"/><path d="M4.5 12c0 1.5 3.4 2.7 7.5 2.7s7.5-1.2 7.5-2.7"/>',
  network:
    '<circle cx="12" cy="12" r="8"/><path d="M4 12h16"/><path d="M12 4c2.15 2.15 3.25 4.9 3.25 8S14.15 17.85 12 20c-2.15-2.15-3.25-4.9-3.25-8S9.85 6.15 12 4Z"/>',
  back: '<path d="M19 12H6.25"/><path d="m11.5 6.75-5.5 5.25 5.5 5.25"/>',
  stats: '<path d="M5 19.5h14M7.5 19.5v-6M12 19.5V6.5M16.5 19.5v-9"/>',
  accounts:
    '<circle cx="12" cy="8" r="3.15"/><path d="M5.25 19.25v-.7c0-2.85 2.7-4.8 6.75-4.8s6.75 1.95 6.75 4.8v.7"/>',
  plus: '<path d="M12 6v12M6 12h12"/>',
  chevron: '<path d="m7 10 5 5 5-5"/>',
  "chevron-left": '<path d="m14 7-5 5 5 5"/>',
  "chevron-right": '<path d="m10 7 5 5-5 5"/>',
  "chevrons-left": '<path d="m13.5 7-5 5 5 5M18.5 7l-5 5 5 5"/>',
  "chevrons-right": '<path d="m10.5 7 5 5-5 5M5.5 7l5 5-5 5"/>',
  info:
    '<circle cx="12" cy="12" r="8"/><path d="M12 11.25V16.25"/><circle cx="12" cy="8.15" r=".95" fill="currentColor" stroke="none"/>',
  more:
    '<circle cx="6" cy="12" r="1.45" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="1.45" fill="currentColor" stroke="none"/><circle cx="18" cy="12" r="1.45" fill="currentColor" stroke="none"/>',
  refresh:
    '<path d="M19.5 12a7.5 7.5 0 0 0-12.8-5.3L4.5 9"/><path d="M4.5 4.75V9H8.75"/><path d="M4.5 12a7.5 7.5 0 0 0 12.8 5.3l2.2-2.3"/><path d="M19.5 19.25V15h-4.25"/>',
  play:
    '<path d="M8.15 6.05v11.9c0 .62.68 1 1.2.66l8.7-5.95c.45-.3.45-1 0-1.32l-8.7-5.95c-.52-.34-1.2.04-1.2.66Z" fill="currentColor" stroke="none"/>',
  pause:
    '<rect x="6.6" y="5.6" width="3.3" height="12.8" rx="1.15" fill="currentColor" stroke="none"/><rect x="14.1" y="5.6" width="3.3" height="12.8" rx="1.15" fill="currentColor" stroke="none"/>',
  checkin:
    '<rect x="4" y="5.5" width="16" height="14.5" rx="2.25"/><path d="M8 3.75v3.1M16 3.75v3.1M4 10.25h16"/><path d="m8.7 15.15 2.15 2.15 4.45-4.5"/>',
  check: '<path d="m5.5 12.25 4.1 4.15L18.75 7.5"/>',
  close: '<path d="m7 7 10 10M17 7 7 17"/>',
  copy:
    '<rect x="8.25" y="8.25" width="11" height="11" rx="2"/><path d="M15.75 8.25V5.75A1.75 1.75 0 0 0 14 4H5.75A1.75 1.75 0 0 0 4 5.75V14A1.75 1.75 0 0 0 5.75 15.75H8.25"/>',
  folder:
    '<path d="M3.75 8.15A1.9 1.9 0 0 1 5.65 6.25h3.7l1.7 1.85h7.3a1.9 1.9 0 0 1 1.9 1.9v7.15a1.9 1.9 0 0 1-1.9 1.9H5.65a1.9 1.9 0 0 1-1.9-1.9V8.15Z"/>',
  import:
    '<path d="M12 4v9"/><path d="m8.25 10.25 3.75 3.75 3.75-3.75"/><path d="M5 15.25V18a1.75 1.75 0 0 0 1.75 1.75h10.5A1.75 1.75 0 0 0 19 18v-2.75"/>',
  search: '<circle cx="11" cy="11" r="6.25"/><path d="m15.75 15.75 4 4"/>',
  delete:
    '<path d="M5 7.25h14"/><path d="M9.5 7.25V5.4A1.15 1.15 0 0 1 10.65 4.25h2.7A1.15 1.15 0 0 1 14.5 5.4v1.85"/><path d="m7.15 7.25.75 11.15A1.6 1.6 0 0 0 9.5 20h5a1.6 1.6 0 0 0 1.6-1.6l.75-11.15"/>',
  key:
    '<circle cx="8" cy="12" r="3.45"/><path d="M11.25 12H20M16.7 12v2.6"/>',
};

export function icon(name) {
  const body = paths[name] || paths.accounts;
  return `<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
}
export const logo = (name) => `./icons/${name}.svg`;
export function mountIcons() {
  document.querySelectorAll("[data-icon]").forEach((node) => {
    node.innerHTML = icon(node.dataset.icon);
  });
}

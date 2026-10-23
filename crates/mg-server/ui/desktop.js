(() => {
  if (window.top !== window) return;
  const invoke = (action, url) =>
    window.__TAURI__.core.invoke("desktop_action", { action, url });
  window.multigateway = { api: {
    window_minimize: () => invoke("minimize"),
    window_maximize: () => invoke("maximize"),
    window_close: () => invoke("close"),
    window_drag: () => invoke("drag"),
    open_folder: () => invoke("open_folder"),
    open_external: (url) => invoke("open_external", url),
  }};
  window.addEventListener("DOMContentLoaded", () => {
    window.dispatchEvent(new Event("multigatewayready"));
  });
})();

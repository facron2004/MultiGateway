const reads = {
  "/health": "gateway_health",
  "/gateway/status": "gateway_status",
  "/gateway/usage/board": "usage_board",
  "/gateway/tray/summary": "tray_summary",
  "/settings": "settings_view",
  "/settings/reveal": "settings_reveal",
  "/gateway/api-keys": "api_key_list",
  "/gateway/api-keys/list": "api_key_list",
};
const writes = {
  "/gateway/configure": "gateway_configure",
  "/settings/desktop": "desktop_settings_save",
  "/settings/appearance": "appearance_settings_save",
  "/gateway/api-keys/generate": "api_key_generate",
  "/gateway/api-keys/delete": "api_key_delete",
};
const providers = {
  snapshot: "provider_snapshot",
  models: "provider_models",
  login: "provider_login",
  poll: "provider_poll",
  enable: "provider_enable_account",
  delete: "provider_delete_account",
  credits: "provider_credits",
  credentials: "provider_credentials_action",
  checkin: "provider_checkin_action",
  batch: "provider_batch",
  region: "provider_region",
  import: "provider_import",
};

async function invoke(command, args, signal) {
  if (signal?.aborted) throw new DOMException("Aborted", "AbortError");
  const pending = window.__TAURI__.core.invoke(command, args);
  if (!signal) return pending;
  let cancel;
  const aborted = new Promise((_, reject) => {
    cancel = () => reject(new DOMException("Aborted", "AbortError"));
    signal.addEventListener("abort", cancel, { once: true });
  });
  try {
    return await Promise.race([pending, aborted]);
  } finally {
    signal.removeEventListener("abort", cancel);
  }
}

export function nativeApi(path, payload, signal) {
  const url = new URL(path, location.href);
  const provider = url.pathname.match(/^\/gateway\/providers\/([^/]+)\/([^/]+)$/);
  let command;
  const args = payload === undefined ? {} : { payload };
  if (provider) {
    command = providers[provider[2]];
    args.providerName = decodeURIComponent(provider[1]);
    if (provider[2] === "models") args.region = url.searchParams.get("region");
  } else {
    command = (payload === undefined ? reads : writes)[url.pathname];
    if (command === "settings_reveal") args.id = url.searchParams.get("id");
  }
  if (!command) throw new Error("不支持的桌面操作");
  return invoke(command, args, signal);
}

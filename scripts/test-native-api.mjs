import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

globalThis.location = { href: "http://tauri.localhost/index.html" };
const calls = [];
globalThis.window = { __TAURI__: { core: { invoke: async (command, args) => {
  calls.push({ command, args });
  return { ok: true };
} } } };
globalThis.fetch = () => { throw new Error("Native UI attempted HTTP"); };
const { api, providerApi } = await import("../crates/mg-server/ui/api.js");
const cases = [
  ["/health", undefined, "gateway_health"],
  ["/gateway/status", undefined, "gateway_status"],
  ["/gateway/usage/board", undefined, "usage_board"],
  ["/settings", undefined, "settings_view"],
  ["/settings/reveal?id=key%20one", undefined, "settings_reveal", { id: "key one" }],
  ["/gateway/api-keys/list", undefined, "api_key_list"],
  ["/gateway/api-keys", undefined, "api_key_list"],
  ["/gateway/configure", { enabled: true }, "gateway_configure"],
  ["/settings/desktop", { auto_start: false }, "desktop_settings_save"],
  ["/settings/appearance", { theme: "dark" }, "appearance_settings_save"],
  ["/gateway/api-keys/generate", { name: "test" }, "api_key_generate"],
  ["/gateway/api-keys/delete", { id: "example" }, "api_key_delete"],
];
for (const [path, payload, command, expected] of cases) {
  await api(path, payload);
  assert.deepEqual(calls.at(-1), { command, args: expected || (payload === undefined ? {} : { payload }) });
}
const actions = {
  snapshot: "provider_snapshot", models: "provider_models", login: "provider_login",
  poll: "provider_poll", enable: "provider_enable_account", delete: "provider_delete_account",
  credits: "provider_credits", credentials: "provider_credentials_action", checkin: "provider_checkin_action",
  batch: "provider_batch", region: "provider_region", import: "provider_import",
};
for (const [action, command] of Object.entries(actions)) {
  const payload = ["snapshot", "models"].includes(action) ? undefined : { id: "example" };
  await providerApi("minimax", action === "models" ? "models?region=global" : action, payload);
  assert.deepEqual(calls.at(-1), { command, args: { providerName: "minimax",
    ...(payload === undefined ? {} : { payload }), ...(action === "models" ? { region: "global" } : {}) } });
}
const mappedCommands = new Set(calls.map(({ command }) => command));
const permission = await readFile(new URL("../crates/mg-server/permissions/desktop.toml", import.meta.url), "utf8");
const allowedCommands = new Set([...permission.matchAll(/^\s+"(\w+)",/gm)].map((match) => match[1]));
allowedCommands.delete("desktop_action");
assert.deepEqual(mappedCommands, allowedCommands);

const aborted = new AbortController();
aborted.abort();
const beforeAbort = calls.length;
await assert.rejects(api("/settings", undefined, aborted.signal), { name: "AbortError" });
assert.equal(calls.length, beforeAbort);
let complete;
window.__TAURI__.core.invoke = () => new Promise((resolve) => { complete = resolve; });
const controller = new AbortController();
const pending = api("/settings", undefined, controller.signal);
controller.abort();
await assert.rejects(pending, { name: "AbortError" });
complete({ ok: true });
await Promise.resolve();

window.__TAURI__.core.invoke = async () => { throw { status: 404, message: "Account not found" }; };
await assert.rejects(api("/settings"), { message: "Account not found" });
window.__TAURI__.core.invoke = async () => ({ success: false, error: "Import rejected" });
await assert.rejects(api("/settings"), { message: "Import rejected" });

delete window.__TAURI__;
let request;
globalThis.fetch = async (path, options) => {
  request = { path, options };
  return new Response(JSON.stringify({ ok: true }), { status: 200 });
};
await api("/settings/desktop", { close_to_tray: true });
assert.equal(request.path, "/settings/desktop");
assert.equal(request.options.method, "POST");
assert.equal(request.options.credentials, "same-origin");
assert.deepEqual(JSON.parse(request.options.body), { close_to_tray: true });
globalThis.fetch = async () => new Response(JSON.stringify({ error: { message: "Forbidden" } }), { status: 403 });
await assert.rejects(api("/settings"), { message: "Forbidden" });
console.log(JSON.stringify({ mappedNativeCommands: mappedCommands.size, localHttpCallsInNativeMode: 0,
  abortAndBusinessErrorsPreserved: true, browserTransportPreserved: true }));

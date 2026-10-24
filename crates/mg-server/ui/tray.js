import { api } from "./api.js";
import { applyI18n, setLocale, t } from "./i18n.js";
import { normalizeAppearance, resolveLanguage, resolveTheme } from "./appearance-model.js";

const $ = (id) => document.getElementById(id);
const inTauri = Boolean(window.__TAURI__?.core);
const DARK_MARKS = new Set(["qoder"]);
const media = window.matchMedia("(prefers-color-scheme: dark)");
const providerNames = new Map();

let appearance = readStoredAppearance();
let current = null;
let loading = false;
let toastTimer = null;
let replayFrame = 0;

function readStoredAppearance() {
  try {
    return normalizeAppearance(
      JSON.parse(localStorage.getItem("mg-appearance") || "null") || {},
    );
  } catch {
    return normalizeAppearance();
  }
}

function langCode() {
  return document.documentElement.lang === "en" ? "en-US" : "zh-CN";
}

function isDark() {
  return document.documentElement.dataset.theme === "dark";
}

function logo(name) {
  return `./icons/${name}${isDark() && DARK_MARKS.has(name) ? "-dark" : ""}.svg`;
}

function refreshLogos() {
  document.querySelectorAll("img[data-logo]").forEach((img) => {
    const next = logo(img.dataset.logo);
    if (!img.src.endsWith(next.slice(1))) img.src = next;
  });
}

function syncAppearance(next) {
  appearance = normalizeAppearance(next);
  const dark = resolveTheme(appearance.theme, media.matches) === "dark";
  document.documentElement.dataset.theme = dark ? "dark" : "light";
  document.documentElement.style.setProperty("--orange", appearance.accent);
  setLocale(resolveLanguage(appearance.language, navigator.language));
  applyI18n();
  refreshLogos();
  if (current) render(current);
}

const escapeHtml = (value) =>
  String(value ?? "").replace(
    /[&<>"']/g,
    (ch) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[ch],
  );

function formatTokens(value) {
  const num = Number(value);
  if (!Number.isFinite(num) || num <= 0) return "0";
  if (num >= 1e9) {
    const scaled = num / 1e9;
    return `${(scaled >= 10 ? scaled.toFixed(1) : scaled.toFixed(2)).replace(/\.?0+$/, "")}B`;
  }
  if (num >= 1e6) {
    const scaled = num / 1e6;
    if (scaled >= 100) return `${Math.round(scaled)}M`;
    if (scaled >= 10) return `${scaled.toFixed(1).replace(/\.0$/, "")}M`;
    return `${scaled.toFixed(2).replace(/\.?0+$/, "")}M`;
  }
  if (num >= 1e3) {
    const scaled = num / 1e3;
    if (scaled >= 100) return `${Math.round(scaled)}K`;
    return `${scaled.toFixed(1).replace(/\.0$/, "")}K`;
  }
  return String(Math.round(num));
}

function relativeTime(seconds) {
  const value = Number(seconds) || 0;
  const diff = Math.max(0, Date.now() / 1000 - value);
  if (diff < 60) return t("trayJustNow");
  const formatter = new Intl.RelativeTimeFormat(langCode(), { numeric: "auto" });
  if (diff < 3600) return formatter.format(-Math.max(1, Math.round(diff / 60)), "minute");
  if (diff < 86400) return formatter.format(-Math.max(1, Math.round(diff / 3600)), "hour");
  return new Intl.DateTimeFormat(langCode(), {
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(value * 1000));
}

function render(data) {
  const today = data.today || {};
  const requests = Number(today.requests) || 0;
  const failed = Number(today.failed) || 0;
  const endpoint = String(data.endpoint || "");

  $("version").textContent = data.version ? `v${data.version}` : "";
  $("endpoint-text").textContent = endpoint.replace(/^https?:\/\//, "");
  $("endpoint").title = `${t("trayCopyEndpoint")} · ${endpoint}`;
  $("m-requests").textContent = requests.toLocaleString();
  $("m-tokens").textContent = formatTokens(today.tokens);
  $("m-success").textContent =
    requests > 0
      ? `${Number.isInteger(((requests - failed) / requests) * 100) ? ((requests - failed) / requests) * 100 : (((requests - failed) / requests) * 100).toFixed(1)}%`
      : "—";

  const active = Number(data.active) || 0;
  $("active-pill").hidden = active <= 0;
  $("active-count").textContent = active.toLocaleString();
  $("key-pill").textContent = `${Number(data.keys) || 0} ${t("trayKeys")}`;

  const accounts = Number(data.accounts) || 0;
  const ready = Number(data.ready_accounts) || 0;
  $("pool-summary").textContent =
    accounts === 0 ? t("trayNotConfigured") : `${ready}/${accounts} ${t("trayReady")}`;

  renderSpark(data.days || []);
  renderProviders(data.providers || []);
  renderRecent(data.recent || []);
  refreshLogos();
}

function renderSpark(days) {
  const spark = $("spark");
  const labels = $("spark-labels");
  if (!days.length) {
    spark.innerHTML = "";
    labels.innerHTML = "";
    return;
  }
  const max = Math.max(1, ...days.map((day) => Number(day.requests) || 0));
  const weekday = new Intl.DateTimeFormat(langCode(), { weekday: "short" });
  spark.innerHTML = days
    .map((day, index) => {
      const value = Number(day.requests) || 0;
      const height = value > 0 ? Math.max(18, Math.round((value / max) * 100)) : 4;
      const title = `${day.date} · ${value.toLocaleString()} ${t("trayReqUnit")} · ${formatTokens(day.tokens)} Token`;
      return `<span class="spark-bar${index === days.length - 1 ? " today" : ""}" title="${escapeHtml(title)}"><i style="--h:${height}%"></i></span>`;
    })
    .join("");
  labels.innerHTML = days
    .map((day, index) => {
      const date = new Date(`${day.date}T12:00:00`);
      const label = Number.isNaN(date.getTime()) ? "" : weekday.format(date);
      return `<span class="${index === days.length - 1 ? "today" : ""}">${escapeHtml(label)}</span>`;
    })
    .join("");
}

function renderProviders(providers) {
  providerNames.clear();
  providers.forEach((provider) => providerNames.set(provider.id, provider.label || provider.id));
  $("providers").innerHTML = providers
    .map((provider) => {
      const label = provider.label || provider.id;
      const accounts = Number(provider.accounts) || 0;
      const ready = Number(provider.ready) || 0;
      const requests = Number(provider.requests) || 0;
      const tokens = Number(provider.tokens) || 0;
      const readyClass = accounts === 0 ? "none" : ready > 0 ? "ok" : "warn";
      const readyText = accounts === 0 ? "—" : `${ready}/${accounts}`;
      const usage =
        requests > 0
          ? `<b>${requests.toLocaleString()}</b><small>${formatTokens(tokens)}</small>`
          : `<b>0</b><small>—</small>`;
      const title = `${label} · ${accounts === 0 ? t("trayNotConfigured") : `${ready}/${accounts} ${t("trayReady")}`} · ${requests.toLocaleString()} ${t("trayReqUnit")} · ${formatTokens(tokens)} Token`;
      return `<button type="button" class="provider-row" data-provider="${escapeHtml(provider.id)}" title="${escapeHtml(title)}">
        <img class="provider-logo" data-logo="${escapeHtml(provider.id)}" src="${logo(provider.id)}" alt="" />
        <span class="provider-name">${escapeHtml(label)}</span>
        <span class="provider-ready ${readyClass}">${escapeHtml(readyText)}</span>
        <span class="provider-usage">${usage}</span>
      </button>`;
    })
    .join("");
}

function renderRecent(recent) {
  const container = $("recent");
  if (!recent.length) {
    container.innerHTML = `<div class="empty-state">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <circle cx="12" cy="12" r="8.5" /><path d="M12 7.5V12l3 1.75" />
      </svg>
      <span>${escapeHtml(t("trayNoActivity"))}</span>
    </div>`;
    return;
  }
  const statusLabels = {
    success: "traySuccess",
    failed: "trayFailed",
    cancelled: "trayCancelled",
    processing: "trayProcessing",
  };
  container.innerHTML = recent
    .slice(0, 3)
    .map((item) => {
      const provider = String(item.provider || "");
      const label = providerNames.get(provider) || provider;
      const model = String(item.model || label || provider);
      const account = String(item.account || "");
      const status = Object.hasOwn(statusLabels, item.status) ? item.status : "success";
      const title = `${label} · ${model}${account ? ` · ${account}` : ""} · ${t(statusLabels[status])}${item.error ? ` · ${item.error}` : ""}`;
      return `<button type="button" class="recent-row" data-open-logs title="${escapeHtml(title)}">
        <img class="recent-logo" data-logo="${escapeHtml(provider)}" src="${logo(provider)}" alt="" />
        <span class="recent-main">
          <span class="recent-model">${escapeHtml(model)}</span>
          ${account ? `<span class="recent-account">${escapeHtml(account)}</span>` : ""}
        </span>
        <span class="recent-time">${escapeHtml(relativeTime(item.time))}</span>
        <i class="recent-status ${status}"></i>
      </button>`;
    })
    .join("");
}

function replayEntry() {
  const root = document.documentElement;
  root.classList.remove("ready");
  const panel = document.querySelector(".tray-panel");
  if (panel) void panel.offsetWidth;
  cancelAnimationFrame(replayFrame);
  replayFrame = requestAnimationFrame(() => root.classList.add("ready"));
}

function showToast(message, error = false) {
  const toast = $("toast");
  toast.textContent = message;
  toast.classList.toggle("error", error);
  toast.hidden = false;
  requestAnimationFrame(() => toast.classList.add("show"));
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toast.classList.remove("show");
    setTimeout(() => {
      toast.hidden = true;
    }, 180);
  }, 1800);
}

async function copyText(value) {
  if (navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(value);
      return;
    } catch {}
  }
  const field = document.createElement("textarea");
  field.value = value;
  field.style.position = "fixed";
  field.style.opacity = "0";
  document.body.append(field);
  field.select();
  const copied = document.execCommand("copy");
  field.remove();
  if (!copied) throw new Error(t("trayCopyFailed"));
}

async function runAction(action, extra = {}) {
  if (!inTauri) {
    if (action === "quit") window.close();
    return;
  }
  try {
    await window.__TAURI__.core.invoke("tray_action", { action, ...extra });
  } catch (error) {
    showToast(error?.message || String(error), true);
  }
}

async function copyEndpoint() {
  const value = current?.endpoint;
  if (!value) return;
  try {
    await copyText(value);
    showToast(t("trayEndpointCopied"));
  } catch (error) {
    showToast(error?.message || t("trayCopyFailed"), true);
  }
}

async function load({ silent = false } = {}) {
  if (loading) return;
  loading = true;
  try {
    const data = await api("/gateway/tray/summary");
    current = data;
    syncAppearance(data.appearance || appearance);
  } catch (error) {
    if (!silent) showToast(error?.message || "无法读取网关状态", true);
  } finally {
    loading = false;
  }
}

function bind() {
  $("open-main").addEventListener("click", () => runAction("show_main"));
  $("view-logs").addEventListener("click", () => runAction("open_logs"));
  $("endpoint").addEventListener("click", copyEndpoint);
  document.querySelectorAll("[data-action]").forEach((button) => {
    button.addEventListener("click", () => {
      const action = {
        main: "show_main",
        logs: "open_logs",
        settings: "open_settings",
        quit: "quit",
      }[button.dataset.action];
      if (action) runAction(action);
    });
  });
  $("providers").addEventListener("click", (event) => {
    const row = event.target.closest("[data-provider]");
    if (row) runAction("open_provider", { provider: row.dataset.provider });
  });
  $("recent").addEventListener("click", (event) => {
    if (event.target.closest("[data-open-logs]")) runAction("open_logs");
  });
  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      runAction("hide");
    }
  });
  window.addEventListener("focus", () => {
    replayEntry();
    load({ silent: true });
  });
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) {
      replayEntry();
      load({ silent: true });
    }
  });
  media.addEventListener?.("change", () => {
    if (appearance.theme === "system") syncAppearance(appearance);
  });
}

syncAppearance(appearance);
bind();
replayEntry();
load();
setInterval(() => {
  if (!document.hidden) load({ silent: true });
}, 10000);

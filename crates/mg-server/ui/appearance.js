import { api } from "./api.js";
import { $, notice } from "./render.js";
import { setLocale, t } from "./i18n.js";
import {
  ACCENT_PRESETS,
  normalizeAppearance,
  resolveLanguage,
  resolveTheme,
} from "./appearance-model.js";

const STORAGE_KEY = "mg-appearance";
let current = normalizeAppearance();
let statusKey = "appearanceHint";
let media;
let fetchGeneration = 0;
let saveQueue = Promise.resolve();

function prefersDark() {
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}

function readStored() {
  try {
    return normalizeAppearance(JSON.parse(localStorage.getItem(STORAGE_KEY) || "null") || {});
  } catch {
    return normalizeAppearance();
  }
}

function writeStored(settings) {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
}

function paintStatus() {
  const status = $("appearance-save-state");
  if (status) status.textContent = t(statusKey);
}

function setStatus(key) {
  statusKey = key;
  paintStatus();
}

function paintControls(settings) {
  document.querySelectorAll("[data-theme-mode]").forEach((button) => {
    const selected = button.dataset.themeMode === settings.theme;
    button.setAttribute("aria-pressed", String(selected));
  });
  const language = $("appearance-language");
  if (language) language.value = settings.language;
  const custom = $("appearance-accent-custom");
  if (custom) custom.value = settings.accent;
  const preset = ACCENT_PRESETS.includes(settings.accent);
  document.querySelectorAll("[data-accent]").forEach((button) => {
    button.setAttribute("aria-pressed", String(button.dataset.accent === settings.accent));
  });
  $("appearance-accent-custom")?.closest(".accent-custom")?.classList.toggle("is-selected", !preset);
}

function applyResolved(settings) {
  const theme = resolveTheme(settings.theme, prefersDark());
  document.documentElement.dataset.theme = theme;
  document.documentElement.style.setProperty("--orange", settings.accent);
  const language = resolveLanguage(settings.language, navigator.language);
  setLocale(language);
  paintControls(settings);
  paintStatus();
}

function setBusy(busy) {
  $("appearance-form")?.querySelectorAll("button, select, input").forEach((control) => {
    control.disabled = busy;
  });
}

export function appearanceFetchToken() {
  return fetchGeneration;
}

function commit(next) {
  const run = saveQueue.then(() => commitNow(next));
  saveQueue = run.catch(() => {});
  return run;
}

async function commitNow(next) {
  const normalized = normalizeAppearance(next);
  if (
    normalized.theme === current.theme &&
    normalized.language === current.language &&
    normalized.accent === current.accent
  ) {
    return;
  }
  fetchGeneration += 1;
  const previous = current;
  current = normalized;
  $("appearance-form").dataset.dirty = "true";
  applyResolved(normalized);
  writeStored(normalized);
  setBusy(true);
  setStatus("appearanceSaving");
  try {
    const result = await api("/settings/appearance", normalized);
    delete $("appearance-form").dataset.dirty;
    current = normalizeAppearance(result.appearance);
    applyResolved(current);
    writeStored(current);
    setStatus("appearanceSaved");
  } catch (error) {
    delete $("appearance-form").dataset.dirty;
    current = previous;
    applyResolved(previous);
    writeStored(previous);
    setStatus("appearanceFailed");
    notice(error.message, true);
  } finally {
    setBusy(false);
  }
}

export function renderAppearance(settings, token = fetchGeneration) {
  if (token !== fetchGeneration) return;
  if ($("appearance-form")?.dataset.dirty) return;
  const next = normalizeAppearance(settings);
  const changed =
    next.theme !== current.theme ||
    next.language !== current.language ||
    next.accent !== current.accent;
  current = next;
  applyResolved(current);
  writeStored(current);
  if (changed) setStatus("appearanceHint");
}

export function bindAppearance() {
  current = readStored();
  applyResolved(current);
  const form = $("appearance-form");
  if (!form || form.dataset.bound) return;
  form.dataset.bound = "true";
  form.onsubmit = (event) => event.preventDefault();
  form.addEventListener("click", (event) => {
    const theme = event.target.closest("[data-theme-mode]");
    if (theme) {
      commit({ ...current, theme: theme.dataset.themeMode });
      return;
    }
    const swatch = event.target.closest("[data-accent]");
    if (swatch) commit({ ...current, accent: swatch.dataset.accent });
  });
  form.addEventListener("change", (event) => {
    if (event.target.id === "appearance-language") {
      commit({ ...current, language: event.target.value });
    }
    if (event.target.id === "appearance-accent-custom") {
      commit({ ...current, accent: event.target.value });
    }
  });
  form.querySelector(".segmented")?.addEventListener("keydown", (event) => {
    if (event.key !== "ArrowRight" && event.key !== "ArrowLeft") return;
    const buttons = [...form.querySelectorAll("[data-theme-mode]")];
    const index = buttons.indexOf(document.activeElement);
    if (index < 0) return;
    event.preventDefault();
    const next = buttons[(index + (event.key === "ArrowRight" ? 1 : -1) + buttons.length) % buttons.length];
    next.focus();
    next.click();
  });
  media = window.matchMedia("(prefers-color-scheme: dark)");
  media.addEventListener("change", () => {
    if (current.theme === "system") applyResolved(current);
  });
  window.addEventListener("mg:locale", paintStatus);
}

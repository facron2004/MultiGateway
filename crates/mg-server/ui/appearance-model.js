export const ACCENT_PRESETS = [
  "#fa751b",
  "#009d91",
  "#3b82f6",
  "#7c3aed",
  "#e11d48",
  "#16a34a",
];

const THEMES = new Set(["system", "light", "dark"]);
const LANGUAGES = new Set(["system", "zh-CN", "en"]);
const ACCENT = /^#[0-9a-f]{6}$/;

export function normalizeAppearance(input = {}) {
  const theme = THEMES.has(input.theme) ? input.theme : "system";
  const language = LANGUAGES.has(input.language) ? input.language : "system";
  const accent = String(input.accent || "").trim().toLowerCase();
  return {
    theme,
    language,
    accent: ACCENT.test(accent) ? accent : "#fa751b",
  };
}

export function resolveTheme(mode, prefersDark) {
  if (mode === "dark") return "dark";
  if (mode === "light") return "light";
  return prefersDark ? "dark" : "light";
}

export function resolveLanguage(preference, navigatorLanguage = "") {
  if (preference === "en" || preference === "zh-CN") return preference;
  return String(navigatorLanguage).toLowerCase().startsWith("zh") ? "zh-CN" : "en";
}

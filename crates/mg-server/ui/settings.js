import { $, notice } from "./render.js";
import { copy } from "./api.js";
import { icon } from "./icons.js";
import { t } from "./i18n.js";

const pages = {
  general: { titleKey: "general", icon: "sliders" },
  gateway: { titleKey: "gateway", icon: "network" },
  keys: { titleKey: "apiKey", icon: "key" },
  data: { titleKey: "data", icon: "database" },
};
let currentPage = "general";

export function settingsPageName() {
  return currentPage;
}

export function showSettingsPage(page = currentPage) {
  currentPage = pages[page] ? page : "general";
  document.querySelectorAll("[data-settings-content]").forEach((content) => {
    content.hidden = content.dataset.settingsContent !== currentPage;
  });
  document.querySelectorAll("[data-settings-page]").forEach((button) => {
    const selected = button.dataset.settingsPage === currentPage;
    button.classList.toggle("selected", selected);
    button.setAttribute("aria-pressed", String(selected));
  });
  $("provider-name").textContent = t(pages[currentPage].titleKey);
  $("settings-page-icon").innerHTML = icon(pages[currentPage].icon);
}

let lastDataSettings = null;

export function renderDataSettings(settings) {
  lastDataSettings = settings;
  $("settings-data-dir").value = settings.data_dir || "";
  $("settings-copy-folder").disabled = !settings.data_dir;
  $("settings-open-folder").disabled = !settings.data_dir || !window.multigateway?.api?.open_folder;
  $("settings-open-folder").title = window.multigateway?.api?.open_folder
    ? t("openFolder") : t("openFolderDesktop");
  $("settings-key-backup").textContent = settings.key_backup_exists
    ? "api_keys.json.bak" : t("keyBackupPending");
}

export function bindSettings() {
  document.querySelectorAll("[data-settings-page]").forEach((button) => {
    button.onclick = () => {
      notice("");
      showSettingsPage(button.dataset.settingsPage);
      // app.js owns the URL; announce the page so it can keep the hash in sync
      // without a circular import.
      window.dispatchEvent(
        new CustomEvent("mg:route", { detail: { view: "settings", page: currentPage } }),
      );
      $("main").scrollTop = 0;
      window.scrollTo(0, 0);
    };
  });
  $("settings-open-folder").onclick = () => window.multigateway?.api?.open_folder?.();
  $("settings-copy-folder").onclick = async () => {
    try {
      await copy($("settings-data-dir").value);
      notice(t("dataDirCopied"));
    } catch (error) { notice(error.message, true); }
  };
  $("copy-unified-endpoint").onclick = async () => {
    try {
      await copy($("unified-endpoint").textContent);
      notice(t("endpointCopied"));
    } catch (error) { notice(error.message, true); }
  };
  window.addEventListener("multigatewayready", () => {
    if ($("settings-data-dir").value) {
      $("settings-open-folder").disabled = !window.multigateway?.api?.open_folder;
      $("settings-open-folder").title = window.multigateway?.api?.open_folder
        ? t("openFolder") : t("openFolderDesktop");
    }
  });
  window.addEventListener("mg:locale", () => {
    if (document.body.classList.contains("settings-open")) showSettingsPage(currentPage);
    if (lastDataSettings) renderDataSettings(lastDataSettings);
  });
}

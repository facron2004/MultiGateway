import { $, notice } from "./render.js";
import { copy } from "./api.js";
import { icon } from "./icons.js";

const pages = {
  general: { title: "通用", icon: "sliders" },
  gateway: { title: "网关", icon: "network" },
  keys: { title: "API Key", icon: "key" },
  data: { title: "数据", icon: "database" },
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
  $("provider-name").textContent = pages[currentPage].title;
  $("settings-page-icon").innerHTML = icon(pages[currentPage].icon);
}

export function renderDataSettings(settings) {
  $("settings-data-dir").value = settings.data_dir || "";
  $("settings-copy-folder").disabled = !settings.data_dir;
  $("settings-open-folder").disabled = !settings.data_dir || !window.pywebview?.api?.open_folder;
  $("settings-open-folder").title = window.pywebview?.api?.open_folder
    ? "打开数据目录" : "在桌面版中打开文件夹";
  $("settings-key-backup").textContent = settings.key_backup_exists
    ? "api_keys.json.bak" : "首次修改密钥后生成";
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
  $("settings-open-folder").onclick = () => window.pywebview?.api?.open_folder?.();
  $("settings-copy-folder").onclick = async () => {
    try {
      await copy($("settings-data-dir").value);
      notice("数据目录已复制");
    } catch (error) { notice(error.message, true); }
  };
  $("copy-unified-endpoint").onclick = async () => {
    try {
      await copy($("unified-endpoint").textContent);
      notice("统一接口地址已复制");
    } catch (error) { notice(error.message, true); }
  };
  window.addEventListener("pywebviewready", () => {
    if ($("settings-data-dir").value) {
      $("settings-open-folder").disabled = !window.pywebview?.api?.open_folder;
    }
  });
}

import { api, copy } from "./api.js";
import { $, notice, escape } from "./render.js";
import { icon } from "./icons.js";
import { formatKeyCount, t } from "./i18n.js";

const knownSecrets = new Map();

/// Revealed plaintext keys are only needed while the keys view is on screen; drop
/// them on navigation so they do not sit in page memory for the whole session.
export function clearRevealedSecrets() {
  knownSecrets.clear();
}
let savedDesktopSettings = null;
let desktopStatus = "desktopHint";
let renderedKeys = null;

function paintDesktopStatus() {
  const status = $("desktop-save-state");
  if (status) status.textContent = t(desktopStatus);
}

window.addEventListener("mg:locale", () => {
  paintDesktopStatus();
  if (renderedKeys) renderApiKeys(renderedKeys);
});

export function renderDesktopSettings(settings, supported) {
  $("desktop-section").hidden = !supported;
  if (!supported) return;
  if ($("desktop-form").dataset.dirty) return;
  savedDesktopSettings = settings;
  $("desktop-auto-start").checked = !!settings?.auto_start;
  $("desktop-start-silently").checked = !!settings?.start_silently;
  $("desktop-close-to-tray").checked = settings?.close_to_tray !== false;
  $("desktop-form").querySelectorAll("input").forEach((input) => { input.disabled = false; });
}

export function renderApiKeys(keys) {
  renderedKeys = Array.isArray(keys) ? keys : [];
  $("settings-key-count").textContent = formatKeyCount(renderedKeys.length);
  const container = $("api-keys-list");
  if (!container) return;
  if (!Array.isArray(keys) || keys.length === 0) {
    container.innerHTML = `<div class="api-keys-empty">${escape(t("noKeys"))}</div>`;
    return;
  }
  container.innerHTML = keys
    .map(
      (k) => `
    <div class="api-key-row" data-id="${escape(k.id)}">
      <div class="api-key-main">
        <div class="api-key-header">
          <strong class="api-key-name">${escape(k.name || "未命名")}</strong>
          ${k.created_at ? `<span class="api-key-time">${escape(k.created_at)}</span>` : ""}
        </div>
        <div class="api-key-secret-box">
          <code class="api-key-masked" title="${escape(k.masked)}">${escape(k.masked)}</code>
        </div>
        <div class="cell-detail api-key-meta">
          <span>${escape(k.realm === "intl" ? "国际版" : k.realm === "cn" ? "国内版" : k.realm || "未分区")}</span>
          <span title="${Array.isArray(k.models) && k.models.length ? "此 Key 仅允许调用列出的模型，其余模型返回 403" : "未设置模型白名单，可调用全部模型"}">${
            Array.isArray(k.models) && k.models.length
              ? "仅限 " + k.models.map((m) => escape(m)).join("、")
              : "不限模型"
          }</span>
          ${k.enabled === false ? "<span>已停用</span>" : ""}
        </div>
      </div>
      <div class="api-key-actions">
        <button type="button" class="key-action-btn key-copy-btn" data-key-action="copy" data-id="${escape(k.id)}" data-name="${escape(k.name || "API Key")}" title="${escape(t("copy"))}">
          ${icon("copy")}<span>${escape(t("copy"))}</span>
        </button>
        <button type="button" class="key-action-btn key-del-btn danger" data-key-action="delete" data-id="${escape(k.id)}" data-name="${escape(k.name || "API Key")}" title="删除此 Key">
          ${icon("delete")}
        </button>
      </div>
    </div>
  `,
    )
    .join("");
}

export function bindAccess(state, callbacks) {
  const { refresh } = callbacks;
  $("desktop-form").onsubmit = (event) => event.preventDefault();
  $("desktop-form").onchange = async () => {
    notice("");
    const previous = savedDesktopSettings;
    const settings = {
      auto_start: $("desktop-auto-start").checked,
      start_silently: $("desktop-start-silently").checked,
      close_to_tray: $("desktop-close-to-tray").checked,
    };
    $("desktop-form").dataset.dirty = "true";
    $("desktop-form").querySelectorAll("input").forEach((input) => { input.disabled = true; });
    desktopStatus = "desktopSaving";
    paintDesktopStatus();
    try {
      const result = await api("/settings/desktop", settings);
      delete $("desktop-form").dataset.dirty;
      renderDesktopSettings(result.desktop, true);
      desktopStatus = "desktopSaved";
      paintDesktopStatus();
    } catch (error) {
      delete $("desktop-form").dataset.dirty;
      renderDesktopSettings(previous, true);
      desktopStatus = "desktopFailed";
      paintDesktopStatus();
      notice(error.message, true);
    }
  };
  $("port-form").onsubmit = (event) => event.preventDefault();

  const keyList = $("api-keys-list");
  if (keyList) {
    keyList.onclick = async (event) => {
      const copyBtn = event.target.closest("[data-key-action='copy']");
      if (copyBtn) {
        const id = copyBtn.dataset.id;
        const name = copyBtn.dataset.name;
        let plain = knownSecrets.get(id);
        if (!plain) {
          try {
            copyBtn.disabled = true;
            const res = await api(`/settings/reveal?id=${encodeURIComponent(id)}`);
            plain = res.key;
            if (plain) knownSecrets.set(id, plain);
          } catch (err) {
            notice(`获取 API Key 失败：${err.message}`, true);
            return;
          } finally {
            copyBtn.disabled = false;
          }
        }
        if (!plain) {
          notice("无法获取该 API Key 的明文内容", true);
          return;
        }
        try {
          await copy(plain);
          const origHtml = copyBtn.innerHTML;
          copyBtn.innerHTML = `${icon("check")}<span>${escape(t("copied"))}</span>`;
          copyBtn.classList.add("copied");
          notice(`API Key「${name || "未命名"}」已复制到剪贴板，可粘贴至其他客户端使用`);
          setTimeout(() => {
            copyBtn.innerHTML = origHtml;
            copyBtn.classList.remove("copied");
          }, 1800);
        } catch (err) {
          notice(`复制失败：${err.message}`, true);
        }
        return;
      }

      const delBtn = event.target.closest("[data-key-action='delete']");
      if (delBtn) {
        const id = delBtn.dataset.id;
        const name = delBtn.dataset.name;
        if (delBtn.dataset.confirming === "true") {
          delBtn.disabled = true;
          try {
            await api("/gateway/api-keys/delete", { id });
            knownSecrets.delete(id);
            notice(`API Key「${name || "未命名"}」已删除`);
            await refresh();
          } catch (err) {
            notice(`删除失败：${err.message}`, true);
            delBtn.disabled = false;
          }
        } else {
          delBtn.dataset.confirming = "true";
          delBtn.innerHTML = `<span class="danger-text">${escape(t("confirmDelete"))}</span>`;
          setTimeout(() => {
            if (delBtn.dataset.confirming === "true") {
              delete delBtn.dataset.confirming;
              delBtn.innerHTML = icon("delete");
            }
          }, 3500);
        }
      }
    };
  }

  $("key-form").onsubmit = async (event) => {
    event.preventDefault();
    const nameInput = $("key-name");
    const name = (nameInput?.value || "").trim();
    if (!name) return;
    event.submitter.disabled = true;
    try {
      const res = await api("/gateway/api-keys/generate", { name });
      if (nameInput) nameInput.value = "";
      if (res.key) {
        knownSecrets.set(res.key.id, res.key.key);
        try {
          await copy(res.key.key);
          notice(`API Key「${res.key.name}」已生成并已复制到剪贴板！可粘贴至其他客户端使用。`);
        } catch {
          notice(`API Key「${res.key.name}」已生成！`);
        }
      }
      await refresh();
    } catch (error) {
      notice(error.message, true);
    } finally {
      event.submitter.disabled = false;
    }
  };

  function folder() {
    if (window.multigateway?.api) {
      $("folder").hidden = false;
      $("folder").onclick = () => window.multigateway.api.open_folder();
    }
  }
  window.addEventListener("multigatewayready", folder);
  folder();
}

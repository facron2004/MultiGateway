import { api, copy } from "./api.js";
import { $, notice, escape } from "./render.js";
import { icon } from "./icons.js";

const knownSecrets = new Map();

export function renderApiKeys(keys) {
  const container = $("api-keys-list");
  if (!container) return;
  if (!Array.isArray(keys) || keys.length === 0) {
    container.innerHTML = `<div class="api-keys-empty">暂无 API Key，请在下方输入名称并生成。</div>`;
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
      </div>
      <div class="api-key-actions">
        <button type="button" class="key-action-btn key-copy-btn" data-key-action="copy" data-id="${escape(k.id)}" data-name="${escape(k.name || "API Key")}" title="复制完整 API Key">
          ${icon("copy")}<span>复制</span>
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
  const { refresh, initialize, mutate, requireLogin, closeMenus } = callbacks;
  window.addEventListener("gateway-login-required", requireLogin);
  $("login").addEventListener("cancel", (event) => event.preventDefault());
  $("login-form").onsubmit = async (event) => {
    event.preventDefault();
    event.submitter.disabled = true;
    try {
      await api("/panel/login", { password: $("password").value });
      $("password").value = "";
      $("login-error").textContent = "";
      $("login").close();
      await initialize();
    } catch (error) {
      $("login-error").textContent = error.message;
    } finally {
      event.submitter.disabled = false;
    }
  };
  $("logout").onclick = async () => {
    closeMenus();
    try {
      await api("/panel/logout", {});
      requireLogin();
    } catch (error) {
      notice(error.message, true);
    }
  };
  $("port-input").oninput = () => {
    $("port-input").dataset.dirty = "true";
  };
  $("host-input").oninput = () => {
    $("host-input").dataset.dirty = "true";
  };
  $("port-form").onsubmit = async (event) => {
    event.preventDefault();
    if (!state.current) return;
    const result = await mutate(
      "gateway",
      {
        host: $("host-input").value.trim(),
        port: Number($("port-input").value),
        enabled: state.current.gateway.enabled,
      },
      event.submitter,
    );
    if (result) {
      delete $("port-input").dataset.dirty;
      delete $("host-input").dataset.dirty;
      await refresh();
    }
  };

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
          copyBtn.innerHTML = `${icon("check")}<span>已复制</span>`;
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
          delBtn.innerHTML = `<span style="font-size:11px;color:var(--red);">确定删除?</span>`;
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

  $("password-form").onsubmit = async (event) => {
    event.preventDefault();
    event.submitter.disabled = true;
    try {
      await api("/panel/password", {
        current: $("old-password").value,
        new: $("new-password").value,
      });
      event.target.reset();
      notice("面板密码已更新");
    } catch (error) {
      notice(error.message, true);
    } finally {
      event.submitter.disabled = false;
    }
  };
  function folder() {
    if (window.pywebview?.api) {
      $("folder").hidden = false;
      $("folder").onclick = () => window.pywebview.api.open_folder();
    }
  }
  window.addEventListener("pywebviewready", folder);
  folder();
}

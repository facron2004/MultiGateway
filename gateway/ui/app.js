import { api, providerApi, copy } from "./api.js";
import {
  $,
  names,
  notice,
  placeholder,
  snapshot,
  accountRows,
  regionButtons,
  scope,
  models,
  operationResults,
} from "./render.js";
import { mountIcons, logo } from "./icons.js";
import { bindAccounts } from "./accounts.js";
import { bindAccess, renderApiKeys } from "./access.js";
import { board, bindBoard, renderBoard } from "./board.js";

const stored = localStorage.getItem("gateway-provider");
const state = {
  provider: names[stored] ? stored : "workbuddy",
  view: "accounts",
  current: null,
  selected: new Set(),
  pending: new Set(),
  models: [],
};
let generation = 0,
  controller = null,
  timer = null;
const viewNames = {
  accounts: "账号与模型",
  logs: "用量统计",
  settings: "设置",
};
mountIcons();

function closeMenus() {
  ["account-menu", "more-menu", "card-menu", "token-detail-menu"].forEach(
    (id) => {
      const el = $(id);
      if (el) el.hidden = true;
    },
  );
  ["account-menu-toggle", "more-toggle", "token-detail-btn"].forEach((id) => {
    const el = $(id);
    if (el) el.setAttribute("aria-expanded", "false");
  });
}
const SIDEBAR_STORAGE_KEY = "mg_sidebar_expanded_v3";

function isSidebarExpanded() {
  return document.body.classList.contains("sidebar-expanded");
}

function setSidebarExpanded(expanded) {
  document.body.classList.toggle("sidebar-expanded", expanded);
  const toggleBtn = $("sidebar-toggle");
  if (toggleBtn) {
    toggleBtn.setAttribute(
      "title",
      expanded ? "折叠侧边栏 (Ctrl+B)" : "展开侧边栏 (Ctrl+B)",
    );
    toggleBtn.setAttribute(
      "aria-label",
      expanded ? "折叠侧边栏" : "展开侧边栏",
    );
  }
  try {
    localStorage.setItem(SIDEBAR_STORAGE_KEY, expanded ? "1" : "0");
  } catch (_) {}
}

function toggleSidebar() {
  setSidebarExpanded(!isSidebarExpanded());
}

function showView() {
  document.body.classList.remove("no-sidebar");
  const sidebar = document.querySelector(".sidebar");
  if (sidebar) sidebar.hidden = false;
  const isAccounts = state.view === "accounts";
  if ($("rail-brand")) $("rail-brand").hidden = false;
  if ($("back-button")) $("back-button").hidden = isAccounts;
  if ($("add-account")) $("add-account").hidden = !isAccounts;
  $("view-name").textContent = isAccounts ? viewNames[state.view] : "";
  $("view-name").hidden = !isAccounts;
  document
    .querySelectorAll("[data-view]")
    .forEach((button) =>
      button.classList.toggle("selected", button.dataset.view === state.view),
    );
  ["accounts", "logs", "settings"].forEach(
    (name) => ($(name + "-panel").hidden = name !== state.view),
  );
  $("models-panel").hidden = !isAccounts;
  $("region-switch").hidden = !isAccounts;
  const moreMenuContainer = $("more-toggle")?.closest(".menu-container");
  if (moreMenuContainer) moreMenuContainer.hidden = !isAccounts;
  const gwBar = document.querySelector(".gateway-bar");
  if (gwBar) gwBar.hidden = true;
  $("provider-name").textContent =
    state.view === "settings"
      ? "网关设置"
      : state.view === "logs"
        ? "用量统计"
        : names[state.provider];
  $("provider-logo").hidden = !isAccounts;
  paintRail();
  if (state.view === "logs") renderBoard();
}
function renderAccounts() {
  if (!state.current) return;
  regionButtons(state.current, state.pending.has(state.provider));
  scope(state.current, state.selected, state.pending.has(state.provider));
  accountRows(
    state.current,
    $("account-filter").value,
    state.selected,
    state.pending.has(state.provider),
  );
}
function resetProvider() {
  state.current = null;
  state.selected.clear();
  state.models = [];
  $("main").dataset.provider = state.provider;
  $("provider-name").textContent = names[state.provider];
  $("provider-logo").src = logo(state.provider);
  $("current-agent").textContent = names[state.provider];
  $("gateway-state").textContent = "连接中";
  $("gateway-dot").classList.remove("ready");
  $("endpoint").textContent = "正在读取地址…";
  $("account-count").textContent = "—";
  $("pool-summary").textContent = "正在读取账号";
  placeholder(
    "account-list",
    8,
    "正在读取 " + names[state.provider] + " 账号池…",
  );
  document
    .querySelectorAll("#region-switch button")
    .forEach((button) => (button.disabled = true));
  placeholder(
    "model-rows",
    4,
    "正在读取 " + names[state.provider] + " 模型目录…",
  );
  $("model-count").textContent = "—";
  $("port-input").value = "";
  $("host-input").value = "";
  $("unified-endpoint").textContent = "正在读取地址…";
  $("listen-host").textContent = "正在读取当前网关端口…";
  ["account-filter", "model-filter"].forEach((id) => ($(id).value = ""));
  [
    "add-account",
    "copy-endpoint",
    "gateway-toggle",
    "account-menu-toggle",
  ].forEach((id) => ($(id).disabled = true));
  paintRail();
  delete $("port-input").dataset.dirty;
  delete $("host-input").dataset.dirty;
  notice("");
  showView();
}
async function refresh() {
  controller?.abort();
  controller = new AbortController();
  const signal = controller.signal,
    name = state.provider,
    stamp = ++generation;
  $("main").setAttribute("aria-busy", "true");
  try {
    if (state.view === "logs") {
      const data = await api("/gateway/usage/board", undefined, signal);
      if (stamp !== generation) return;
      board.data = data;
      renderBoard();
      return;
    }
    const data = await providerApi(name, "snapshot", undefined, signal);
    if (stamp !== generation || name !== state.provider) return;
    if (state.current && state.current.region !== data.region) {
      state.selected.clear();
      state.models = [];
      $("model-count").textContent = "—";
      placeholder("model-rows", 4, "正在读取当前区域模型目录…");
    }
    state.current = data;
    snapshot(data);
    renderAccounts();
    [
      "add-account",
      "copy-endpoint",
      "gateway-toggle",
      "account-menu-toggle",
    ].forEach((id) => ($(id).disabled = false));
    if (state.view === "accounts") {
      try {
        const result = await providerApi(name, "models", undefined, signal);
        if (stamp !== generation || name !== state.provider) return;
        state.models = result.data;
        models(name, state.models, $("model-filter").value);
      } catch (error) {
        if (error.name === "AbortError" || stamp !== generation) return;
        state.models = [];
        $("model-count").textContent = "—";
        placeholder("model-rows", 4, "模型目录读取失败，请刷新重试。");
      }
    } else if (state.view === "settings") {
      const settings = await api("/settings", undefined, signal);
      if (stamp !== generation || name !== state.provider) return;
      renderApiKeys(settings.api_keys || []);
    }
  } catch (error) {
    if (error.name === "AbortError" || stamp !== generation) return;
    notice(`${names[name]}：${error.message}`, true);
    if (!state.current)
      placeholder("account-list", 8, "账号池读取失败，请刷新重试。");
    if (state.view === "accounts" && !state.current)
      placeholder("model-rows", 4, "模型目录读取失败，请刷新重试。");
  } finally {
    if (stamp === generation) $("main").setAttribute("aria-busy", "false");
  }
}
function switchProvider(name) {
  closeMenus();
  controller?.abort();
  generation++;
  [
    "account-dialog",
    "import-dialog",
    "confirm-dialog",
    "operation-results",
  ].forEach((id) => {
    if ($(id).open) $(id).close();
  });
  state.provider = name;
  state.view = "accounts";
  localStorage.setItem("gateway-provider", name);
  resetProvider();
  refresh();
}
function paintRail() {
  const isAccounts = state.view === "accounts";
  if ($("rail-brand")) $("rail-brand").hidden = false;
  if ($("back-button")) $("back-button").hidden = isAccounts;
  document.querySelectorAll(".provider").forEach((button) => {
    const selected = isAccounts && button.dataset.provider === state.provider;
    button.classList.toggle("selected", selected);
    button.setAttribute("aria-pressed", String(selected));
  });
  document.querySelectorAll("[data-view]").forEach((button) => {
    const selected = button.dataset.view === state.view;
    button.classList.toggle("selected", selected);
    button.setAttribute("aria-pressed", String(selected));
  });
}
async function mutate(resource, payload, button, name = state.provider) {
  if (state.pending.has(name)) return null;
  state.pending.add(name);
  if (button) button.disabled = true;
  renderAccounts();
  notice(
    `${names[name]}：正在${resource === "batch" ? "逐个处理账号" : "执行操作"}…`,
  );
  try {
    const result =
      resource === "gateway"
        ? await api("/gateway/configure", payload)
        : await providerApi(name, resource, payload);
    if (name === state.provider) {
      await refresh();
      if (resource === "batch") {
        notice(
          `完成 ${result.succeeded} 个，失败 ${result.failed} 个，跳过 ${result.skipped} 个。`,
          result.failed > 0,
        );
        operationResults(name, payload.operation, result);
      } else notice(`${names[name]}：操作已完成`);
    }
    return result;
  } catch (error) {
    notice(`${names[name]}：${error.message}`, true);
    return null;
  } finally {
    state.pending.delete(name);
    if (button) button.disabled = false;
    renderAccounts();
  }
}
function changeView(view) {
  closeMenus();
  notice("");
  state.view = view;
  showView();
  if (view === "accounts" && state.current) {
    renderAccounts();
    models(state.provider, state.models, $("model-filter").value);
  }
  refresh();
}
document.querySelectorAll(".provider").forEach((button) => {
  const name = button.dataset.provider;
  button.onclick = () => switchProvider(name);
});
document
  .querySelectorAll("[data-view]")
  .forEach(
    (button) => (button.onclick = () => changeView(button.dataset.view)),
  );
if ($("sidebar-toggle")) $("sidebar-toggle").onclick = () => toggleSidebar();
window.addEventListener("keydown", (e) => {
  if (
    (e.ctrlKey || e.metaKey) &&
    e.key.toLowerCase() === "b" &&
    !e.target.matches("input, textarea, select")
  ) {
    e.preventDefault();
    toggleSidebar();
  }
});
if (localStorage.getItem(SIDEBAR_STORAGE_KEY) === "1") {
  setSidebarExpanded(true);
}
if ($("back-button")) $("back-button").onclick = () => changeView("accounts");
$("account-home").onclick = () => changeView("accounts");
$("refresh").onclick = () => {
  closeMenus();
  notice("");
  refresh();
};
$("account-filter").oninput = renderAccounts;
$("model-filter").oninput = (event) =>
  models(state.provider, state.models, event.target.value);
$("region-switch").onclick = (event) => {
  const button = event.target.closest("[data-region]");
  if (
    !button ||
    !state.current ||
    button.dataset.region === state.current.region
  )
    return;
  state.selected.clear();
  mutate("region", { region: button.dataset.region }, button);
};
$("copy-endpoint").onclick = async () => {
  try {
    await copy($("endpoint").textContent);
    notice("当前网关地址已复制");
  } catch (error) {
    notice(error.message, true);
  }
};
$("gateway-toggle").onclick = () =>
  state.current &&
  mutate(
    "gateway",
    {
      port: state.current.gateway.port,
      enabled: !state.current.gateway.running,
    },
    $("gateway-toggle"),
  );
$("model-rows").onclick = async (event) => {
  const button = event.target.closest("[data-copy-model]");
  if (button)
    try {
      await copy(button.dataset.copyModel);
      notice("模型名已复制");
    } catch (error) {
      notice(error.message, true);
    }
};
$("account-list").onchange = (event) => {
  const id = event.target.dataset.selectAccount;
  if (!id) return;
  if (event.target.checked) state.selected.add(id);
  else state.selected.delete(id);
  renderAccounts();
};
$("select-all").onchange = (event) => {
  state.selected.clear();
  if (event.target.checked)
    state.current?.accounts
      .filter((row) => row.region === state.current.region)
      .forEach((row) => state.selected.add(row.id));
  renderAccounts();
};
document.querySelectorAll("[data-batch]").forEach(
  (button) =>
    (button.onclick = () => {
      closeMenus();
      mutate(
        "batch",
        {
          operation: button.dataset.batch,
          ids: state.selected.size
            ? [...state.selected]
            : state.current.accounts
                .filter((row) => row.region === state.current.region)
                .map((row) => row.id),
        },
        button,
      );
    }),
);
for (const [buttonId, menuId] of [
  ["account-menu-toggle", "account-menu"],
  ["more-toggle", "more-menu"],
  ["token-detail-btn", "token-detail-menu"],
]) {
  const btn = $(buttonId);
  const menu = $(menuId);
  if (btn && menu) {
    btn.onclick = () => {
      const open = menu.hidden;
      closeMenus();
      menu.hidden = !open;
      btn.setAttribute("aria-expanded", String(open));
      if (open) menu.querySelector("button:not(:disabled)")?.focus();
    };
  }
}
document.addEventListener("click", (event) => {
  if (!event.target.closest(".menu-container,.account-actions,.card-menu"))
    closeMenus();
});
document.addEventListener("keydown", (event) => {
  const menu = event.target.closest("[role=menu]");
  if (event.key === "Escape") {
    const hasOpenMenu = ["account-menu", "more-menu", "card-menu", "token-detail-menu"].some(
      (id) => !$(id)?.hidden,
    );
    if (hasOpenMenu) {
      closeMenus();
      return;
    }
    const hasOpenDialog = document.querySelector("dialog[open]");
    if (hasOpenDialog) return;
    if (state.view !== "accounts") {
      changeView("accounts");
      return;
    }
    closeMenus();
    return;
  }
  if (!menu || !["ArrowDown", "ArrowUp"].includes(event.key)) return;
  event.preventDefault();
  const buttons = [...menu.querySelectorAll("button:not(:disabled)")];
  buttons[
    (buttons.indexOf(document.activeElement) +
      (event.key === "ArrowDown" ? 1 : -1) +
      buttons.length) %
      buttons.length
  ]?.focus();
});
bindAccounts(state, refresh, mutate, closeMenus);
bindBoard(paintRail);
function requireLogin() {
  clearInterval(timer);
  controller?.abort();
  generation++;
  closeMenus();
  resetProvider();
  [
    "account-dialog",
    "import-dialog",
    "confirm-dialog",
    "operation-results",
  ].forEach((id) => {
    if ($(id).open) $(id).close();
  });
  if (!$("login").open) $("login").showModal();
}
bindAccess(state, { refresh, initialize, mutate, requireLogin, closeMenus });
async function initialize() {
  clearInterval(timer);
  try {
    const status = await api("/panel/status");
    if (!status.authenticated) {
      requireLogin();
      return;
    }
  } catch (error) {
    notice(error.message, true);
    return;
  }
  resetProvider();
  await refresh();
  if ($("login").open) return;
  timer = setInterval(() => {
    if (
      !document.hidden &&
      $("main").getAttribute("aria-busy") !== "true" &&
      !state.pending.has(state.provider) &&
      !document.activeElement?.matches("input,textarea,select") &&
      !document.querySelector("dialog[open]") &&
      ["account-menu", "more-menu", "card-menu"].every((id) => $(id).hidden)
    )
      refresh();
  }, 10000);
  for (const name of Object.keys(names).filter(
    (name) => name !== state.provider,
  )) {
    providerApi(name, "snapshot")
      .then((data) => {
        const badge = document.querySelector(`[data-port="${name}"]`);
        badge.classList.toggle("ready", data.gateway.running && !data.error);
        badge.parentElement.title = `${names[name]} · 端口 ${data.gateway.port}`;
      })
      .catch(() => {});
  }
}
function initWindowControls() {
  const winControls = $("window-controls");
  if (!winControls) return;

  $("win-min")?.addEventListener("click", (e) => {
    e.stopPropagation();
    window.pywebview?.api?.window_minimize?.();
  });
  $("win-max")?.addEventListener("click", (e) => {
    e.stopPropagation();
    window.pywebview?.api?.window_maximize?.();
  });
  $("win-close")?.addEventListener("click", (e) => {
    e.stopPropagation();
    window.pywebview?.api?.window_close?.();
  });

  const topbar = document.querySelector(".topbar");
  topbar?.addEventListener("dblclick", (e) => {
    if (e.target.closest("button, a, input, select, textarea, .dropdown, [role='menuitem']")) return;
    window.pywebview?.api?.window_maximize?.();
  });

  topbar?.addEventListener(
    "mousedown",
    (e) => {
      if (e.target.closest("button, a, input, select, textarea, .dropdown, [role='menuitem']")) {
        e.stopPropagation();
      }
    },
    true,
  );
}
initWindowControls();
initialize();

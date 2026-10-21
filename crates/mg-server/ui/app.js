import { api, providerApi, copy } from "./api.js";
import {
  $,
  names,
  usageUnit,
  notice,
  placeholder,
  snapshot,
  accountRows,
  regionButtons,
  scope,
  models,
  operationResults,
  debounce,
} from "./render.js";
import { mountIcons, logo } from "./icons.js";
import { bindAccounts } from "./accounts.js";
import { bindAccess, renderApiKeys, renderDesktopSettings, clearRevealedSecrets } from "./access.js";
import { board, bindBoard, renderBoard } from "./board.js";
import { bindSettings, showSettingsPage, renderDataSettings, settingsPageName } from "./settings.js";

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
  timer = null,
  lastRefreshAt = 0;
const snapshotCache = new Map();
const modelCache = new Map();
const MODEL_TTL = 60000;
const modelCacheKey = (provider, region) => `${provider}:${region}`;
const viewNames = {
  accounts: "账号与模型",
  logs: "用量统计",
  settings: "设置",
};
mountIcons();

let lastMenuTrigger = null;

const MENU_IDS = ["account-menu", "more-menu", "card-menu", "token-detail-menu"];

function closeMenus() {
  // Not every menu exists in every view, so resolve elements and drop nulls
  // instead of assuming each id is present.
  const openMenus = MENU_IDS.map((id) => $(id)).filter((el) => el && !el.hidden);
  openMenus.forEach((el) => {
    el.hidden = true;
  });
  ["account-menu-toggle", "more-toggle", "token-detail-btn"].forEach((id) => {
    const el = $(id);
    if (el) el.setAttribute("aria-expanded", "false");
  });
  // Return focus to the trigger when the user was operating the menu by keyboard,
  // instead of dropping focus on <body>.
  if (openMenus.length && lastMenuTrigger && document.contains(lastMenuTrigger)) {
    const active = document.activeElement;
    const focusWasInMenu = openMenus.some((el) => el.contains(active));
    if (focusWasInMenu || active === document.body) lastMenuTrigger.focus();
  }
  lastMenuTrigger = null;
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
  const sidebar = document.querySelector(".sidebar");
  if (sidebar) sidebar.hidden = false;
  const isAccounts = state.view === "accounts";
  const isSettings = state.view === "settings";
  document.body.classList.toggle("settings-open", isSettings);
  document.querySelector(".agent-list").hidden = isSettings;
  $("settings-navigation").hidden = !isSettings;
  $("settings-page-icon").hidden = !isSettings;
  if ($("rail-brand")) $("rail-brand").hidden = false;
  if ($("back-button")) $("back-button").hidden = isAccounts || isSettings;
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
  $("provider-name").textContent =
    state.view === "settings"
      ? "网关设置"
      : state.view === "logs"
        ? "用量统计"
        : names[state.provider];
  $("provider-logo").hidden = !isAccounts;
  paintRail();
  if (isSettings) showSettingsPage();
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
  const unit = usageUnit(state.provider);
  $("account-usage-label").textContent = unit;
  $("model-usage-label").textContent = `消耗${unit}系数`;
  $("refresh-usage-label").textContent = `刷新${unit}`;
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
  ["add-account", "account-menu-toggle"].forEach((id) => ($(id).disabled = true));
  paintRail();
  notice("");
  showView();
  const cachedSnapshot = snapshotCache.get(state.provider);
  if (!cachedSnapshot) return;
  state.current = cachedSnapshot;
  snapshot(cachedSnapshot);
  renderAccounts();
  ["add-account", "account-menu-toggle"].forEach((id) => ($(id).disabled = false));
  const cachedModels = modelCache.get(
    modelCacheKey(state.provider, cachedSnapshot.region),
  );
  if (cachedModels) {
    state.models = cachedModels.data;
    models(state.provider, state.models, $("model-filter").value);
  }
}
function refreshStatus(signal) {
  return api("/gateway/status", undefined, signal)
    .then((status) => {
      for (const [name, data] of Object.entries(status)) {
        const badge = document.querySelector(`[data-port="${name}"]`);
        if (!badge) continue;
        badge.classList.toggle("ready", data.running && !data.error);
        badge.parentElement.title = `${names[name]} · ${
          data.error || (data.running ? "运行中" : "已停止")
        }`;
      }
    })
    .catch(() => {});
}
async function refresh() {
  controller?.abort();
  controller = new AbortController();
  const signal = controller.signal,
    name = state.provider,
    stamp = ++generation;
  const view = state.view;
  $("main").setAttribute("aria-busy", "true");
  refreshStatus(signal);
  try {
    if (view === "logs") {
      const data = await api("/gateway/usage/board", undefined, signal);
      if (stamp !== generation) return;
      board.data = data;
      renderBoard();
      return;
    }
    const settingsPromise =
      view === "settings"
        ? api("/settings", undefined, signal).then(
            (value) => ({ value }),
            (error) => ({ error }),
          )
        : null;
    const data = await providerApi(name, "snapshot", undefined, signal);
    if (stamp !== generation || name !== state.provider || view !== state.view)
      return;
    if (state.current && state.current.region !== data.region) {
      state.selected.clear();
      state.models = [];
      $("model-count").textContent = "—";
      placeholder("model-rows", 4, "正在读取当前区域模型目录…");
    }
    state.current = data;
    snapshotCache.set(name, data);
    snapshot(data);
    renderAccounts();
    ["add-account", "account-menu-toggle"].forEach(
      (id) => ($(id).disabled = false),
    );
    if (view === "accounts") {
      const cacheKey = modelCacheKey(name, data.region);
      const cached = modelCache.get(cacheKey);
      if (cached && Date.now() - cached.time < MODEL_TTL) {
        state.models = cached.data;
        models(name, state.models, $("model-filter").value);
      } else {
        const result = await providerApi(
          name,
          `models?region=${encodeURIComponent(data.region)}`,
          undefined,
          signal,
        ).then(
          (value) => ({ value }),
          (error) => ({ error }),
        );
        if (
          stamp !== generation ||
          name !== state.provider ||
          view !== state.view
        )
          return;
        if (result.error) {
          if (result.error.name === "AbortError") return;
          if (cached) {
            state.models = cached.data;
            models(name, state.models, $("model-filter").value);
          } else {
            state.models = [];
            $("model-count").textContent = "—";
            placeholder("model-rows", 4, "模型目录读取失败，请刷新重试。");
          }
        } else {
          state.models = result.value.data;
          modelCache.set(cacheKey, { time: Date.now(), data: state.models });
          models(name, state.models, $("model-filter").value);
        }
      }
    } else if (view === "settings") {
      const result = await settingsPromise;
      if (stamp !== generation || name !== state.provider || view !== state.view)
        return;
      if (result.error) throw result.error;
      renderApiKeys(result.value.api_keys || []);
      renderDesktopSettings(result.value.desktop, result.value.desktop_supported);
      renderDataSettings(result.value);
    }
  } catch (error) {
    if (error.name === "AbortError" || stamp !== generation) return;
    notice(`${names[name]}：${error.message}`, true);
    if (!state.current)
      placeholder("account-list", 8, "账号池读取失败，请刷新重试。");
    if (state.view === "accounts" && !state.current)
      placeholder("model-rows", 4, "模型目录读取失败，请刷新重试。");
  } finally {
    if (stamp === generation) {
      $("main").setAttribute("aria-busy", "false");
      lastRefreshAt = Date.now();
    }
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
  if ($("back-button")) $("back-button").hidden = isAccounts || state.view === "settings";
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
    const result = await providerApi(name, resource, payload);
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
  clearRevealedSecrets();
  state.view = view;
  writeHash();
  showView();
  if (view === "accounts" && state.current) {
    renderAccounts();
    models(state.provider, state.models, $("model-filter").value);
  }
  refresh();
}

const ROUTABLE_VIEWS = ["accounts", "logs", "settings"];
let lastWrittenHash = "";

function parseHash() {
  const raw = decodeURIComponent(location.hash || "").replace(/^#\/?/, "");
  const [view, page] = raw.split("/");
  return {
    view: ROUTABLE_VIEWS.includes(view) ? view : null,
    page: page || null,
  };
}

function writeHash() {
  const target =
    state.view === "settings"
      ? `#/settings/${settingsPageName()}`
      : `#/${state.view}`;
  if (location.hash !== target) {
    // Remember our own writes so the hashchange handler can ignore them and only
    // react to real user navigation (back/forward/manual edits).
    lastWrittenHash = target;
    location.hash = target;
  }
}

function applyHash() {
  const { view, page } = parseHash();
  if (!view) return;
  if (view === "settings") showSettingsPage(page || "general");
  if (view !== state.view) changeView(view);
  else showView();
}

window.addEventListener("hashchange", () => {
  if (location.hash === lastWrittenHash) return;
  applyHash();
});
window.addEventListener("mg:route", () => writeHash());
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
$("account-filter").oninput = debounce(renderAccounts, 150);
$("model-filter").oninput = debounce(
  (event) => models(state.provider, state.models, event.target.value),
  150,
);
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
      if (open) {
        lastMenuTrigger = btn;
        menu.querySelector("button:not(:disabled)")?.focus();
      }
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
bindAccess(state, { refresh, mutate });
bindSettings();
async function initialize() {
  clearInterval(timer);
  resetProvider();
  // Surface the serving build so a stale desktop window is distinguishable from a
  // freshly rebuilt gateway.
  api("/health")
    .then((health) => {
      const el = $("gateway-version");
      if (el && health.version) el.textContent = `v${health.version}`;
    })
    .catch(() => {
      const el = $("gateway-version");
      if (el) el.textContent = "无法读取";
    });
  await refresh();
  timer = setInterval(() => {
    if (document.hidden) return;
    const blocked =
      $("main").getAttribute("aria-busy") === "true" ||
      state.pending.has(state.provider) ||
      document.querySelector("dialog[open]") ||
      !["account-menu", "more-menu", "card-menu"].every((id) => $(id).hidden);
    if (blocked) return;
    const focused = document.activeElement?.matches("input,textarea,select");
    const stale = Date.now() - lastRefreshAt > 60000;
    if (!focused || stale) refresh();
  }, 10000);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) refresh();
  });
}
function initWindowControls() {
  const winControls = $("window-controls");
  if (!winControls) return;
  const reveal = () => {
    winControls.hidden = false;
  };
  if (window.pywebview?.api) reveal();
  else window.addEventListener("pywebviewready", reveal);

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

// Land on the view the URL points at so refreshes and shared links restore
// context instead of always resetting to the account list.
{
  const { view, page } = parseHash();
  if (view) {
    state.view = view;
    if (view === "settings") showSettingsPage(page || "general");
  }
  showView();
  writeHash();
}

initialize();

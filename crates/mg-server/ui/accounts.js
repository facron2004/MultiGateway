import { providerApi, external } from "./api.js";
import { $, names, escape, notice, usageUnit } from "./render.js";
import { icon } from "./icons.js";

export function bindAccounts(state, refresh, mutate, closeMenus) {
  let deletion = null,
    loginContext = null,
    epoch = 0,
    pollTimer = null;
  async function executeRowAction(action, rowId, trigger) {
    if (!state.current) return;
    const row = state.current.accounts.find((item) => item.id === rowId);
    if (!row) return;
    closeMenus();
    if (action === "delete") {
      deletion = { provider: state.provider, id: row.id };
      $("confirm-message").textContent =
        `从 ${names[state.provider]} 账号池移除“${row.name}”的本机凭证。`;
      $("confirm-dialog").showModal();
    } else {
      await mutate(
        "batch",
        { operation: action, ids: [row.id] },
        trigger,
      );
    }
  }

  $("account-list").onclick = (event) => {
    const toggle = event.target.closest("[data-account-action]");
    if (toggle && state.current) {
      const row = state.current.accounts.find(
        (item) => item.id === toggle.dataset.id,
      );
      if (row) mutate("enable", { id: row.id, enabled: !row.enabled }, toggle);
      return;
    }
    const rowAction = event.target.closest("[data-row-action]");
    if (rowAction) {
      executeRowAction(rowAction.dataset.rowAction, rowAction.dataset.id, rowAction);
      return;
    }
    const menu = event.target.closest("[data-card-menu]");
    if (!menu || !state.current) return;
    const row = state.current.accounts.find(
      (item) => item.id === menu.dataset.cardMenu,
    );
    if (!row) return;
    closeMenus();
    const actions = [
      ["credits", "刷新" + usageUnit(state.provider), "refresh"],
      ["credentials", "刷新凭证", "refresh"],
      ["checkin", "完成签到", "checkin"],
      ["delete", "移除账号", "delete"],
    ];
    $("card-menu").innerHTML =
      `<p>${escape(row.name)}</p>` +
      actions
        .map(
          ([action, label, glyph]) =>
            `<button role="menuitem" data-row-action="${action}" data-id="${escape(row.id)}" ${(action !== "delete" && !state.current.capabilities[action]) || (action === "checkin" && !row.checkin?.supported) ? "disabled" : ""}>${icon(glyph)}${label}</button>`,
        )
        .join("");
    const rect = menu.getBoundingClientRect();
    $("card-menu").style.left =
      Math.max(8, Math.min(rect.right - 205, window.innerWidth - 220)) + "px";
    $("card-menu").style.top =
      Math.max(8, Math.min(rect.bottom + 7, window.innerHeight - 210)) + "px";
    $("card-menu").hidden = false;
    $("card-menu").querySelector("button:not(:disabled)")?.focus();
  };
  $("card-menu").onclick = (event) => {
    const button = event.target.closest("[data-row-action]");
    if (button) {
      executeRowAction(button.dataset.rowAction, button.dataset.id, button);
    }
  };
  $("confirm-delete").onclick = () => {
    if (deletion)
      mutate("delete", { id: deletion.id }, null, deletion.provider);
    $("confirm-dialog").close();
  };
  document
    .querySelectorAll("[data-close]")
    .forEach(
      (button) => (button.onclick = () => $(button.dataset.close).close()),
    );
  document.querySelectorAll("dialog").forEach((dialog) => {
    if (dialog.id === "login") return;
    let startedOnBackdrop = false;
    dialog.addEventListener("mousedown", (event) => {
      if (event.target === dialog) {
        const rect = dialog.getBoundingClientRect();
        const isInside =
          rect.top <= event.clientY &&
          event.clientY <= rect.bottom &&
          rect.left <= event.clientX &&
          event.clientX <= rect.right;
        startedOnBackdrop = !isInside;
      } else {
        startedOnBackdrop = false;
      }
    });
    dialog.addEventListener("click", (event) => {
      if (startedOnBackdrop && event.target === dialog) {
        const rect = dialog.getBoundingClientRect();
        const isInside =
          rect.top <= event.clientY &&
          event.clientY <= rect.bottom &&
          rect.left <= event.clientX &&
          event.clientX <= rect.right;
        if (!isInside) {
          dialog.close();
        }
      }
      startedOnBackdrop = false;
    });
  });
  $("account-dialog").addEventListener("close", () => {
    epoch++;
    clearTimeout(pollTimer);
    loginContext = null;
  });
  $("add-account").onclick = () => {
    if (!state.current) return;
    epoch++;
    loginContext = { provider: state.provider, epoch };
    $("account-dialog-title").textContent =
      "添加 " + names[state.provider] + " 账号";
    $("login-region").innerHTML =
      state.provider === "minimax"
        ? '<option value="cn">国内账号</option><option value="global">国际账号</option>'
        : '<option value="cn">国内账号</option><option value="intl">国际账号</option>';
    $("login-region").value = state.current.region;
    $("login-region").disabled = !state.current.capabilities.regions;
    $("account-form").hidden = false;
    $("authorization").hidden = true;
    $("begin-login").disabled = false;
    $("login-instructions").textContent =
      "在浏览器完成官方授权，账号会加入当前账号池。";
    $("account-dialog").showModal();
  };
  async function poll(context, login, interval) {
    if (
      !loginContext ||
      context.epoch !== epoch ||
      context.provider !== state.provider
    )
      return;
    try {
      const result = await providerApi(context.provider, "poll", {
        login_id: login.login_id,
        state: login.state,
      });
      if (
        !loginContext ||
        context.epoch !== epoch ||
        context.provider !== state.provider
      )
        return;
      if (["ok", "authenticated"].includes(result.status)) {
        $("authorization-state").textContent = "授权成功，账号已加入账号池。";
        await refresh();
        return;
      }
      if (["error", "unknown", "expired"].includes(result.status))
        throw new Error(result.message || "授权已失效，请重试");
      pollTimer = setTimeout(
        () => poll(context, login, result.interval || interval),
        (result.interval || interval) * 1000,
      );
    } catch (error) {
      if (context.epoch === epoch)
        $("authorization-state").textContent = error.message;
    }
  }
  $("account-form").onsubmit = async (event) => {
    event.preventDefault();
    const context = loginContext;
    if (!context) return;
    $("begin-login").disabled = true;
    try {
      const login = await providerApi(context.provider, "login", {
        region: $("login-region").value,
      });
      if (
        !loginContext ||
        context.epoch !== epoch ||
        context.provider !== state.provider
      )
        return;
      const url = login.url || login.authUrl;
      if (!/^https?:\/\//.test(url)) throw new Error("授权地址不受支持");
      $("device-code").textContent = login.user_code || "";
      $("device-code").hidden = !login.user_code;
      $("authorize-link").href = url;
      $("authorization-state").textContent = "等待官方页面完成授权…";
      $("account-form").hidden = true;
      $("authorization").hidden = false;
      external(url);
      if (context.provider !== "trae")
        pollTimer = setTimeout(
          () => poll(context, login, login.interval || 3),
          (login.interval || 3) * 1000,
        );
    } catch (error) {
      if (context.epoch === epoch)
        $("login-instructions").textContent = error.message;
    } finally {
      if (context.epoch === epoch) $("begin-login").disabled = false;
    }
  };
  $("authorize-link").onclick = (event) => {
    if (window.multigateway?.api) {
      event.preventDefault();
      external(event.currentTarget.href);
    }
  };
  $("login-complete").onclick = () => {
    $("account-dialog").close();
    refresh();
  };
  $("import-account").onclick = () => {
    closeMenus();
    $("import-title").textContent = "导入 " + names[state.provider] + " 账号";
    $("import-data").value = "";
    $("import-dialog").dataset.provider = state.provider;
    $("import-dialog").showModal();
  };
  $("import-dialog").addEventListener("close", () => {
    $("import-data").value = "";
  });
  $("import-form").onsubmit = async (event) => {
    event.preventDefault();
    try {
      const value = JSON.parse($("import-data").value);
      const result = await mutate(
        "import",
        { data: value },
        event.submitter,
        $("import-dialog").dataset.provider,
      );
      if (result && $("import-dialog").open) $("import-dialog").close();
    } catch (error) {
      notice("JSON 格式有误：" + error.message, true);
    }
  };
}

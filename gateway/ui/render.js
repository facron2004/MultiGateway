import { icon } from "./icons.js";
export const $ = (id) => document.getElementById(id);
export const names = {
  workbuddy: "WorkBuddy",
  trae: "Trae",
  minimax: "MiniMax Code",
  qoder: "Qoder",
  antigravity: "Antigravity",
  grok: "Grok",
  copilot: "GitHub Copilot",
};
export const regions = { cn: "国内", intl: "国际", global: "国际" };
export const usageUnit = (provider) =>
  ["antigravity", "grok", "copilot"].includes(provider) ? "额度" : "积分";
export const escape = (value) =>
  String(value ?? "").replace(
    /[&<>"']/g,
    (char) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        char
      ],
  );
export const number = (value) =>
  value === null || value === undefined || !Number.isFinite(Number(value))
    ? "未知"
    : Number(value).toLocaleString("zh-CN");
export function date(value) {
  if (!value) return "未知";
  const stamp =
    typeof value === "number"
      ? new Date(value * (value > 1e12 ? 1 : 1000))
      : new Date(value);
  return Number.isNaN(stamp.getTime())
    ? String(value)
    : stamp.toLocaleString("zh-CN", {
        month: "2-digit",
        day: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
        hour12: false,
      });
}
let noticeTimer;
export function notice(message, error = false) {
  clearTimeout(noticeTimer);
  $("notice").textContent = message;
  $("notice").hidden = !message;
  $("notice").classList.toggle("error", error);
  if (message && !error && !message.includes("正在"))
    noticeTimer = setTimeout(() => {
      $("notice").hidden = true;
    }, 4500);
}
export function placeholder(target, columns, message) {
  $(target).innerHTML =
    `<tr><td colspan="${columns}"><div class="empty">${escape(message)}</div></td></tr>`;
}
export function snapshot(data) {
  const { provider, gateway, pool, capabilities } = data;
  const address = new URL(location.origin);
  if (gateway.host !== "0.0.0.0") address.hostname = gateway.host;
  address.port = gateway.port;
  $("endpoint").textContent = address.origin + "/" + provider + "/v1";
  $("unified-endpoint").textContent = address.origin + "/v1";
  $("gateway-state").textContent = gateway.running ? "运行中" : "已停止";
  $("gateway-dot").classList.toggle("ready", gateway.running);
  $("gateway-toggle").innerHTML = icon(gateway.running ? "pause" : "play");
  $("gateway-toggle").setAttribute(
    "aria-label",
    gateway.running ? "停止网关" : "启动网关",
  );
  $("gateway-toggle").title = gateway.running ? "停止统一网关" : "启动统一网关";
  $("current-agent").textContent = names[provider];
  const rail = document.querySelector(`[data-port="${provider}"]`);
  rail.classList.toggle("ready", gateway.running && !data.error);
  rail.parentElement.title = `${names[provider]} · /${provider}/v1 · 统一端口 ${gateway.port}`;
  if (!$("port-input").dataset.dirty) $("port-input").value = gateway.port;
  if (!$("host-input").dataset.dirty) $("host-input").value = gateway.host;
  $("listen-host").textContent =
    `${Object.keys(names).length} 个 Agent 分组共用此地址、端口和访问 Key。`;
  $("import-account").hidden = !capabilities.import;
  $("account-count").textContent = pool.total;
  $("pool-summary").textContent =
    `${regions[data.region]}区域 ${pool.region_total} 个账号 · ${pool.ready} 个可调度`;
  $("updated").textContent =
    "更新于 " + new Date().toLocaleTimeString("zh-CN", { hour12: false });
  if (data.error || gateway.error) notice(data.error || gateway.error, true);
}
export function regionButtons(data, busy = false) {
  const singleRegion = !data.capabilities.regions;
  $("region-switch").hidden = singleRegion || $("accounts-panel").hidden;
  document
    .querySelectorAll("#region-switch button")
    .forEach((button, index) => {
      const region =
        index === 0 ? "cn" : data.provider === "minimax" ? "global" : "intl";
      button.dataset.region = region;
      const selected = region === data.region;
      button.classList.toggle("selected", selected);
      button.setAttribute("aria-pressed", String(selected));
      button.disabled = busy || (index === 1 && singleRegion);
      button.title =
        index === 1 && singleRegion ? "当前 Agent 暂不支持国际版" : "";
    });
}
export function accountRows(data, filter, selected, busy = false) {
  const rows = data.accounts.filter((row) =>
    `${row.name} ${row.id} ${regions[row.region] || ""}`
      .toLowerCase()
      .includes(filter.toLowerCase()),
  );
  if (!rows.length) {
    placeholder(
      "account-list",
      8,
      filter
        ? "没有匹配的账号"
        : names[data.provider] + " 账号池为空，点击顶栏「添加账号」完成授权。",
    );
    return;
  }
  $("account-list").innerHTML = rows
    .map((row) => {
      const current = row.region === data.region;
      const active = current && row.enabled && row.ready;
      const status = !row.enabled
        ? "已停用"
        : !current
          ? "其他区域"
          : row.ready
            ? "可用"
            : "暂不可用";
      const checkin = !row.checkin?.supported
        ? "不支持"
        : row.checkin.done === true
          ? "已签到"
          : row.checkin.done === false
            ? "未签到"
            : "未知";
      return `<tr class="account-row ${active ? "active" : ""} ${selected.has(row.id) ? "picked" : ""}" data-account="${escape(row.id)}">
      <td><input type="checkbox" data-select-account="${escape(row.id)}" aria-label="选择 ${escape(row.name)}" ${selected.has(row.id) ? "checked" : ""} ${current && !busy ? "" : "disabled"}></td>
      <td><span class="region-label ${row.region === "cn" ? "china" : "global"}">${escape(regions[row.region] || row.region)}版</span></td>
      <td class="account-name"><strong>${escape(row.name)}</strong><div class="cell-detail"><code title="${escape(row.id)}">${escape(row.id.slice(0, 22))}</code><br>会话有效期 ${escape(date(row.expires_at))}</div></td>
      <td><span class="account-state ${active ? "ready" : ""}">${status}</span>${row.error ? `<div class="account-error" title="${escape(row.error)}">${escape(row.error)}</div>` : ""}</td>
      <td><strong class="account-credits ${row.remaining == null ? "muted" : ""}">${row.remaining === "unlimited" ? "无限制" : number(row.remaining)}</strong></td>
      <td><span class="${row.checkin?.done === true ? "checked-in" : "muted"}">${checkin}</span></td>
      <td class="token-count" title="${row.daily_tokens == null ? "上游未返回 Token 用量" : row.daily_tokens_complete === false ? "部分请求未返回用量，仅汇总上游已返回的实际 Token" : "今日输入与输出 Token 总量"}">${row.daily_tokens == null ? "—" : number(row.daily_tokens)}</td>
      <td><div class="account-actions">
        <button class="action-icon-btn ${row.enabled ? "btn-pause" : "btn-play"}" data-account-action="enable" data-id="${escape(row.id)}" aria-label="${row.enabled ? "停用账号" : "启用账号"}" title="${row.enabled ? "停用账号" : "启用账号"}" ${busy ? "disabled" : ""}>${icon(row.enabled ? "pause" : "play")}</button>
        <button class="action-icon-btn" data-row-action="credits" data-id="${escape(row.id)}" aria-label="刷新${usageUnit(data.provider)}" title="${!data.capabilities.credits ? "不支持刷新" : "刷新"}${usageUnit(data.provider)}" ${busy || !data.capabilities.credits ? "disabled" : ""}>${icon("refresh")}</button>
        <button class="action-icon-btn" data-row-action="credentials" data-id="${escape(row.id)}" aria-label="刷新凭证" title="${!data.capabilities.credentials ? "不支持刷新凭证" : "刷新凭证"}" ${busy || !data.capabilities.credentials ? "disabled" : ""}>${icon("key")}</button>
        <button class="action-icon-btn" data-row-action="checkin" data-id="${escape(row.id)}" aria-label="完成签到" title="${!data.capabilities.checkin || !row.checkin?.supported ? "不支持签到" : "完成签到"}" ${busy || !data.capabilities.checkin || !row.checkin?.supported ? "disabled" : ""}>${icon("checkin")}</button>
        <button class="action-icon-btn btn-delete" data-row-action="delete" data-id="${escape(row.id)}" aria-label="移除账号" title="移除账号" ${busy ? "disabled" : ""}>${icon("delete")}</button>
      </div></td>
    </tr>`;
    })
    .join("");
}
export function scope(data, selected, busy) {
  const rows = data.accounts.filter((row) => row.region === data.region);
  const valid = new Set(rows.map((row) => row.id));
  for (const id of selected) if (!valid.has(id)) selected.delete(id);
  $("selection-label").textContent = selected.size
    ? "已选 " + selected.size
    : "全选";
  $("select-all").checked = rows.length > 0 && selected.size === rows.length;
  $("select-all").indeterminate =
    selected.size > 0 && selected.size < rows.length;
  $("select-all").disabled = !rows.length;
  $("operation-scope").textContent = selected.size
    ? `已选 ${selected.size} 个 ${names[data.provider]} 账号`
    : `当前${regions[data.region]}区域 · ${rows.length} 个账号`;
  document.querySelectorAll("[data-batch]").forEach((button) => {
    const action = button.dataset.batch;
    const unsupported =
      ["credits", "checkin", "credentials"].includes(action) &&
      !data.capabilities[action];
    const noCheckinTarget =
      action === "checkin" &&
      !rows.some(
        (row) =>
          (!selected.size || selected.has(row.id)) && row.checkin?.supported,
      );
    button.disabled = busy || !rows.length || unsupported || noCheckinTarget;
    button.title = unsupported
      ? `${names[data.provider]} 未提供${{ checkin: "签到", credits: usageUnit(data.provider) + "查询", credentials: "凭证刷新" }[action]}接口`
      : noCheckinTarget
        ? "当前区域没有支持签到的账号"
        : "";
  });
}
const statuses = {
  success: "成功",
  failed: "失败",
  processing: "处理中",
  cancelled: "已取消",
};
export { statuses };
function factor(value) {
  const match = String(value ?? "")
    .trim()
    .match(/^(?:x|×)?\s*(\d+(?:\.\d+)?)\s*(?:x|×)?$/i);
  return match && Number.isFinite(Number(match[1]))
    ? Number(match[1]).toLocaleString("zh-CN", { maximumFractionDigits: 4 }) +
        "×"
    : "未知";
}
function modelCredits(row) {
  let value = row.credits ?? row.price_factor;
  if (row.off_peak_active_now === true)
    value = row.price_factor_valley ?? value;
  else if (row.off_peak_active_now === false)
    value = row.price_factor_peak ?? value;
  const current = factor(value);
  let detail = current === "未知" ? "上游未提供" : "";
  if (row.price_factor_peak != null && row.price_factor_valley != null) {
    detail = `高峰 ${factor(row.price_factor_peak)} · 低谷 ${factor(row.price_factor_valley)}`;
    const window = row.off_peak;
    if (window?.window_start && window?.window_end)
      detail += ` · 低谷 ${window.window_start}–${window.window_end}${window.timezone ? "（" + window.timezone + "）" : ""}`;
  }
  return `<span class="credit-factor${current === "未知" ? " muted" : ""}">${current}</span>${detail ? `<div class="cell-detail">${escape(detail)}</div>` : ""}`;
}
function formatTokens(n) {
  if (!n) return "";
  if (typeof n === "string") {
    const num = Number(n.replace(/[^\d.]/g, ""));
    if (Number.isFinite(num)) n = num;
    else return n;
  }
  return n >= 1000000 ? (Math.round(n / 100000) / 10) + "M" : Math.round(n / 1000) + "K";
}

export function models(provider, rows, filter = "") {
  const visible = rows.filter((row) =>
    row.id.toLowerCase().includes(filter.toLowerCase()),
  );
  $("model-count").textContent = rows.length;
  if (!visible.length)
    return placeholder(
      "model-rows",
      4,
      filter ? "没有匹配的模型" : "暂无模型目录，请添加账号后刷新。",
    );
  $("model-rows").innerHTML = visible
    .map((row) => {
      const disp =
        row.display_name &&
        row.display_name.toLowerCase() !== row.id.toLowerCase()
          ? row.display_name
          : "";
      const baseCtx = row.context_window || row.context;
      const maxCtx = row.max_context;
      const hasMax = row.max_mode || (maxCtx && maxCtx > (baseCtx || 0));
      let ctxDetails = [];
      if (disp) ctxDetails.push(escape(disp));
      if (baseCtx) ctxDetails.push(`上下文 ${formatTokens(baseCtx)}`);
      if (hasMax && maxCtx) {
        ctxDetails.push(
          `<span class="max-badge" title="支持 Max 模式扩大至 ${formatTokens(maxCtx)} 上下文">Max ${formatTokens(maxCtx)}</span>`,
        );
      }
      const detailHtml = ctxDetails.length
        ? `<div class="cell-detail">${ctxDetails.join(" · ")}</div>`
        : "";
      return `<tr><td><code>${escape(row.id)}</code>${detailHtml}</td><td>${modelCredits(row)}</td><td><code>${escape(provider + "/" + row.id)}</code></td><td><button class="link-button" data-copy-model="${escape(row.id)}">复制模型名</button></td></tr>`;
    })
    .join("");
}
export function operationResults(provider, operation, result) {
  const labels = {
    checkin: "签到",
    credits: "刷新" + usageUnit(provider),
    credentials: "刷新凭证",
    enable: "启用账号",
    disable: "停用账号",
  };
  $("operation-title").textContent =
    `${names[provider]} · ${labels[operation]}结果`;
  $("operation-summary").textContent =
    `完成 ${result.succeeded} 个，失败 ${result.failed} 个，跳过 ${result.skipped} 个。`;
  $("operation-rows").innerHTML = result.results
    .map(
      (row) =>
        `<div class="operation-row"><span class="badge ${row.ok ? "success" : row.skipped ? "" : "failed"}">${row.ok ? "完成" : row.skipped ? "跳过" : "失败"}</span><div>${escape(row.name)}<p>${escape(row.message)}</p></div></div>`,
    )
    .join("");
  $("operation-results").showModal();
}

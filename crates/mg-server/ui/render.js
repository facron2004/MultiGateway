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
export function notice(message, error = false) {
  const host = $("notice");
  clearTimeout(host._dismissTimer);
  host._dismissTimer = null;
  if (!message) {
    host.replaceChildren();
    host.hidden = true;
    host.setAttribute("aria-live", "polite");
    return;
  }
  host.hidden = false;
  host.setAttribute("aria-live", error ? "assertive" : "polite");
  const item = document.createElement("div");
  item.className = error ? "notice-item error" : "notice-item";
  item.textContent = message;
  host.replaceChildren(item);
  if (!message.includes("正在")) {
    host._dismissTimer = setTimeout(() => {
      if (host.firstElementChild === item) {
        host.replaceChildren();
        host.hidden = true;
      }
    }, error ? 8000 : 4500);
  }
}
export function debounce(fn, wait = 150) {
  let timer;
  return (...args) => {
    clearTimeout(timer);
    timer = setTimeout(() => fn(...args), wait);
  };
}
const rowTemplate = document.createElement("template");
function parseRow(html) {
  rowTemplate.innerHTML = html;
  return rowTemplate.content.firstElementChild;
}
export function patchRows(tbody, items, keyOf, htmlOf, decorate) {
  const previous = tbody._patched || (tbody._patched = new Map());
  const pool = new Map();
  for (const child of Array.from(tbody.children)) {
    const key = child.dataset.rowKey;
    if (key !== undefined && !pool.has(key)) pool.set(key, child);
  }
  const next = new Map();
  const ordered = [];
  for (const item of items) {
    let key = String(keyOf(item));
    if (next.has(key)) {
      let suffix = 2;
      while (next.has(key + "#" + suffix)) suffix++;
      key = key + "#" + suffix;
    }
    const html = htmlOf(item);
    next.set(key, html);
    let node = pool.get(key);
    if (node) {
      pool.delete(key);
      if (previous.get(key) !== html) {
        const fresh = parseRow(html);
        for (const attr of Array.from(node.attributes))
          if (!fresh.hasAttribute(attr.name))
            node.removeAttribute(attr.name);
        for (const attr of Array.from(fresh.attributes))
          if (node.getAttribute(attr.name) !== attr.value)
            node.setAttribute(attr.name, attr.value);
        if (node.innerHTML !== fresh.innerHTML)
          node.innerHTML = fresh.innerHTML;
      }
    } else {
      node = parseRow(html);
    }
    if (decorate) decorate(node, item);
    ordered.push(node);
  }
  let cursor = tbody.firstElementChild;
  for (const node of ordered) {
    if (cursor === node) {
      cursor = cursor.nextElementSibling;
      continue;
    }
    tbody.insertBefore(node, cursor);
  }
  while (cursor) {
    const rest = cursor.nextElementSibling;
    cursor.remove();
    cursor = rest;
  }
  tbody._patched = next;
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
  $("unified-endpoint").textContent = address.origin + "/v1";
  const rail = document.querySelector(`[data-port="${provider}"]`);
  rail.classList.toggle("ready", gateway.running && !data.error);
  rail.parentElement.title = `${names[provider]} · /${provider}/v1 · 统一端口 ${gateway.port}`;
  $("port-input").value = gateway.port;
  $("host-input").value = gateway.host;
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
function quotaAmount(value, unit) {
  if (value === "unlimited") return "无限制";
  if (value == null) return "—";
  return number(value) + (unit === "%" ? "%" : unit ? " " + unit : "");
}
function accountQuota(row, provider) {
  const quota = row.quota || {};
  const details = quota.details || {};
  const unlimited = details.unlimited === true;
  if (usageUnit(provider) !== "额度") {
    return `<strong class="account-credits ${row.remaining == null && !unlimited ? "muted" : ""}">${row.remaining === "unlimited" || unlimited ? "无限制" : number(row.remaining)}</strong>`;
  }
  const buckets = quota.buckets || details.buckets || [];
  const unit = quota.unit || details.unit;
  const updatedAt = quota.updated_at || quota.checked_at;
  const tooltip = buckets.map((bucket) =>
    `${bucket.label}：${quotaAmount(bucket.remaining, bucket.unit)}${bucket.limit != null ? " / " + number(bucket.limit) : ""}${bucket.reset_at ? "，重置 " + date(bucket.reset_at) : ""}`,
  );
  if (updatedAt) tooltip.push("更新于 " + date(updatedAt));
  if (quota.error) tooltip.push("刷新失败：" + quota.error);
  const detail = [];
  if (quota.label) detail.push(quota.label);
  else if (details.label) detail.push(details.label);
  if (buckets.length > 3) detail.push(`${buckets.length} 个模型，悬停查看`);
  else for (const bucket of buckets.slice(1)) {
    detail.push(`${bucket.label}：${quotaAmount(bucket.remaining, bucket.unit)}`);
  }
  if (quota.reset_at) detail.push("重置 " + date(quota.reset_at));
  if (updatedAt) detail.push("更新 " + date(updatedAt));
  if (quota.error) detail.push(row.remaining == null ? "额度查询失败" : "刷新失败，显示上次结果");
  else if (row.remaining == null) detail.push("尚未查询额度");
  return `<strong class="account-credits ${row.remaining == null && !unlimited ? "muted" : ""}" title="${escape(tooltip.join("\n"))}">${escape(unlimited ? "无限制" : quotaAmount(row.remaining, unit))}</strong><div class="cell-detail" title="${escape(tooltip.join("\n"))}">${detail.map(escape).join("<br>")}</div>`;
}
function accountRowHtml(row, data, busy) {
  const current = row.region === data.region;
  const active = current && row.enabled && row.ready;
  const cooling = (row.cooldown_until || 0) > Date.now() / 1000;
  const status = !row.enabled
    ? "已停用"
    : !current
      ? "其他区域"
      : cooling
        ? "冷却中"
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
  return `<tr class="account-row ${active ? "active" : ""}" data-row-key="${escape(row.id)}" data-account="${escape(row.id)}">
      <td><input type="checkbox" data-select-account="${escape(row.id)}" aria-label="选择 ${escape(row.name)}" ${current && !busy ? "" : "disabled"}></td>
      <td><span class="region-label ${row.region === "cn" ? "china" : "global"}">${escape(regions[row.region] || row.region)}版</span></td>
      <td class="account-name"><strong>${escape(row.name)}</strong><div class="cell-detail"><code title="${escape(row.id)}">${escape(row.id.slice(0, 22))}</code><br>会话有效期 ${escape(date(row.expires_at))}</div></td>
      <td><span class="account-state ${active ? "ready" : ""}"${cooling ? ' title="上游近期返回错误，该账号暂停调度约 60 秒后自动恢复"' : ""}>${status}</span>${row.error ? `<div class="account-error" title="${escape(row.error)}">${escape(row.error)}</div>` : ""}</td>
      <td>${accountQuota(row, data.provider)}</td>
      <td><span class="${row.checkin?.done === true ? "checked-in" : "muted"}">${checkin}</span></td>
      <td class="token-count" title="${row.daily_tokens == null ? "上游未返回 Token 用量" : row.daily_tokens_complete === false ? "部分请求未返回用量，仅汇总上游已返回的实际 Token" : "今日输入与输出 Token 总量"}">${row.daily_tokens == null ? "—" : number(row.daily_tokens)}</td>
      <td><div class="account-actions">
        <button class="action-icon-btn ${row.enabled ? "btn-pause" : "btn-play"}" data-account-action="enable" data-id="${escape(row.id)}" aria-label="${row.enabled ? "停用账号" : "启用账号"}" title="${row.enabled ? "停用账号" : "启用账号"}" ${busy ? "disabled" : ""}>${icon(row.enabled ? "pause" : "play")}</button>
        <button class="action-icon-btn" data-row-action="credits" data-id="${escape(row.id)}" aria-label="刷新${usageUnit(data.provider)}" title="${!data.capabilities.credits ? "不支持刷新" : "刷新"}${usageUnit(data.provider)}" ${busy || !data.capabilities.credits ? "disabled" : ""}>${icon("refresh")}</button>
        <button class="action-icon-btn" data-row-action="credentials" data-id="${escape(row.id)}" aria-label="刷新凭证" title="${!data.capabilities.credentials ? "不支持刷新凭证" : "刷新凭证"}" ${busy || !data.capabilities.credentials ? "disabled" : ""}>${icon("key")}</button>
        <button class="action-icon-btn" data-row-action="checkin" data-id="${escape(row.id)}" aria-label="完成签到" title="${!data.capabilities.checkin || !row.checkin?.supported ? "不支持签到" : "完成签到"}" ${busy || !data.capabilities.checkin || !row.checkin?.supported ? "disabled" : ""}>${icon("checkin")}</button>
        <button class="action-icon-btn danger" data-row-action="delete" data-id="${escape(row.id)}" aria-label="移除账号" title="移除账号（删除本机凭证）" ${busy ? "disabled" : ""}>${icon("delete")}</button>
        <button class="action-icon-btn btn-delete" data-row-action="delete" data-id="${escape(row.id)}" aria-label="移除账号" title="移除账号" ${busy ? "disabled" : ""}>${icon("delete")}</button>
      </div></td>
    </tr>`;
}
export function accountRows(data, filter, selected, busy = false) {
  const rows = data.accounts.filter((row) =>
    `${row.name} ${row.id} ${regions[row.region] || ""}`
      .toLowerCase()
      .includes(filter.toLowerCase()),
  );
  const live = $("account-status");
  if (!rows.length) {
    if (live)
      live.textContent = filter ? "没有匹配的账号" : "0 个账号";
    placeholder(
      "account-list",
      8,
      filter
        ? "没有匹配的账号"
        : names[data.provider] + " 账号池为空，点击顶栏「添加账号」完成授权。",
    );
    return;
  }
  patchRows(
    $("account-list"),
    rows,
    (row) => row.id,
    (row) => accountRowHtml(row, data, busy),
    (node, row) => {
      node.classList.toggle("picked", selected.has(row.id));
      const box = node.querySelector("input[data-select-account]");
      if (box) box.checked = selected.has(row.id);
    },
  );
  if (live) live.textContent = `${rows.length} 个账号，已更新`;
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
    else return escape(n);
  }
  return n >= 1000000 ? (Math.round(n / 100000) / 10) + "M" : Math.round(n / 1000) + "K";
}

function modelRowHtml(provider, row) {
  const disp =
    row.display_name &&
    (provider === "trae"
      ? row.display_name !== row.id
      : row.display_name.toLowerCase() !== row.id.toLowerCase())
      ? row.display_name
      : "";
  const label = provider === "trae" && disp ? disp : row.id;
  const gatewayName = `${provider}/${label}`;
  const baseCtx = row.context_window || row.context;
  const maxCtx = row.max_context;
  const hasMax = row.max_mode || (maxCtx && maxCtx > (baseCtx || 0));
  let ctxDetails = [];
  if (disp && provider !== "trae") ctxDetails.push(escape(disp));
  if (baseCtx) ctxDetails.push(`上下文 ${formatTokens(baseCtx)}`);
  if (hasMax && maxCtx) {
    ctxDetails.push(
      `<span class="max-badge" title="支持 Max 模式扩大至 ${formatTokens(maxCtx)} 上下文">Max ${formatTokens(maxCtx)}</span>`,
    );
  }
  const detailHtml = ctxDetails.length
    ? `<div class="cell-detail">${ctxDetails.join(" · ")}</div>`
    : "";
  return `<tr data-row-key="${escape(row.id)}"><td><code>${escape(label)}</code>${detailHtml}</td><td>${capabilityHtml(row)}</td><td>${modelCredits(row)}</td><td><code>${escape(gatewayName)}</code></td><td><button class="link-button" data-copy-model="${escape(row.id)}">复制模型名</button></td></tr>`;
}

/// What a model can do, in the two rows a client cares about: inputs (does it
/// read images, how much output can it take) and thinking strength. Every field
/// is optional so a provider that publishes nothing still renders cleanly.
function capabilityHtml(row) {
  const badges = [];
  if (row.supports_vision) badges.push('<span class="cap-chip">识图</span>');
  if (row.supports_tool_calls) badges.push('<span class="cap-chip">工具</span>');
  if (row.max_output_tokens)
    badges.push(
      `<span class="cap-chip" title="最大输出">出 ${formatTokens(row.max_output_tokens)}</span>`,
    );
  if (row.supports_reasoning) {
    const efforts = row.reasoning_efforts || [];
    const def = row.reasoning_default_effort;
    const tiers = efforts.length
      ? efforts
          .map((tier) => (tier === def ? `<b>${escape(tier)}</b>` : escape(tier)))
          .join("/")
      : "可思考";
    const off = row.reasoning_can_disable ? "/关" : "";
    badges.push(
      `<span class="cap-chip" title="思考强度（加粗为默认）">思考 ${tiers}${off}</span>`,
    );
  }
  return badges.length
    ? badges.join("")
    : '<span class="muted">—</span>';
}

export function models(provider, rows, filter = "") {
  const loweredFilter = filter.toLowerCase();
  const visible = rows.filter((row) =>
    row.id.toLowerCase().includes(loweredFilter) ||
    (row.display_name || "").toLowerCase().includes(loweredFilter),
  );
  $("model-count").textContent = rows.length;
  if (!visible.length)
    return placeholder(
      "model-rows",
      5,
      filter ? "没有匹配的模型" : "暂无模型目录，请添加账号后刷新。",
    );
  patchRows(
    $("model-rows"),
    visible,
    (row) => row.id,
    (row) => modelRowHtml(provider, row),
  );
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

import { $, names, escape, date, statuses, usageUnit, patchRows, debounce } from "./render.js";
import { logo, icon } from "./icons.js";
import { t } from "./i18n.js";

const PROVIDERS = [...Object.keys(names)];
let notify = () => {};

export const board = {
  data: { rows: [], facets: { providers: [], models: [], accounts: [] } },
  agents: new Set(),
  model: "",
  account: "",
  range: "today",
  status: "",
  session: "",
  search: "",
  metric: "tokens",
  page: 1,
  pageSize: 10,
};

export function formatTokens(value) {
  const num = Number(value);
  if (!Number.isFinite(num) || num <= 0) return "0";
  if (num >= 1e9) {
    const v = num / 1e9;
    return (v >= 10 ? v.toFixed(1) : v.toFixed(2)).replace(/\.?0+$/, "") + "B";
  }
  if (num >= 1e6) {
    const v = num / 1e6;
    if (v >= 100) return Math.round(v) + "M";
    if (v >= 10) return v.toFixed(1).replace(/\.0$/, "") + "M";
    return v.toFixed(2).replace(/\.?0+$/, "") + "M";
  }
  if (num >= 1e3) {
    const v = num / 1e3;
    if (v >= 100) return Math.round(v) + "K";
    return v.toFixed(1).replace(/\.0$/, "") + "K";
  }
  return String(Math.round(num));
}

export function formatCostUSD(rows) {
  let total = 0;
  for (const row of rows) {
    if (row.cost_usd != null && Number.isFinite(Number(row.cost_usd))) {
      total += Number(row.cost_usd);
    } else {
      const prompt = Number(row.input_tokens) || 0;
      const cached = Math.min(Number(row.cached_tokens) || 0, prompt);
      const fresh = Math.max(prompt - cached, 0);
      const output = Number(row.output_tokens) || 0;
      const m = String(row.model || "").toLowerCase();
      let rates = [0.2, 0.05, 0.8];
      if (m.includes("deepseek")) rates = [0.14, 0.014, 0.28];
      else if (m.includes("glm")) rates = [0.15, 0.03, 0.5];
      else if (m.includes("step")) rates = [0.2, 0.05, 1.0];
      else if (m.includes("claude") && (m.includes("sonnet") || m.includes("opus")))
        rates = [3.0, 0.3, 15.0];
      else if (m.includes("claude")) rates = [0.8, 0.08, 4.0];
      else if (m.includes("gpt-4o-mini")) rates = [0.15, 0.075, 0.6];
      else if (m.includes("gpt-4") || m.includes("gpt-5")) rates = [2.5, 1.25, 10.0];
      else if (m.includes("qwen")) rates = [0.2, 0.05, 0.6];
      total += (fresh * rates[0] + cached * rates[1] + output * rates[2]) / 1000000;
    }
  }
  return "$" + total.toFixed(2);
}

function compact(value) {
  const number = Number(value);
  if (value === null || value === undefined || !Number.isFinite(number))
    return "—";
  const trim = (scaled, unit) =>
    String(Number(scaled.toFixed(1))).replace(/\.0$/, "") + unit;
  if (Math.abs(number) >= 1e8) return trim(number / 1e8, "亿");
  if (Math.abs(number) >= 1e4) return trim(number / 1e4, "万");
  return number.toLocaleString("zh-CN");
}

function duration(ms) {
  const value = Number(ms);
  if (ms === null || ms === undefined || !Number.isFinite(value)) return "—";
  if (value < 1000) return Math.round(value) + " ms";
  if (value < 60000) return (value / 1000).toFixed(1) + " s";
  return (
    Math.floor(value / 60000) +
    " 分 " +
    String(Math.round((value % 60000) / 1000)).padStart(2, "0") +
    " 秒"
  );
}

function credit(value) {
  const number = Number(value);
  if (value === null || value === undefined || !Number.isFinite(number))
    return "—";
  return number.toLocaleString("zh-CN", { maximumFractionDigits: 2 });
}

function known(rows, key) {
  return rows
    .map((row) => row[key])
    .filter((value) => value !== null && value !== undefined);
}

function sum(rows, key) {
  return rows.reduce((total, row) => total + (Number(row[key]) || 0), 0);
}

function average(values) {
  return values.length ? values.reduce((total, value) => total + value, 0) / values.length : null;
}

function cutoff() {
  if (board.range === "all") return 0;
  if (board.range === "today") {
    const day = new Date();
    day.setHours(0, 0, 0, 0);
    return day.getTime() / 1000;
  }
  return Date.now() / 1000 - Number(board.range) * 86400;
}

export function visibleRows() {
  const needle = board.search.trim().toLowerCase();
  const from = cutoff();
  return board.data.rows.filter(
    (row) =>
      (!board.agents.size || board.agents.has(row.provider)) &&
      (!board.model || row.provider + "/" + row.model === board.model) &&
      (!board.account || row.provider + "/" + row.account === board.account) &&
      (!board.status ||
        (board.status === "failed"
          ? row.status === "failed" || row.status === "cancelled"
          : row.status === board.status)) &&
      (!board.session || row.session_id === board.session) &&
      (row.timestamp || 0) >= from &&
      (!needle ||
        `${row.model} ${row.session_id} ${row.account} ${row.account_name}
         ${row.error} ${names[row.provider] || row.provider}`
          .toLowerCase()
          .includes(needle)),
  );
}

function kpis(rows, unit) {
  const cached = sum(rows, "cached_tokens");
  const fresh = sum(rows, "new_input_tokens");
  const prompt = sum(rows, "input_tokens");
  const output = sum(rows, "output_tokens");
  const totalTokens = prompt + output;
  const credits = sum(rows, "credits");
  const hitRate = prompt ? `${((cached / prompt) * 100).toFixed(1)}%` : "0.0%";

  return {
    mainCards: [
      {
        label: "总成本（估算）",
        value: formatCostUSD(rows),
        hasInfo: true,
        info: "按各模型公开的 $/1M Token 参考价估算，仅供参考",
      },
      {
        label: "总请求数",
        value: rows.length.toLocaleString("zh-CN"),
        hasInfo: false,
      },
      {
        label: "真实消耗 Tokens",
        value: formatTokens(totalTokens),
        hasInfo: true,
        info: "Prompt 计费 Tokens 与 Output 输出 Tokens 总和",
      },
      {
        label: "缓存命中率",
        value: hitRate,
        hasInfo: true,
        info: "缓存命中 Tokens 占总输入 Tokens 的比例",
      },
    ],
    subCards: [
      {
        label: "新增输入",
        value: formatTokens(fresh),
      },
      {
        label: "Output",
        value: formatTokens(output),
      },
      {
        label: `${unit}消耗`,
        value: credit(credits),
      },
      {
        label: "命中",
        value: formatTokens(cached),
      },
    ],
  };
}

export function bucketize(rows) {
  if (!rows || !rows.length) return [];

  const now = new Date();
  const year = now.getFullYear();
  const month = now.getMonth();
  const dayOfMonth = now.getDate();
  const todayStart = new Date(year, month, dayOfMonth, 0, 0, 0, 0).getTime();

  // 1. 当天: 按分时走 (只显示到当前小时及之前: 00:00 - now.getHours():00)
  if (board.range === "today") {
    const currentHour = now.getHours();
    const buckets = [];
    const labelStep = currentHour <= 6 ? 1 : currentHour <= 12 ? 2 : 3;

    for (let h = 0; h <= currentHour; h++) {
      const timeStr = `${String(h).padStart(2, "0")}:00`;
      const endStr = `${String(h).padStart(2, "0")}:59`;
      const showLabel =
        h % labelStep === 0 ||
        (h === currentHour && currentHour - Math.floor(currentHour / labelStep) * labelStep >= 2);

      buckets.push({
        at: (todayStart + h * 3600000) / 1000,
        hour: h,
        rows: [],
        labelShown: showLabel ? timeStr : "",
        tooltip: `${String(month + 1).padStart(2, "0")}/${String(dayOfMonth).padStart(2, "0")} ${timeStr}-${endStr}`,
      });
    }
    rows.forEach((row) => {
      if (!row.timestamp) return;
      const rd = new Date(row.timestamp * 1000);
      if (rd.getFullYear() === year && rd.getMonth() === month && rd.getDate() === dayOfMonth) {
        const h = rd.getHours();
        if (h <= currentHour && buckets[h]) buckets[h].rows.push(row);
      }
    });
    return buckets;
  }

  // 2. 7天内: 按分日走 (7 calendar days ending today)
  // 3. 30天内: 按分日走 (30 calendar days ending today)
  const rangeDays = { 7: 6, 30: 29 }[board.range];
  if (rangeDays !== undefined) {
    const buckets = [];
    for (let i = rangeDays; i >= 0; i--) {
      const d = new Date(todayStart - i * 86400000);
      const mStr = String(d.getMonth() + 1).padStart(2, "0");
      const dStr = String(d.getDate()).padStart(2, "0");
      const dateStr = `${mStr}/${dStr}`;
      buckets.push({
        at: d.getTime() / 1000,
        dayStart: d.getTime(),
        dayEnd: d.getTime() + 86400000,
        rows: [],
        labelShown: rangeDays === 6 ? dateStr : (rangeDays - i) % 5 === 0 || i === 0 ? dateStr : "",
        tooltip: `${d.getFullYear()}/${mStr}/${dStr}`,
      });
    }
    const firstDayStart = buckets[0].dayStart;
    rows.forEach((row) => {
      if (!row.timestamp) return;
      const index = Math.floor(
        (row.timestamp * 1000 - firstDayStart) / 86400000,
      );
      const bucket = buckets[index];
      if (bucket) bucket.rows.push(row);
    });
    return buckets;
  }

  // 4. 全部: 按分日走 (Daily from earliest date up to today, min 7 days)
  const stamps = known(rows, "timestamp");
  if (!stamps.length) return [];
  const minTime = Math.min(...stamps) * 1000;
  const minD = new Date(minTime);
  const earliestStart = new Date(minD.getFullYear(), minD.getMonth(), minD.getDate(), 0, 0, 0, 0).getTime();
  let days = Math.round((todayStart - earliestStart) / 86400000) + 1;
  if (days < 7) days = 7;

  const buckets = [];
  const startDayTime = todayStart - (days - 1) * 86400000;
  const labelInterval = Math.max(Math.ceil(days / 8), 1);

  for (let i = 0; i < days; i++) {
    const d = new Date(startDayTime + i * 86400000);
    const mStr = String(d.getMonth() + 1).padStart(2, "0");
    const dStr = String(d.getDate()).padStart(2, "0");
    const dateStr = `${mStr}/${dStr}`;
    const show = i % labelInterval === 0 || i === days - 1;
    buckets.push({
      at: d.getTime() / 1000,
      dayStart: d.getTime(),
      dayEnd: d.getTime() + 86400000,
      rows: [],
      labelShown: show ? dateStr : "",
      tooltip: `${d.getFullYear()}/${mStr}/${dStr}`,
    });
  }
  rows.forEach((row) => {
    if (!row.timestamp) return;
    const index = Math.floor(
      (row.timestamp * 1000 - startDayTime) / 86400000,
    );
    const bucket = buckets[index];
    if (bucket) bucket.rows.push(row);
  });
  return buckets;
}

function metricValue(rows) {
  if (board.metric === "requests") return rows.length;
  if (board.metric === "credits") return sum(rows, "credits");
  return sum(rows, "input_tokens") + sum(rows, "output_tokens");
}

function setHtml(el, html) {
  if (!el || el._lastHtml === html) return;
  el._lastHtml = html;
  el.innerHTML = html;
}

let chartRectCache = null;
let tipCache = null;
function invalidateChartCache() {
  chartRectCache = null;
  tipCache = null;
  const tooltipEl = $("chart-tooltip");
  if (tooltipEl) tooltipEl.hidden = true;
}
window.addEventListener("resize", invalidateChartCache);
window.addEventListener("scroll", invalidateChartCache, true);

function renderChart(rows, unit) {
  const buckets = bucketize(rows);
  const peak = Math.max(...buckets.map((bucket) => metricValue(bucket.rows)), 1);
  const metricLabel = { tokens: "Tokens", requests: "请求", credits: unit }[board.metric];
  $("chart-title").textContent = `使用趋势 · ${
    { today: "当天", 7: "7天内", 30: "30天内", all: "全部" }[board.range]
  } · ${metricLabel}`;

  const chartBody = $("chart-body");
  const tooltipEl = $("chart-tooltip");
  tipCache = null;

  if (!buckets.length) {
    setHtml(chartBody, '<div class="chart-empty">筛选后没有请求</div>');
    if (tooltipEl) tooltipEl.hidden = true;
    return;
  }

  setHtml(
    chartBody,
    buckets
      .map((bucket, index) => {
        const value = metricValue(bucket.rows);
        const height = value ? Math.max(2, Math.round((value / peak) * 112)) : 0;
        const shown = bucket.labelShown || "";
        const bar = height ? `<i style="height:${height}px"></i>` : "";
        return `<div class="chart-col" data-idx="${index}"><b>${bar}</b><span>${escape(shown)}</span></div>`;
      })
      .join(""),
  );

  if (tooltipEl) {
    chartBody.onmouseleave = () => {
      tooltipEl.hidden = true;
      tipCache = null;
    };

    chartBody.onmousemove = (event) => {
      const col = event.target.closest(".chart-col");
      if (!col || !chartBody.contains(col)) {
        tooltipEl.hidden = true;
        tipCache = null;
        return;
      }
      const idx = Number(col.dataset.idx);
      const bucket = buckets[idx];
      if (!bucket) {
        tooltipEl.hidden = true;
        tipCache = null;
        return;
      }
      if (tipCache && tipCache.idx === idx) return;

      const value = metricValue(bucket.rows);
      const formattedVal =
        board.metric === "credits"
          ? credit(value) + " " + unit
          : compact(value) + (board.metric === "tokens" ? " Tokens" : " 次");
      const reqCount = bucket.rows.length;

      tooltipEl.innerHTML = `
        <div class="chart-tooltip-time">${escape(bucket.tooltip)}</div>
        <div class="chart-tooltip-row">
          <span class="chart-tooltip-label">请求次数</span>
          <strong class="chart-tooltip-val">${reqCount} 次</strong>
        </div>
        <div class="chart-tooltip-row">
          <span class="chart-tooltip-label">${metricLabel}</span>
          <strong class="chart-tooltip-val accent">${formattedVal}</strong>
        </div>
      `;
      tooltipEl.hidden = false;

      const chartContainer = col.closest(".board-chart");
      if (!chartRectCache || chartRectCache.el !== chartContainer)
        chartRectCache = {
          el: chartContainer,
          rect: chartContainer.getBoundingClientRect(),
        };
      const chartRect = chartRectCache.rect;
      const bar = col.querySelector("i");
      const track = col.querySelector("b").getBoundingClientRect();
      const anchor = bar
        ? bar.getBoundingClientRect()
        : { left: track.left, width: track.width, top: track.bottom, bottom: track.bottom };
      const tipWidth = tooltipEl.offsetWidth || 160;
      const tipHeight = tooltipEl.offsetHeight || 68;
      const rawX = anchor.left + anchor.width / 2 - chartRect.left;
      const clampedX = Math.max(tipWidth / 2 + 8, Math.min(rawX, chartRect.width - tipWidth / 2 - 8));
      const arrowShift = Math.max(-(tipWidth / 2 - 14), Math.min(tipWidth / 2 - 14, rawX - clampedX));
      const head = chartContainer.querySelector(".chart-head");
      const headBottom = head ? head.getBoundingClientRect().bottom - chartRect.top + 6 : 36;
      const aboveTop = anchor.top - chartRect.top - 10 - tipHeight;
      const below = aboveTop < headBottom;
      tooltipEl.classList.toggle("below", below);
      tooltipEl.style.setProperty("--arrow-shift", arrowShift + "px");
      const y = below ? anchor.bottom - chartRect.top + 10 : anchor.top - chartRect.top - 10;
      tipCache = { idx };
      tooltipEl.style.transform = `translate(${clampedX}px, ${y}px) translate(-50%, ${below ? "0" : "-100%"})`;
    };
  }
}

function renderChips() {
  const counts = {};
  (board.data.facets.providers || []).forEach((row) => (counts[row.id] = row.count));
  const total = Object.values(counts).reduce((a, b) => a + b, 0);
  setHtml(
    $("agent-chips"),
    `<button class="chip${board.agents.size ? "" : " selected"}" data-agent="" title="全部 Agent · ${total} 条请求">
      <span>全部</span>
    </button>` +
      PROVIDERS.map(
        (name) =>
          `<button class="chip${board.agents.has(name) ? " selected" : ""}" data-agent="${name}"
          title="${escape(names[name])} · ${counts[name] || 0} 条请求" aria-pressed="${board.agents.has(name)}" aria-label="${escape(names[name])}">
          <img src="${escape(logo(name))}" alt="${escape(names[name])}">
        </button>`,
      ).join(""),
  );
}

const signatures = {};

function fillOptions(id, options) {
  const signature = JSON.stringify(options);
  if (signatures[id] === signature) return;
  signatures[id] = signature;
  const select = $(id);
  const current = select.value;
  select.innerHTML =
    `<option value="">${id === "model-select" ? "全部模型" : "全部账号"}</option>` +
    options
      .map(
        (option) =>
          `<option value="${escape(option.value)}">${escape(option.label)}</option>`,
      )
      .join("");
  select.value = options.some((option) => option.value === current) ? current : "";
  select.title = select.selectedOptions[0]?.text || (id === "model-select" ? "按模型筛选" : "按账号筛选");
  if (id === "model-select") board.model = select.value;
  else board.account = select.value;
}

function renderSelects() {
  const facets = board.data.facets;
  const isSingle = board.agents.size === 1;
  const filterList = (list) =>
    isSingle ? (list || []).filter((row) => board.agents.has(row.provider)) : (list || []);

  fillOptions(
    "model-select",
    filterList(facets.models).map((row) => ({
      value: row.provider + "/" + row.model,
      label: isSingle
        ? `${row.model} (${row.count})`
        : `${row.model} · ${names[row.provider] || row.provider} (${row.count})`,
    })),
  );
  fillOptions(
    "account-select",
    filterList(facets.accounts).map((row) => ({
      value: row.provider + "/" + row.id,
      label: isSingle
        ? `${row.name} (${row.count})`
        : `${row.name} · ${names[row.provider] || row.provider} (${row.count})`,
    })),
  );
}

function renderActive() {
  const tags = [];
  if (board.session)
    tags.push(
      `<span>只看会话</span><code>${escape(
        board.session.slice(-10),
      )}</code><button data-clear="session" title="取消会话筛选">×</button>`,
    );
  if (board.search)
    tags.push(
      `<span>搜索</span><code>${escape(
        board.search,
      )}</code><button data-clear="search" title="清除搜索">×</button>`,
    );
  setHtml($("board-active"), tags.join(""));
  $("board-active").hidden = !tags.length;
}

function statusCell(row) {
  const badge = row.status === "cancelled" ? "failed" : row.status;
  const detail =
    row.status === "success"
      ? ""
      : [(statuses[row.status] || row.status), row.error].filter(Boolean).join(" · ");
  return `<td><span class="badge ${escape(badge)}"${
    detail ? ` title="${escape(detail)}"` : ""
  }>${row.status === "success" ? "成功" : row.status === "processing" ? "处理中" : row.status === "cancelled" ? "已取消" : "失败"}</span></td>`;
}

// Which upstream route served the turn, when the provider has more than one.
//
// The Trae provider can answer from a direct model call or from a full agent
// session. The two carry very different prompts and latencies, so the board
// has to name the one that actually ran.
function routeCell(upstream) {
  if (!upstream) return "—";
  const label = upstream === "trae:direct-model"
    ? t("routeDirect")
    : upstream === "trae:agent-session"
      ? t("routeAgent")
      : upstream;
  return `<span class="badge route-badge" title="${escape(upstream)}">${escape(label)}</span>`;
}

function boardRowHtml(row) {
  const key = escape(
    `${row.timestamp}|${row.provider}|${row.session_id}|${row.model}|${row.account}`,
  );
  return `<tr data-row-key="${key}">
      <td title="${escape(date(row.timestamp))}">${escape(date(row.timestamp))}</td>
      <td class="app-cell" title="${escape(names[row.provider] || row.provider)}"><img src="${escape(logo(row.provider))}" alt=""><span>${escape(
        names[row.provider] || row.provider,
      )}</span></td>
      <td class="model-cell" title="${escape(row.model || '')}">${escape(row.model || "—")}</td>
      <td class="account-cell" title="${escape(row.account_name || row.account || '')}">${escape(
        row.account_name || (row.account || "").slice(0, 10) || "—",
      )}</td>
      ${statusCell(row)}
      <td class="route-cell">${routeCell(row.upstream)}</td>
      <td>${compact(row.new_input_tokens)}</td>
      <td>${compact(row.cached_tokens)}</td>
      <td>${compact(row.output_tokens)}</td>
      <td>${credit(row.credits)}</td>
      <td>${row.tokens_per_sec == null ? "—" : Number(row.tokens_per_sec).toFixed(1)}</td>
      <td>${duration(row.ttft_ms)}</td>
      <td>${duration(row.duration_ms)}</td>
    </tr>`;
}

function renderRows(rows) {
  const total = rows.length;
  const totalPages = Math.max(1, Math.ceil(total / board.pageSize));
  if (board.page > totalPages) board.page = totalPages;
  if (board.page < 1) board.page = 1;

  const start = (board.page - 1) * board.pageSize;
  const pageRows = rows.slice(start, start + board.pageSize);

  const prevBtn = $("page-prev");
  const nextBtn = $("page-next");
  const infoEl = $("pagination-info");
  const indicatorEl = $("page-indicator");

  if (infoEl) infoEl.textContent = `共 ${total} 条记录`;
  if (indicatorEl) indicatorEl.textContent = `第 ${board.page} / ${totalPages} 页`;
  if (prevBtn) prevBtn.disabled = board.page <= 1;
  if (nextBtn) nextBtn.disabled = board.page >= totalPages;

  if (!pageRows.length) {
    $("board-rows").innerHTML =
      `<tr><td colspan="12"><div class="empty">${
        board.data.rows.length
          ? "当前筛选没有匹配的请求"
          : "暂无请求记录，客户端调用网关后将在这里汇总"
      }</div></td></tr>`;
    return;
  }
  patchRows(
    $("board-rows"),
    pageRows,
    (row) =>
      `${row.timestamp}|${row.provider}|${row.session_id}|${row.model}|${row.account}`,
    boardRowHtml,
  );
}

export function renderBoard() {
  const rows = visibleRows();
  renderChips();
  renderSelects();
  renderActive();
  const providers = board.agents.size ? [...board.agents] : PROVIDERS;
  const unit = [...new Set(providers.map(usageUnit))].join(" / ");
  $("board-usage-label").textContent = unit;
  document.querySelector('#chart-toggle [data-metric="credits"]').textContent = unit;
  const data = kpis(rows, unit);
  if ($("kpi-grid")) {
    setHtml(
      $("kpi-grid"),
      data.mainCards
        .map(
          (card) => `
        <div class="kpi-card">
          <div class="kpi-card-header">
            <span>${escape(card.label)}</span>
            ${
              card.hasInfo
                ? `<span class="kpi-info-icon" title="${escape(card.info)}">${icon("info")}</span>`
                : ""
            }
          </div>
          <div class="kpi-card-val">${escape(card.value)}</div>
        </div>
      `,
        )
        .join(""),
    );
  }
  if ($("kpi-sub-grid")) {
    setHtml(
      $("kpi-sub-grid"),
      data.subCards
        .map(
          (card) => `
        <div class="kpi-sub-card">
          <div class="kpi-card-header">
            <span>${escape(card.label)}</span>
          </div>
          <div class="kpi-card-val">${escape(card.value)}</div>
        </div>
      `,
        )
        .join(""),
    );
  }
  renderChart(rows, unit);
  renderRows(rows);
  document
    .querySelectorAll("#chart-toggle button")
    .forEach((button) =>
      button.classList.toggle("selected", button.dataset.metric === board.metric),
    );
  notify();
}

export function selectAgent(name) {
  if (!name || (board.agents.size === 1 && board.agents.has(name))) {
    board.agents.clear();
  } else {
    board.agents = new Set([name]);
  }
  board.model = "";
  board.account = "";
  board.session = "";
  board.page = 1;
  $("model-select").value = "";
  $("account-select").value = "";
  renderBoard();
}

export function clearFilters() {
  board.agents.clear();
  board.model = "";
  board.account = "";
  board.status = "";
  board.session = "";
  board.search = "";
  board.page = 1;
  $("log-filter").value = "";
  $("model-select").value = "";
  $("account-select").value = "";
  $("status-select").value = "";
  renderBoard();
}

export function bindBoard(changed = () => {}) {
  notify = changed;
  $("agent-chips").onclick = (event) => {
    const chip = event.target.closest("[data-agent]");
    if (!chip) return;
    selectAgent(chip.dataset.agent);
  };
  $("model-select").onchange = (event) => {
    board.model = event.target.value;
    event.target.title = event.target.selectedOptions[0]?.text || "按模型筛选";
    if (board.model) {
      const [provider] = board.model.split("/");
      if (provider) board.agents = new Set([provider]);
      board.account = "";
      board.session = "";
      $("account-select").value = "";
    }
    board.page = 1;
    renderBoard();
  };
  $("account-select").onchange = (event) => {
    board.account = event.target.value;
    event.target.title = event.target.selectedOptions[0]?.text || "按账号筛选";
    if (board.account) {
      const [provider] = board.account.split("/");
      if (provider) board.agents = new Set([provider]);
      board.model = "";
      board.session = "";
      $("model-select").value = "";
    }
    board.page = 1;
    renderBoard();
  };
  $("status-select").onchange = (event) => {
    board.status = event.target.value;
    event.target.title = event.target.selectedOptions[0]?.text || "按状态筛选";
    board.page = 1;
    renderBoard();
  };
  $("range-select").onchange = (event) => {
    board.range = event.target.value;
    event.target.title = event.target.selectedOptions[0]?.text || "按时间范围筛选";
    board.page = 1;
    renderBoard();
  };
  $("log-filter").oninput = debounce((event) => {
    board.search = event.target.value;
    board.page = 1;
    renderBoard();
  }, 150);
  $("board-rows").onclick = (event) => {
    const button = event.target.closest("[data-session]");
    if (!button) return;
    board.session = board.session === button.dataset.session ? "" : button.dataset.session;
    if (board.session) {
      board.model = "";
      board.account = "";
      $("model-select").value = "";
      $("account-select").value = "";
    }
    board.page = 1;
    renderBoard();
  };
  $("board-active").onclick = (event) => {
    const clear = event.target.closest("[data-clear]");
    if (clear) {
      if (clear.dataset.clear === "session") board.session = "";
      else if (clear.dataset.clear === "search") {
        board.search = "";
        $("log-filter").value = "";
      }
      board.page = 1;
      renderBoard();
    } else if (event.target.id === "board-clear") {
      clearFilters();
    }
  };
  $("chart-toggle").onclick = (event) => {
    const button = event.target.closest("[data-metric]");
    if (!button) return;
    board.metric = button.dataset.metric;
    renderBoard();
  };
  const expandBtn = $("kpi-expand-btn");
  if (expandBtn) {
    expandBtn.onclick = () => {
      const subGrid = $("kpi-sub-grid");
      if (!subGrid) return;
      const isExpanded = expandBtn.getAttribute("aria-expanded") === "true";
      const next = !isExpanded;
      expandBtn.setAttribute("aria-expanded", String(next));
      subGrid.hidden = !next;
    };
  }
  const prevBtn = $("page-prev");
  if (prevBtn) {
    prevBtn.onclick = () => {
      if (board.page > 1) {
        board.page--;
        renderBoard();
      }
    };
  }
  const nextBtn = $("page-next");
  if (nextBtn) {
    nextBtn.onclick = () => {
      const rows = visibleRows();
      const totalPages = Math.max(1, Math.ceil(rows.length / board.pageSize));
      if (board.page < totalPages) {
        board.page++;
        renderBoard();
      }
    };
  }
}

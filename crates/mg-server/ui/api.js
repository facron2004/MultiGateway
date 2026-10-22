export async function api(path, payload, signal) {
  const headers =
    payload === undefined ? {} : { "Content-Type": "application/json" };
  let response;
  try {
    response = await fetch(path, {
      method: payload === undefined ? "GET" : "POST",
      headers,
      body: payload === undefined ? undefined : JSON.stringify(payload),
      credentials: "same-origin",
      signal,
    });
  } catch (err) {
    // AbortError must stay distinguishable: refresh() relies on it to ignore
    // superseded requests.
    if (err?.name === "AbortError") throw err;
    throw new Error("无法连接网关服务，可能正在重启或已退出");
  }
  const text = await response.text();
  let data;
  try {
    data = text ? JSON.parse(text) : {};
  } catch (_e) {
    data = {
      error: {
        message: text || `HTTP ${response.status} ${response.statusText}`,
      },
    };
  }
  if (!response.ok) {
    throw new Error(
      data.error?.message ||
        data.message ||
        data.error ||
        `请求失败 (${response.status})`,
    );
  }
  if (data.success === false || data.ok === false)
    throw new Error(data.error || data.msg || "操作失败");
  return data;
}

export function providerApi(name, resource, payload, signal) {
  return api(`/gateway/providers/${name}/${resource}`, payload, signal);
}

export function external(url) {
  if (!/^https?:\/\//.test(url)) throw new Error("授权地址不受支持");
  if (window.pywebview?.api) return window.pywebview.api.open_external(url);
  window.open(url, "_blank", "noopener,noreferrer");
}

export async function copy(value) {
  if (navigator.clipboard) return navigator.clipboard.writeText(value);
  const field = document.createElement("textarea");
  field.value = value;
  document.body.append(field);
  field.select();
  const copied = document.execCommand("copy");
  field.remove();
  if (!copied) throw new Error("请手动复制地址");
}

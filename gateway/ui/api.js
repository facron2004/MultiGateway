let desktopToken = new URLSearchParams(location.hash.slice(1)).get("token");
if (desktopToken) history.replaceState(null, "", location.pathname);

export async function api(path, payload, signal) {
  const headers =
    payload === undefined ? {} : { "Content-Type": "application/json" };
  if (desktopToken) headers["X-Panel-Token"] = desktopToken;
  const response = await fetch(path, {
    method: payload === undefined ? "GET" : "POST",
    headers,
    body: payload === undefined ? undefined : JSON.stringify(payload),
    credentials: "same-origin",
    signal,
  });
  const data = await response.json();
  if (response.ok) desktopToken = null;
  if (!response.ok) {
    if (
      response.status === 401 &&
      !["/panel/login", "/panel/password"].includes(path)
    )
      window.dispatchEvent(new Event("gateway-login-required"));
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

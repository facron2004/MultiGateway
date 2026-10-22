const dictionaries = {
  "zh-CN": {
    documentTitle: "MultiGateway · 账号网关",
    viewAccounts: "账号与模型",
    viewLogs: "用量统计",
    viewSettings: "设置",
    gatewaySettingsNav: "网关设置",
    back: "返回",
    backToAccounts: "返回账号列表",
    general: "通用",
    generalSettings: "通用设置",
    gateway: "网关",
    gatewaySettings: "网关连接设置",
    apiKey: "API Key",
    apiKeySettings: "API Key 设置",
    data: "数据",
    dataSettings: "数据设置",
    settingsNav: "设置分类",
    appearance: "外观",
    themeMode: "明暗",
    followSystem: "跟随系统",
    themeLight: "亮色",
    themeDark: "暗色",
    systemLanguage: "系统语言",
    themeColor: "主题色",
    accentOrange: "橙色",
    accentTeal: "青绿",
    accentBlue: "蓝色",
    accentViolet: "紫色",
    accentRose: "玫红",
    accentGreen: "绿色",
    customAccent: "自定义主题色",
    appearanceHint: "主题、语言和主题色会立即生效，并自动保存。",
    appearanceSaving: "正在保存…",
    appearanceSaved: "已保存。",
    appearanceFailed: "保存失败，已恢复原设置。",
    windowAndTerminal: "窗口与终端",
    launchAtLogin: "开机自启",
    startSilently: "静默启动",
    startSilentlyHelp: "启动后网关正常运行，窗口不弹出，可通过托盘图标打开。",
    closeToTray: "关闭时最小化到托盘",
    closeToTrayHelp: "关闭窗口后保持网关运行，选择托盘菜单中的“退出网关”可完全退出。",
    openWindow: "打开窗口",
    openWindowValue: "单击托盘图标",
    quitGateway: "退出网关",
    quitGatewayValue: "托盘右键菜单 → 退出网关",
    desktopHint: "开关修改后自动保存，静默启动将在下次启动时生效。",
    desktopSaving: "正在保存…",
    desktopSaved: "已保存。静默启动将在下次启动时生效。",
    desktopFailed: "保存失败，已恢复原设置。",
    unifiedGateway: "统一出口网关",
    listenHost: "监听地址",
    listenPort: "监听端口",
    listenReadonly: "只读，修改需重启网关后生效",
    listenNote: "监听地址与端口为只读展示，修改需重启网关后生效。",
    readingPort: "正在读取当前网关端口…",
    readingAddress: "正在读取地址…",
    endpoints: "接口地址",
    endpointBody: "所有 Agent 分组共用统一入口与访问密钥。",
    copyEndpoint: "复制统一接口地址",
    endpointCopied: "统一接口地址已复制",
    accessKeys: "访问密钥",
    keysDescription: "外部 Agent 调用网关时使用的 API Key，保存在当前数据目录中。",
    noKeys: "暂无 API Key，请在下方输入名称并生成。",
    newKey: "新增 API Key",
    keyPlaceholder: "输入 Key 名称，如：Cursor、Claude Code",
    generateKey: "生成 API Key",
    copy: "复制",
    copied: "已复制",
    confirmDelete: "确定删除?",
    storage: "存储位置",
    dataDirLabel: "MultiGateway 数据目录",
    dataDirHelp: "当前实例实际使用的目录，包含账号、API Key、用量记录和桌面偏好。",
    openFolder: "打开数据目录",
    openFolderDesktop: "在桌面版中打开文件夹",
    copyFolder: "复制数据目录",
    dataDirHint: "这里显示当前实例正在使用的存储位置。",
    dataDirCopied: "数据目录已复制",
    version: "网关版本",
    versionLoading: "读取中…",
    versionUnavailable: "无法读取",
    keyPersistence: "密钥持久化",
    keyFile: "密钥文件",
    keyBackup: "上次修改前的备份",
    keyBackupHelp: "每次修改 Key 时，自动保留修改前的文件。",
    keyBackupPending: "首次修改密钥后生成",
    keyNote: "API Key 使用原子写入保存，已有密钥会在重启后继续保留。",
  },
  en: {
    documentTitle: "MultiGateway · Account gateway",
    viewAccounts: "Accounts and models",
    viewLogs: "Usage",
    viewSettings: "Settings",
    gatewaySettingsNav: "Gateway settings",
    back: "Back",
    backToAccounts: "Back to accounts",
    general: "General",
    generalSettings: "General settings",
    gateway: "Gateway",
    gatewaySettings: "Gateway connection",
    apiKey: "API Key",
    apiKeySettings: "API key settings",
    data: "Data",
    dataSettings: "Data settings",
    settingsNav: "Settings",
    appearance: "Appearance",
    themeMode: "Theme",
    followSystem: "Follow system",
    themeLight: "Light",
    themeDark: "Dark",
    systemLanguage: "Language",
    themeColor: "Accent",
    accentOrange: "Orange",
    accentTeal: "Teal",
    accentBlue: "Blue",
    accentViolet: "Violet",
    accentRose: "Rose",
    accentGreen: "Green",
    customAccent: "Custom accent",
    appearanceHint: "Theme, language, and accent apply immediately and save automatically.",
    appearanceSaving: "Saving…",
    appearanceSaved: "Saved.",
    appearanceFailed: "Could not save. The previous settings were restored.",
    windowAndTerminal: "Window and terminal",
    launchAtLogin: "Launch at login",
    startSilently: "Start silently",
    startSilentlyHelp: "The gateway starts without opening a window. Open it from the tray icon.",
    closeToTray: "Minimize to tray on close",
    closeToTrayHelp: "Closing the window keeps the gateway running. Choose Quit gateway in the tray menu to exit.",
    openWindow: "Open window",
    openWindowValue: "Click the tray icon",
    quitGateway: "Quit gateway",
    quitGatewayValue: "Tray menu → Quit gateway",
    desktopHint: "Switches save automatically. Silent start applies the next time the app launches.",
    desktopSaving: "Saving…",
    desktopSaved: "Saved. Silent start applies on the next launch.",
    desktopFailed: "Could not save. The previous settings were restored.",
    unifiedGateway: "Unified gateway",
    listenHost: "Listen address",
    listenPort: "Listen port",
    listenReadonly: "Read-only. Restart the gateway to change this.",
    listenNote: "Address and port are read-only. Restart the gateway to change them.",
    readingPort: "Reading the gateway port…",
    readingAddress: "Reading the address…",
    endpoints: "Endpoint",
    endpointBody: "Every agent shares one endpoint and the same access keys.",
    copyEndpoint: "Copy the unified endpoint",
    endpointCopied: "Unified endpoint copied",
    accessKeys: "Access keys",
    keysDescription: "API keys for external agents. They are stored in the current data directory.",
    noKeys: "No API keys yet. Enter a name below to generate one.",
    newKey: "New API key",
    keyPlaceholder: "Key name, for example Cursor or Claude Code",
    generateKey: "Generate API key",
    copy: "Copy",
    copied: "Copied",
    confirmDelete: "Delete?",
    storage: "Storage",
    dataDirLabel: "MultiGateway data directory",
    dataDirHelp: "The directory this instance uses for accounts, API keys, usage, and desktop preferences.",
    openFolder: "Open data folder",
    openFolderDesktop: "Open the folder in the desktop app",
    copyFolder: "Copy data directory",
    dataDirHint: "This is the storage location the current instance is using.",
    dataDirCopied: "Data directory copied",
    version: "Gateway version",
    versionLoading: "Loading…",
    versionUnavailable: "Unavailable",
    keyPersistence: "Key storage",
    keyFile: "Key file",
    keyBackup: "Backup before the last change",
    keyBackupHelp: "Each key change keeps a copy of the previous file.",
    keyBackupPending: "Created after the first key change",
    keyNote: "API keys are saved atomically and remain available after a restart.",
  },
};

let current = "zh-CN";

export function locale() {
  return current;
}

export function t(key) {
  return dictionaries[current]?.[key] ?? dictionaries["zh-CN"][key] ?? key;
}

export function formatKeyCount(count) {
  const total = Number(count) || 0;
  if (current === "en") return total === 1 ? "1 key" : `${total} keys`;
  return `${total} 个 Key`;
}

export function applyI18n(root = document) {
  root.querySelectorAll("[data-i18n]").forEach((element) => {
    element.textContent = t(element.dataset.i18n);
  });
  root.querySelectorAll("[data-i18n-title]").forEach((element) => {
    element.title = t(element.dataset.i18nTitle);
  });
  root.querySelectorAll("[data-i18n-aria]").forEach((element) => {
    element.setAttribute("aria-label", t(element.dataset.i18nAria));
  });
  root.querySelectorAll("[data-i18n-placeholder]").forEach((element) => {
    element.placeholder = t(element.dataset.i18nPlaceholder);
  });
}

export function setLocale(next) {
  const resolved = next === "en" ? "en" : "zh-CN";
  const changed = resolved !== current;
  current = resolved;
  document.documentElement.lang = current === "en" ? "en" : "zh-CN";
  document.documentElement.dataset.lang = current;
  document.title = t("documentTitle");
  if (!changed) return;
  applyI18n();
  window.dispatchEvent(new CustomEvent("mg:locale"));
}

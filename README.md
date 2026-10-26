<p align="center">
  <img src="crates/mg-server/ui/icons/multigateway.png" width="112" height="112" alt="MultiGateway 图标" />
</p>

<h1 align="center">MultiGateway</h1>

<p align="center">
  一个网关，统一管理多个 AI 服务的账号池。
</p>

<p align="center">
  <a href="#环境要求"><img src="https://img.shields.io/badge/Rust-1.90%2B-000000?logo=rust&amp;logoColor=white" alt="Rust 1.90+" /></a>
  <a href="Cargo.toml"><img src="https://img.shields.io/badge/Version-0.11.2-2563EB" alt="版本 0.11.2" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-22C55E" alt="MIT 协议" /></a>
  <a href="#构建与运行"><img src="https://img.shields.io/badge/Desktop-Windows%20%7C%20macOS-0078D4" alt="Windows 和 macOS 桌面版" /></a>
</p>

<p align="center">
  <strong>简体中文</strong> · <a href="README.en.md">English</a>
</p>

<p align="center">
  <a href="#功能特性">功能特性</a> ·
  <a href="#支持的服务">支持的服务</a> ·
  <a href="#环境要求">环境要求</a> ·
  <a href="#构建与运行">构建与运行</a> ·
  <a href="#接口">接口</a> ·
  <a href="#项目结构">项目结构</a> ·
  <a href="#运行测试">运行测试</a> ·
  <a href="CHANGELOG.md">更新说明</a> ·
  <a href="#开源协议">开源协议</a>
</p>

MultiGateway 是一个用 Rust 编写的多账号反向代理网关。它把 **WorkBuddy**、**Trae**、**MiniMax**、**Qoder**、**Antigravity**、**Grok** 和 **GitHub Copilot** 收进同一个桌面面板，并用 OpenAI、Anthropic 和 Responses 兼容接口对外提供模型。

当前版本 **0.11.2**。账号池、协议转换和各厂商适配都在本仓库的 Rust 代码里，Tauri 2 桌面窗口、托盘和 Web 面板一起编进 Windows EXE 或 macOS 应用。

---

## 功能特性

- **多服务账号池**：七个服务共用一套启用、轮转、会话粘性和冷却逻辑。上游按账号拒绝时，该账号会短暂退出自动选择。
- **国内版 / 国际版**：WorkBuddy、Trae、MiniMax 和 Qoder 可以按区域切换。模型目录、额度查询和签到跟当前区域走，不会把国际版凭证发到国内接口。
- **模型目录**：展示上游返回的模型名、上下文和消耗积分系数，并列出每个模型的识图、工具、最大输出和思考档位。上游没给系数时，使用该服务已确认的官方快照补齐，并标出高峰 / 低谷窗口。
- **签到**：WorkBuddy 国内个人账号、Trae，以及开放了每日签到接口的 Qoder 区域，可以在账号表里签到。
- **协议兼容**：`/v1/chat/completions`、`/v1/models`、`/v1/responses` 和 `/v1/messages`。流式响应逐块转发，工具调用和推理内容会保留。
- **用量看板**：按服务、模型和账号查看请求、Token 和积分 / 额度。
- **桌面与托盘**：WebView2 窗口，支持开机启动、静默启动，以及关闭时留在托盘；左键单击托盘图标打开主窗口，右键打开快捷面板，查看今日用量、账号就绪情况和最近请求。也可以只跑 HTTP 服务。

---

## 支持的服务

| 服务 | 区域 | 签到 | 说明 |
| --- | --- | --- | --- |
| **WorkBuddy** | 国内版 / 国际版 | 国内个人账号 | 额度查询、账号导入与凭证刷新 |
| **Trae** | 国内版 / 国际版 | 支持 | 账号导入；国际版模型目录、积分和签到使用国际区接口 |
| **MiniMax** | 国内版 / 国际版 | 不支持 | 账号导入与凭证刷新；Token Plan 额度需要 Subscription Key |
| **Qoder** | 国内版 / 国际版 | 区域开放时支持 | 账号导入；模型目录带积分系数；凭证可刷新 |
| **Antigravity** | 单一区域 | 不支持 | 账号导入与凭证刷新 |
| **Grok** | 单一区域 | 不支持 | 账号导入、额度查询与凭证刷新 |
| **GitHub Copilot** | 单一区域 | 不支持 | GitHub 设备码授权，再换取短期 Copilot 令牌 |

模型请求使用带服务前缀的 ID，例如 `trae/glm-5.3`、`qoder/Qwen3.8-Max`、`copilot/gpt-4.1`。分组端点 `/trae/v1/models` 保留上游原始模型 ID。

### Trae 上游通道

Trae 有两条上游通道，面板里可以直接切换（Trae 页顶部工具栏的「直连 Solo / Agent 会话」），切换后立即生效，不需要重启网关：

| 取值 | 上游接口 | 特点 |
| --- | --- | --- |
| `solo`（默认） | `/api/agent/v3/llm_utils_chat` | 直连模型，不走 Agent 层，首字通常 1–1.5 秒，输入 Token 只包含真实对话量 |
| `remote` | `/api/remote/v1/chat_sessions` | 完整 Trae Agent：自带约 2.4 万 Token 的 Agent 提示、内置技能和沙箱，首字通常 3–8 秒 |

`remote` 也接受 `agent`、`session`、`web` 别名。直连通道建流失败时会自动回退到 `remote`，请求不会直接失败。
需要 Trae 的技能和沙箱能力时再切回 `remote`。

回退是静默的，但记录不是：每条请求都会带上这一轮实际走的通道（`trae:direct-model` 或 `trae:agent-session`），
在 `requests.jsonl` 的 `upstream` 字段和用量看板的「通道」列里都能看到。两条通道的输入 Token 差一个数量级
（直连只有真实对话量，Agent 会话自带约 2.4 万 Token 的 Agent 提示），对不上时先看这一列。

通道选择的优先级是：**面板里选的 > 环境变量 `TRAE_UPSTREAM_MODE` > 默认 `solo`**。
面板的选择保存在数据目录的 `provider_settings.json`，重启后仍然生效；删掉这个文件就回到环境变量或默认值。

上游对每个账号只允许 **2 个并发 Agent 会话**（`solo_agent_parallel_limit`）。直连通道和 Agent 会话通道
共用这份预算，所以网关按账号本地限流：超出的请求在本地排队等待，而不是被上游拒绝或在上游深处排队
（实测会被拖到 50 秒以上）。并发上限可用 `TRAE_WEB_PARALLEL_LIMIT` 调整（默认 `2`）。
这类并发冲突属于流量问题，不会被当成账号故障而冷却账号。

两条通道都会把上游的提示词缓存命中（`cache_read_input_tokens`）透传给客户端，调用方可以在响应的
`usage.prompt_tokens_details.cached_tokens` 以及面板的用量看板里看到缓存命中情况。

---

## 环境要求

- Windows 10/11 x64、ARM64，或 macOS 11+ Apple Silicon、Intel
- 从源码构建需要 Rust **1.90+**；Windows 需要 MSVC C++ 生成工具，macOS 需要 Xcode 命令行工具
- Tauri 2 使用系统 WebView2（Windows）或 WKWebView（macOS），无需捆绑浏览器运行时

---

## 构建与运行

```bash
git clone https://github.com/facron2004/MultiGateway.git
cd MultiGateway
cargo run -p mg-server -- --host 127.0.0.1 --port 8791
```

发布版：

```bash
cargo build --locked --release -p mg-server --bin MultiGateway
```

Windows 也可以使用 `run_multigateway.bat` 启动，或用 `build_multigateway.bat` 构建便携版。

构建由 `tauri-build` 生成桌面资源，直接使用 Cargo 即可产出 Tauri EXE。桌面面板由 Tauri 内嵌资源加载，账号、设置和用量管理通过 Tauri IPC 直接调用 Rust 业务函数。HTTP 网关负责向外部客户端提供模型 API，也提供浏览器面板；两种面板共用一份资源和业务状态，无需 Node.js 构建工具或额外的后端进程。Windows 上 Tauri 使用系统 WebView2 渲染界面。

生成的程序在 `target/release/MultiGateway.exe`。把它放到一个可写目录再运行。账号、API Key 和用量默认写在程序旁的 `gateway-data/`。已有数据目录可以用参数指过去：

```cmd
MultiGateway.exe --host 127.0.0.1 --port 8791 --data-dir "D:\MultiGateway-data"
```

`--silent` 启动服务但不弹出窗口。`--headless` 只运行 HTTP 服务。托盘面板里的「退出网关」会结束程序。

面板地址是 `http://127.0.0.1:8791/`。API Key 保存在数据目录的 `api_keys.json`，修改前会留下一份 `api_keys.json.bak`。

升级时先退出程序，再替换 exe，并保留原来的 `gateway-data/`。
桌面浏览器数据保存在所选数据目录的 `webview2/` 中。

macOS 上运行以下命令，分别生成 `dist/MultiGateway-0.11.2-macos-arm64.dmg` 和 `dist/MultiGateway-0.11.2-macos-x64.dmg`：

```bash
bash scripts/build-macos-dmg.sh
```

也可以在对应架构的 Mac 上单独构建：`bash scripts/build-macos-dmg.sh aarch64-apple-darwin` 或 `bash scripts/build-macos-dmg.sh x86_64-apple-darwin`。将 DMG 中的 `MultiGateway.app` 拖到「应用程序」后打开；应用使用本机临时签名，尚未进行 Apple 公证。macOS 账号和配置默认保存在 `~/Library/Application Support/MultiGateway`，应用旁已有 `gateway-data/` 时优先使用该目录。升级时保留数据目录。

GitHub Actions 分别在 Windows x64、ARM64 和 macOS Apple Silicon、Intel 上运行 Rust 测试、构建并检查发布包；macOS 还会挂载 DMG，验证签名并启动包内程序。

---

## 接口

| 用途 | 地址 |
| --- | --- |
| Web 面板 | `http://127.0.0.1:8791/` |
| OpenAI Chat | `http://127.0.0.1:8791/v1/chat/completions` |
| OpenAI Responses | `http://127.0.0.1:8791/v1/responses` |
| Anthropic Messages | `http://127.0.0.1:8791/v1/messages` |
| 模型列表 | `http://127.0.0.1:8791/v1/models` |

管理接口（`/gateway/*`、`/settings*` 和面板静态资源）只接受本机回环地址的请求。即使使用
`--host 0.0.0.0` 向局域网提供推理接口，账号管理、API Key 和设置页也不会暴露到局域网。
未配置任何 API Key 时，局域网推理请求会被拒绝；本机 `127.0.0.1` 仍可直接使用。来自非本机网页
Origin 的浏览器请求同样会被拒绝，网页调用前请先在面板中创建 API Key。对外提供推理服务前请先创建 API Key。

统一的 `/v1/models` 只返回带服务前缀的模型 ID。模型条目会带上上游实际提供的能力，所有服务使用同一套字段：

- **识图**：`supports_vision`、`supports_images`、`vision`、`multimodal`、`abilities.vision`
- **输入 / 输出**：`input_modalities`、`output_modalities`、`modalities`、`architecture`
- **长度**：`context_length`、`max_input_tokens`、`max_output_tokens`、`max_completion_tokens`
- **工具**：`supports_tool_calls`、`abilities.functionCall`
- **思考强度**：`reasoning_efforts`、`reasoning_default_effort`、`reasoning_can_disable`，
  以及 `thinking` 块（`supported`、`can_disable`、`default_effort`、`efforts`、`levels`）

思考档位统一成 `minimal` / `low` / `medium` / `high` / `xhigh` / `max` 六级，各服务自己的叫法会先归一
（例如 Trae 的 `light` → `low`，`extra_high` → `xhigh`）。`thinking.levels` 里多出的 `none` 表示
这个模型的思考可以关闭；`reasoning_default_effort` 优先取上游声明的默认档，其次取模型 ID 里带的具体档位
（如 `gemini-3.8-flash-low` → `low`），最后才是该服务的常规默认档。

上游没有给出的能力是 `null`，不会按模型名猜测；唯一例外是该服务**确定**支持的能力
（如 Antigravity / Grok / MiniMax 的识图），会作为该服务的默认值补上，但永远不会覆盖上游明确给出的值。
能力开关（`supports_*`、`reasoning_can_disable`、`thinking`、两个 modality 列表）在**每个**模型条目上都会出现，
客户端不必区分是哪个服务返回的。

请求侧同样对齐：`reasoning_effort` 会转成上游自己的思考档位（Gemini 的 `thinkingBudget`、Trae 的
`reasoning_effort`、Antigravity 的 `thinkingConfig`），并且会保证输出上限始终大于思考预算，避免上游 400。

---

## 项目结构

```text
crates/
  mg-core/        账号池、API Key、用量记录和配置
  mg-protocols/   OpenAI、Anthropic、Gemini、Responses 转换
  mg-providers/   七个服务的目录、对话、额度和签到
  mg-server/      HTTP 服务、桌面窗口和内嵌面板
```

---

## 运行测试

```bash
cargo test --workspace
```

测试使用本地夹具，不需要真实账号，也不会访问上游。

---

## 开源协议

本项目使用 [MIT 协议](LICENSE)。
服务图标的来源记在 [`crates/mg-server/ui/icons/SOURCES.json`](crates/mg-server/ui/icons/SOURCES.json)。
第三方许可文本保留在 [`licenses/`](licenses/) 中。

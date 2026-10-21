<p align="center">
  <img src="crates/mg-server/ui/icons/multigateway.png" width="112" height="112" alt="MultiGateway 图标" />
</p>

<h1 align="center">MultiGateway</h1>

<p align="center">
  一个网关，统一管理多个 AI 服务的账号池。
</p>

<p align="center">
  <a href="#环境要求"><img src="https://img.shields.io/badge/Rust-1.90%2B-000000?logo=rust&amp;logoColor=white" alt="Rust 1.90+" /></a>
  <a href="Cargo.toml"><img src="https://img.shields.io/badge/Version-0.11.1-2563EB" alt="版本 0.11.1" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-22C55E" alt="MIT 协议" /></a>
  <a href="#构建与运行"><img src="https://img.shields.io/badge/Desktop-Windows-0078D4?logo=windows&amp;logoColor=white" alt="Windows 桌面版" /></a>
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
  <a href="#开源协议">开源协议</a>
</p>

MultiGateway 是一个用 Rust 编写的多账号反向代理网关。它把 **WorkBuddy**、**Trae**、**MiniMax**、**Qoder**、**Antigravity**、**Grok** 和 **GitHub Copilot** 收进同一个桌面面板，并用 OpenAI、Anthropic 和 Responses 兼容接口对外提供模型。

当前版本 **0.11.1**。账号池、协议转换和各厂商适配都在本仓库的 Rust 代码里，桌面窗口、托盘和 Web 面板一起编进 `MultiGateway.exe`。

---

## 功能特性

- **多服务账号池**：七个服务共用一套启用、轮转、会话粘性和冷却逻辑。上游按账号拒绝时，该账号会短暂退出自动选择。
- **国内版 / 国际版**：WorkBuddy、Trae、MiniMax 和 Qoder 可以按区域切换。模型目录、额度查询和签到跟当前区域走，不会把国际版凭证发到国内接口。
- **模型目录**：展示上游返回的模型名、上下文和消耗积分系数。上游没给系数时，使用该服务已确认的官方快照补齐，并标出高峰 / 低谷窗口。
- **签到**：WorkBuddy 国内个人账号、Trae，以及开放了每日签到接口的 Qoder 区域，可以在账号表里签到。
- **协议兼容**：`/v1/chat/completions`、`/v1/models`、`/v1/responses` 和 `/v1/messages`。流式响应逐块转发，工具调用和推理内容会保留。
- **用量看板**：按服务、模型和账号查看请求、Token 和积分 / 额度。
- **桌面与托盘**：WebView2 窗口，支持开机启动、静默启动，以及关闭时留在托盘。也可以只跑 HTTP 服务。

---

## 支持的服务

| 服务 | 区域 | 签到 | 说明 |
| --- | --- | --- | --- |
| **WorkBuddy** | 国内版 / 国际版 | 国内个人账号 | 额度查询与凭证刷新 |
| **Trae** | 国内版 / 国际版 | 支持 | 国际版模型目录、积分和签到使用国际区接口 |
| **MiniMax** | 国内版 / 国际版 | 不支持 | 凭证刷新；Token Plan 额度需要 Subscription Key |
| **Qoder** | 国内版 / 国际版 | 区域开放时支持 | 模型目录带积分系数；凭证可刷新 |
| **Antigravity** | 单一区域 | 不支持 | 账号导入与凭证刷新 |
| **Grok** | 单一区域 | 不支持 | 账号导入、额度查询与凭证刷新 |
| **GitHub Copilot** | 单一区域 | 不支持 | GitHub 设备码授权，再换取短期 Copilot 令牌 |

模型请求使用带服务前缀的 ID，例如 `trae/glm-5.3`、`qoder/Qwen3.8-Max`、`copilot/gpt-4.1`。分组端点 `/trae/v1/models` 保留上游原始模型 ID。

---

## 环境要求

- Windows 10/11 x64
- 从源码构建需要 Rust **1.90+** 和 MSVC C++ 生成工具
- 桌面窗口使用系统里的 WebView2

---

## 构建与运行

```bash
git clone https://github.com/facron2004/MultiGateway.git
cd MultiGateway
cargo run -p mg-server -- --host 127.0.0.1 --port 8791
```

发布版：

```bash
cargo build --release -p mg-server
```

生成的程序在 `target/release/MultiGateway.exe`。把它放到一个可写目录再运行。账号、API Key 和用量默认写在程序旁的 `gateway-data/`。已有数据目录可以用参数指过去：

```cmd
MultiGateway.exe --host 127.0.0.1 --port 8791 --data-dir "D:\MultiGateway-data"
```

`--silent` 启动服务但不弹出窗口。`--headless` 只运行 HTTP 服务。托盘里的「退出网关」会结束程序。

面板地址是 `http://127.0.0.1:8791/`。API Key 保存在数据目录的 `api_keys.json`，修改前会留下一份 `api_keys.json.bak`。

升级时先退出程序，再替换 exe，并保留原来的 `gateway-data/`。

---

## 接口

| 用途 | 地址 |
| --- | --- |
| Web 面板 | `http://127.0.0.1:8791/` |
| OpenAI Chat | `http://127.0.0.1:8791/v1/chat/completions` |
| OpenAI Responses | `http://127.0.0.1:8791/v1/responses` |
| Anthropic Messages | `http://127.0.0.1:8791/v1/messages` |
| 模型列表 | `http://127.0.0.1:8791/v1/models` |

统一的 `/v1/models` 只返回带服务前缀的模型 ID。模型条目会带上上游实际提供的能力：

- `supports_reasoning`、`capabilities.reasoning`：是否支持推理
- `reasoning_efforts`、`reasoning_default_effort`：推理强度和默认值
- `supports_vision`、`input_modalities`：是否接受图片

上游没有给出的能力是 `null`，不会按模型名猜测。

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

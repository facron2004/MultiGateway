<p align="center">
  <img src="gateway/ui/icons/multigateway.png" width="112" height="112" alt="MultiGateway 图标" />
</p>

<h1 align="center">MultiGateway</h1>

<p align="center">
  一个网关，统一管理多个 AI 服务的账号池。
</p>

<p align="center">
  <a href="#环境要求"><img src="https://img.shields.io/badge/Python-3.11%2B-3776AB?logo=python&amp;logoColor=white" alt="Python 3.11+" /></a>
  <a href="gateway/__init__.py"><img src="https://img.shields.io/badge/Version-0.10.0-2563EB" alt="版本 0.10.0" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-22C55E" alt="MIT 协议" /></a>
  <a href="#下载使用"><img src="https://img.shields.io/badge/Desktop-Windows-0078D4" alt="Windows 桌面版" /></a>
</p>

<p align="center">
  <strong>简体中文</strong> · <a href="README.en.md">English</a>
</p>

<p align="center">
  <a href="#功能特性">功能特性</a> ·
  <a href="#支持的服务">支持的服务</a> ·
  <a href="#下载使用">下载使用</a> ·
  <a href="#快速开始">快速开始</a> ·
  <a href="#项目结构">项目结构</a> ·
  <a href="#运行测试">运行测试</a> ·
  <a href="#开源协议">开源协议</a>
</p>

MultiGateway 是一个多账号反向代理网关，支持 **WorkBuddy**、**Trae**、
**MiniMax Code**、**Qoder**、**Antigravity**、**Grok** 和 **GitHub Copilot**，
通过统一接口调用不同服务，并在同一面板中管理账号与用量。

当前版本 **0.10.0**：完善 GitHub Copilot、Grok、Antigravity 的真实额度查询，
支持自动 / 手动刷新、分项额度与重置时间展示。
仅支持单一区域的服务会自动隐藏国内版 / 国际版切换。

---

## 功能特性

- **多服务账号池**：统一接入七个服务，支持多账号管理、会话粘性和负载均衡。
- **OAuth 授权管理**：支持设备码授权、本地回调授权、凭据导入、令牌自动刷新，以及账号健康状态和区域管理；具体能力因服务而异。
- **协议兼容**：提供 OpenAI 风格的 `/v1/chat/completions`、`/v1/models`、`/v1/responses` 和 Anthropic 风格的 `/v1/messages`，为使用 Gemini 或 Responses 协议的服务转换请求与 SSE 流。
- **推理控制**：支持推理强度设置、自定义推理档位和推理 Token 用量跟踪。
- **桌面与无界面模式**：可使用内嵌 WebView2 的无边框桌面窗口，也可作为后台 HTTP 服务运行。
- **统一用量看板**：按服务、模型和账号查看请求、Token、积分 / 额度及费用估算。
- **本地客户端配置**：内置 Codex CLI、OpenCode、DSH、Crush、ZCode、Claude Code 等客户端的接入配置工具。

---

## 支持的服务

| 服务 | 集成方式 | 登录 / 授权 | 要求与说明 |
| --- | --- | --- | --- |
| **WorkBuddy** | 内嵌 WorkBuddy 后端 | 浏览器登录，由本地后端轮询授权结果 | 支持国内版 / 国际版 |
| **Trae** | 内嵌 Trae 后端 | 浏览器登录，转交本地回调助手（端口 8765） | 仅国内版，无区域切换 |
| **MiniMax Code** | 基于 MIT 客户端协议的适配器 | OAuth 设备码 + PKCE | 支持国内版 / 国际版，不支持账号导入 |
| **Qoder** | 内嵌 Qoder 后端 | 由内嵌后端处理浏览器登录 | 支持国内版 / 国际版 |
| **Antigravity** | 原生 Google Antigravity OAuth | Google 授权码 + 本地回调 | 需配置本地 OAuth 客户端，Google 账号需具备 Cloud Code Assist 权限；额度与账号档位有关，上游接口可能变化 |
| **Grok** | 原生 xAI Grok CLI / Grok Build OAuth | 授权码 + PKCE + 本地回调 | OAuth 令牌使用 Responses API；免费账号可能因权限限制返回 402 / 403 |
| **GitHub Copilot** | 原生 Copilot OAuth | GitHub 设备码授权，再换取短期 Copilot 令牌 | 需要有效的 Copilot 席位；请求头指纹和约 20 分钟一次的令牌刷新由适配器处理 |

Trae、Antigravity、Grok 和 GitHub Copilot 为单区域服务；
WorkBuddy、MiniMax Code 和 Qoder 保留国内版 / 国际版切换。

使用说明：

- **Antigravity**：账号需具备 Code Assist 使用权限。不同模型的额度独立，上游端点来自实际客户端行为，可能随上游更新而变化。
- **Grok**：网关会将 Chat Completions 和 Anthropic Messages 请求转换为 Responses 请求。普通或免费账号的可用模型、传输方式受上游权限限制，网关会保留实际权限错误。
- **GitHub Copilot**：需要有效席位。请使用自己的账号和额度，并遵守上游对自动化、共享使用及并发的限制。

三个原生 OAuth 集成的授权数据保存在网关数据目录下的
`native/<provider>/accounts/` 中。

---

## 环境要求

- 源码运行需要 Python **3.11+**，推荐 **3.13**；Windows 构建脚本使用 Python 3.13。
- Python 依赖见 [`requirements-gateway.txt`](requirements-gateway.txt)。
- Windows x64 便携版无需安装 Python；桌面模式使用 WebView2。

---

## 下载使用

前往 [GitHub Releases](https://github.com/facron2004/MultiGateway/releases/latest)，
下载 `MultiGateway-v0.10.0-windows-x64.zip`，完整解压到有写入权限的目录，
双击 `MultiGateway.exe` 即可启动。

程序会在可执行文件旁的 `gateway-data/` 中保存账号、配置和用量数据。
升级时请先关闭程序，再替换可执行文件，并保留 `gateway-data/` 和本地 `.env`。
迁移目录时也需要一起迁移数据，或通过 `--data-dir` 指定原数据目录。
发布包不包含预置账号、令牌或个人配置。

---

## 快速开始

### 1. 安装依赖

```bash
git clone https://github.com/facron2004/MultiGateway.git
cd MultiGateway
python -m pip install -r requirements-gateway.txt
```

### 2. 无界面模式

```bash
python gateway_app.py --headless --host 127.0.0.1 --port 8791
```

在浏览器打开 `http://127.0.0.1:8791/`。
启动后控制台会输出带登录令牌的面板地址。
便携版支持相同的命令行参数，例如：

```cmd
MultiGateway.exe --headless --host 127.0.0.1 --port 8791 --data-dir "D:\MultiGateway-data"
```

便携版无界面模式的面板地址会写入数据目录下的 `gateway.log`。

使用 **Antigravity OAuth** 前，请将 [`.env.example`](.env.example) 复制为启动脚本
或可执行文件旁的 `.env`，填入 Antigravity 安装所使用的 OAuth 客户端 ID 和密钥。
同名环境变量优先于 `.env` 中的值；其他服务不需要这项配置。
`.env` 已被 Git 忽略，请保留在本地。

### 3. 桌面模式

```bash
python gateway_app.py
```

### 4. 构建 Windows 可执行文件

```cmd
build_gateway.bat
```

脚本会通过 PyInstaller 构建并在项目根目录生成 `MultiGateway.exe`。

---

## 项目结构

```text
gateway_app.py          桌面与无界面模式入口
gateway/               网关路由、管理功能与服务适配器
  native/              原生 OAuth 服务集成
  ui/                  公共面板资源
  vendor/              内嵌 WorkBuddy、Trae 和 Qoder 运行时
  tests/               本地协议与 HTTP 集成测试
scripts/               维护工具（fetch_pricing.py）
pricing/               内置价格快照
docs/                  架构说明与协议研究
```

模块职责、内嵌集成与运行数据的边界，见 [项目结构说明](docs/architecture.md)。

---

## 运行测试

```bash
python -m unittest discover -s gateway/tests
```

原生服务测试位于 `gateway/tests/test_native_<provider>.py`，
使用本地模拟的服务协议，无需真实账号或访问上游网络。

---

## 开源协议

本项目使用 [MIT 协议](LICENSE)。
内嵌集成及其许可证见 [`gateway/SOURCES.md`](gateway/SOURCES.md)。
0.5.0 新增的三个 OAuth 集成为独立实现，行为参考项目也在该文件中列出。

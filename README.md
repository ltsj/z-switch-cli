# z-switch-cli

> 纯 Rust 原生实现 · 开源无广告的 Claude Code / Codex / Grok 供应商一键管理与秒级热切换命令行工具

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2021-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/ltsj/z-switch-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/ltsj/z-switch-cli/actions/workflows/ci.yml)

---

## 🌟 核心特性

- **⚡ 纯 Rust 极速原生**：单二进制独立运行，无需 Tauri / Node / Electron 运行时依赖，秒级响应，超低内存占用。
- **🔄 直连与本地热切双模式**：
  - **直连模式 (Direct Mode)**：原子安全写盘，直接修改 `~/.claude/settings.json`、`~/.codex/{auth.json,config.toml}` 与 `~/.grok/config.toml`。
  - **本地代理热切模式 (Proxy Hot-Switch)**：常驻后台守护进程（`127.0.0.1:8999`），拦截大模型流式请求，通过 HTTP IPC 控制面实现**运行时动态路由热更**，终端开发无需重启会话。
- **📊 供应商测速与验真**：支持 HTTP 层网络延迟探测 + 真实流式对话首字延迟（TTFT）测速，自动脱敏 API Key。
- **🖥️ 交互式 TUI 与命令行双控**：支持直接运行交互式菜单（上下键选择、确认、自动补全），亦支持纯命令行管道化调用与快捷参数。
- **🛡️ 安全备份与环境自愈**：
  - 写入配置前毫秒级自动时间戳备份（`~/.z-switch/backups/`）。
  - 支持 `doctor` 环境体检与 `repair` 一键修复代理残留。
  - 支持 `restore` 一键回退到官方基线。
- **📦 一键无缝迁移**：全兼容 `cc-switch`（自动读取 SQLite `cc-switch.db` 及 JSON 备份），一键批量导入。
- **🔌 生态联动**：自动同步 Claude Desktop 桌面端 3p 网关配置与 VS Code Claude 扩展鉴权标记。

---

## 🚀 快速上手

### 1. 交互式菜单模式
直接在任意终端运行即可开启交互式 TUI 菜单：
```bash
z-switch
```

### 2. 命令行快捷操作

#### 切换供应商
```bash
# 模糊匹配名称切换（默认自动选择代理热切或直连）
z-switch use deepseek

# 显式指定应用和运行模式
z-switch use deepseek --app claude --proxy     # 强制本地代理热切模式
z-switch use glm4 --app codex --direct         # 强制直连写盘模式

# 指定自定义本地代理端口
z-switch use deepseek --app claude --proxy --port 8999
```

#### 查看与测速
```bash
# 列出所有已配置的供应商及其当前状态
z-switch list
z-switch list --app claude

# 测试指定供应商的 HTTP 延迟与真实流式 TTFT（首字延迟）
z-switch test deepseek
z-switch test --app claude --all               # 批量测速应用下的所有供应商
z-switch test deepseek --stream false           # 仅测试连接，不进行流式对话
```

#### 添加、编辑与删除
```bash
# 命令行添加 Claude 供应商
z-switch add --app claude --name "DeepSeek" --url "https://api.deepseek.com" --key "sk-xxxx" --model "deepseek-chat"

# 命令行添加 Codex 供应商
z-switch add --app codex --name "GLM" --url "https://open.bigmodel.cn/api/paas/v4" --key "sk-xxxx" --model "glm-4-plus"

# 编辑供应商配置（修改 URL 或模型）
z-switch edit deepseek --app claude --model "deepseek-reasoner"

# 删除供应商
z-switch remove deepseek --app claude
```

#### 本地常驻代理管理
```bash
# 启动后台常驻代理守护进程（默认 8999 端口）
z-switch proxy start

# 查看代理运行状态、PID、已路由应用与流量计数
z-switch proxy status

# 停止后台代理
z-switch proxy stop

# 重启后台代理
z-switch proxy restart

# 前台运行代理（调试时使用，按 Ctrl+C 退出）
z-switch proxy start --foreground

# 查看代理错误日志
z-switch proxy logs --tail 50
```

#### 导入与自愈
```bash
# 从 cc-switch 自动扫描导入 (SQLite / JSON)
z-switch import cc-switch

# 指定 cc-switch 数据库或 JSON 导出文件
z-switch import cc-switch --db "C:\\path\\to\\cc-switch.db"
z-switch import cc-switch --json "C:\\path\\to\\cc-switch.json"

# 从本机当前客户端环境导入
z-switch import live

# 环境体检与诊断 (检查是否存在 127.0.0.1 代理占位残留)
z-switch doctor

# 一键修复配置与环境残留
z-switch repair
```

#### 导出、恢复与配置
```bash
# 导出全部供应商配置（默认输出到终端）
z-switch export
z-switch export --output ./providers-backup.json

# 恢复指定应用的官方基线配置
z-switch restore --app claude

# 查看全部运行配置，或读取/修改单项布尔设置
z-switch config
z-switch config backupBeforeWrite
z-switch config backupBeforeWrite false
```

---

## 🔄 与 GUI 版本的配置共享机制

`z-switch-cli` 与桌面 GUI 版 `z-switch` 共享同一套配置：
- **配置文件目录**：统一读写 `~/.z-switch/providers.json`。
- **备份与快照池**：统一存储于 `~/.z-switch/backups/`（自动维护最近 60 份快照并安全轮转）。
- **端口隔离**：GUI 代理默认使用 `8899`，CLI 代理默认使用 `8999`，二者同时开启互不争抢端口。
- **代理状态文件**：CLI 默认使用 `~/.z-switch/proxy.pid`；指定其它端口时使用对应的 `proxy-<port>.pid`。
- **写入确定性**：采用原子写入、JSON 键排序与 CLI 进程间共享锁，避免 CLI 自身并发写入半成品。GUI 端仍建议避免在 CLI 写入的同时保存同一配置，并在外部切换后刷新 GUI。

---

## 🏗️ 架构设计

```
┌─────────────────────────────────────────────────────────────┐
│                 任意终端命令行 (CLI Commands)                │
│                                                             │
│   $ z-switch use deepseek                                   │
│   $ z-switch proxy status                                   │
└──────────────┬───────────────────────────────┬──────────────┘
               │ ① 写持久化配置                 │ ② HTTP IPC 控制信令
               ▼ (~/.z-switch/providers.json)  ▼ (POST http://127.0.0.1:8999/_admin/switch)
┌─────────────────────────┐         ┌─────────────────────────────────────────┐
│  ~/.z-switch/           │         │         z-switch 后台守护进程           │
│  ├── providers.json     │         │            (Daemon Process)             │
│  ├── proxy.pid          │         │ ┌─────────────────────────────────────┐ │
│  ├── logs/              │         │ │  Control Plane (本地管理路由)       │ │
│  │   └── proxy-errors.jsonl       │ │  • /_admin/health / status         │ │
└─────────────────────────┘         │ │  • /_admin/switch (内存热重载)      │ │
                                    │ │  • /_admin/shutdown                │ │
                                    │ └──────────────────┬──────────────────┘ │
                                    │                    ▼ 原子更新           │
                                    │ ┌─────────────────────────────────────┐ │
                                    │ │ In-Memory targets (读写锁)          │ │
                                    │ └──────────────────┬──────────────────┘ │
                                    │                    │ 转发映射           │
┌─────────────────────────┐ 转发请求 │ ┌──────────────────▼──────────────────┐ │
│ Claude Code / Codex CLI ├────────┼─►│  Data Plane (Axum 高性能反代核心)    │ │
│ (请求 127.0.0.1:8999)   │ (零缓冲)│ │  • /v1/messages                     │ │
│                         │         │ │  • /v1/chat/completions             │ │
│                         │         │ │  • /v1/responses                    │ │
└─────────────────────────┘         │ └──────────────────┬──────────────────┘ │
                                    └────────────────────┼────────────────────┘
                                                         ▼ 向上游透传并流式返回
                                    ┌─────────────────────────────────────────┐
                                    │ 大模型中转供应商 / 官方 API (DeepSeek...) │
                                    └─────────────────────────────────────────┘
```

---

## 🛠️ 构建与编译

### 环境要求
- Rust 1.82+ (edition 2021)
- Cargo

### 直接下载

Windows、Linux 与 macOS（Intel / Apple Silicon）版本可从 [Releases](https://github.com/ltsj/z-switch-cli/releases/latest) 直接下载，无需安装 Rust。

### 本地编译
```bash
git clone https://github.com/ltsj/z-switch-cli.git
cd z-switch-cli
cargo build --release
```

编译产物位于 `./target/release/z-switch` (Windows 为 `z-switch.exe`)。

### 持续集成

每次推送都会自动执行格式检查、Clippy、单元测试和多平台 release 构建；推送 `v*` 标签时会同步创建 GitHub Release。

---

## 📄 许可证

本项目基于 [MIT License](LICENSE) 开源。

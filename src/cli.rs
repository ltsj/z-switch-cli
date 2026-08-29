//! 命令行参数解析定义。
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "z-switch",
    bin_name = "z-switch",
    version,
    about = "z-switch-cli · Claude Code / Codex / Grok 供应商一键切换命令行工具",
    long_about = "专为 Claude Code、Codex CLI 与 Grok CLI 设计的极速、原生供应商管理与热切工具。\n支持直连与本地代理（127.0.0.1:8999）常驻后台热切双模式。"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// 指定目标应用（claude、codex、grok）
    #[arg(short, long, global = true, value_enum)]
    pub app: Option<AppType>,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppType {
    Claude,
    Codex,
    Grok,
}

impl AppType {
    pub fn as_str(&self) -> &'static str {
        match self {
            AppType::Claude => "claude",
            AppType::Codex => "codex",
            AppType::Grok => "grok",
        }
    }
}

impl std::fmt::Display for AppType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// 列出所有供应商及其状态、当前生效标记与延迟
    #[command(alias = "ls")]
    List {
        /// 仅列出指定应用的供应商
        #[arg(value_enum)]
        app: Option<AppType>,
    },

    /// 切换供应商（支持模糊匹配名称或 ID）
    #[command(alias = "switch")]
    Use {
        /// 供应商名称或 ID（留空则进入交互式选择）
        query: Option<String>,

        /// 目标应用（默认为 claude）
        #[arg(short, long, value_enum)]
        app: Option<AppType>,

        /// 强制使用本地代理热切模式
        #[arg(long, conflicts_with = "direct")]
        proxy: bool,

        /// 强制使用直连模式
        #[arg(long, conflicts_with = "proxy")]
        direct: bool,

        /// 本地代理端口（默认 8999）
        #[arg(long)]
        port: Option<u16>,
    },

    /// 对供应商进行连接性与流式 TTFT 测速
    Test {
        /// 供应商名称或 ID（留空则测试当前或交互选择）
        query: Option<String>,

        /// 目标应用
        #[arg(short, long, value_enum)]
        app: Option<AppType>,

        /// 进行真实流式对话首字延时 (TTFT) 测速
        #[arg(short, long, default_value_t = true)]
        stream: bool,

        /// 测试该应用下的所有供应商
        #[arg(long)]
        all: bool,
    },

    /// 新增供应商
    Add {
        /// 目标应用
        #[arg(short, long, value_enum)]
        app: Option<AppType>,

        /// 供应商显示名称
        #[arg(short, long)]
        name: Option<String>,

        /// 接口 Base URL
        #[arg(short, long)]
        url: Option<String>,

        /// API Key 或 Auth Token
        #[arg(short, long)]
        key: Option<String>,

        /// Claude Key 字段名（ANTHROPIC_AUTH_TOKEN 或 ANTHROPIC_API_KEY）
        #[arg(long)]
        key_field: Option<String>,

        /// 默认模型
        #[arg(short, long)]
        model: Option<String>,

        /// 协议格式（chat 或 responses，适用于 Codex/Grok）
        #[arg(long)]
        wire_api: Option<String>,
    },

    /// 修改现有供应商配置
    Edit {
        /// 供应商名称或 ID（留空则交互式选择）
        query: Option<String>,

        /// 目标应用
        #[arg(short, long, value_enum)]
        app: Option<AppType>,

        /// 修改显示名称
        #[arg(short, long)]
        name: Option<String>,

        /// 修改 Base URL
        #[arg(short, long)]
        url: Option<String>,

        /// 修改 API Key
        #[arg(short, long)]
        key: Option<String>,

        /// 修改默认模型
        #[arg(short, long)]
        model: Option<String>,
    },

    /// 删除供应商
    #[command(alias = "rm", alias = "del")]
    Remove {
        /// 供应商名称或 ID
        query: Option<String>,

        /// 目标应用
        #[arg(short, long, value_enum)]
        app: Option<AppType>,

        /// 若当前供应商正在使用中的处理方式 (keep 保留配置 / restore 恢复官方基线)
        #[arg(long)]
        mode: Option<String>,
    },

    /// 本地常驻代理管理
    Proxy {
        #[command(subcommand)]
        action: ProxyAction,
    },

    /// 从现有环境或 cc-switch 导入供应商
    Import {
        #[command(subcommand)]
        source: ImportSource,
    },

    /// 导出所有供应商与配置为 JSON
    Export {
        /// 输出文件路径（留空则输出至控制台）
        #[arg(short, long)]
        output: Option<String>,
    },

    /// 恢复官方基线配置
    Restore {
        /// 目标应用
        #[arg(value_enum)]
        app: Option<AppType>,
    },

    /// 环境诊断与配置健康检查
    Doctor,

    /// 修复本地残留占位配置或环境异常
    Repair {
        /// 目标应用
        #[arg(value_enum)]
        app: Option<AppType>,
    },

    /// 系统配置管理
    Config {
        /// 配置项名称（backupBeforeWrite / applyClaudeDesktop / applyClaudePlugin）
        key: Option<String>,

        /// 设置的值（true / false）
        value: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProxyAction {
    /// 启动本地代理（默认后台常驻）
    Start {
        /// 代理监听端口（默认 8999）
        #[arg(short, long, default_value_t = 8999)]
        port: u16,

        /// 在前台运行（用于调试）
        #[arg(short, long)]
        foreground: bool,
    },

    /// 停止后台常驻代理
    Stop {
        /// 代理端口（默认 8999）
        #[arg(short, long, default_value_t = 8999)]
        port: u16,
    },

    /// 重启后台代理
    Restart {
        /// 代理端口（默认 8999）
        #[arg(short, long, default_value_t = 8999)]
        port: u16,
    },

    /// 查看本地代理运行状态与流量统计
    Status {
        /// 代理端口（默认 8999）
        #[arg(short, long, default_value_t = 8999)]
        port: u16,
    },

    /// 查看代理错误日志
    Logs {
        /// 仅显示最近 N 条记录
        #[arg(short, long, default_value_t = 20)]
        tail: usize,
    },

    /// 内部守护工作进程命令（供后台脱离进程调用）
    #[command(hide = true)]
    Worker {
        #[arg(short, long, default_value_t = 8999)]
        port: u16,
    },
}

#[derive(Subcommand, Debug)]
pub enum ImportSource {
    /// 从 cc-switch 导入（自动检测 SQLite / JSON）
    CcSwitch {
        /// 指定 cc-switch.db 路径
        #[arg(long)]
        db: Option<String>,

        /// 指定 cc-switch json 导出文件路径
        #[arg(long)]
        json: Option<String>,
    },

    /// 从当前本机客户端环境 (~/.claude, ~/.codex, ~/.grok) 导入
    Live,
}

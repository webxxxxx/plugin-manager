//! The metadata shapes the table is made of: the console's grouping and the
//! value kinds. Split out of `config_doc.rs` so the table itself stays inside
//! the production-line budget.

/// README 与设置表单里的分组，按阅读顺序排列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Resources,
    Network,
    Logs,
    Lifecycle,
    Security,
    Paths,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Group::Resources => "资源与形状",
            Group::Network => "网络",
            Group::Logs => "日志",
            Group::Lifecycle => "生命周期",
            Group::Security => "安全",
            Group::Paths => "路径",
        }
    }
}

/// 值的形状：决定表单控件与悬浮说明里的单位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Enum,
    Bytes,
    Seconds,
    Count,
    Bool,
    SocketAddr,
    Path,
    PasswordHash,
}

impl Kind {
    /// The machine-readable form the console maps to an input control.
    pub fn code(self) -> &'static str {
        match self {
            Kind::Enum => "enum",
            Kind::Bytes => "bytes",
            Kind::Seconds => "seconds",
            Kind::Count => "count",
            Kind::Bool => "bool",
            Kind::SocketAddr => "socket-addr",
            Kind::Path => "path",
            Kind::PasswordHash => "password-hash",
        }
    }
}

/// 一行配置项元数据。
#[derive(Debug, Clone, Copy)]
pub struct ConfKey {
    pub key: &'static str,
    /// 控制台设置表单显示的中文短名；英文键留在悬浮说明里。
    pub label: &'static str,
    pub group: Group,
    pub kind: Kind,
    /// 默认值或派生规则；写规则不写死数字（预算是运行期算出来的）。
    pub default: &'static str,
    /// 干什么。
    pub doc: &'static str,
    /// 推荐怎么设。
    pub recommend: &'static str,
    /// 控制台设置表单是否可以改。路径根与探测输入是只读事实，不是设置。
    pub editable_in_console: bool,
    /// 改完是否需要重启 daemon（`auth_passwd` 走 `svc lan passwd` 可以热更新）。
    pub needs_restart: bool,
}

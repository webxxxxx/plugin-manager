//! `svcd.conf` 配置项的单一元数据表。
//!
//! 同一份信息今天有三个消费者：README 的配置项表、控制台设置表单（#263）的悬浮说明，
//! 以及「这个键还能不能写」。三处各写一份必然漂移，所以只在这里写一次：
//! 默认值、作用、推荐怎么设、能不能在控制台改、改完要不要重启。
//!
//! 表格与 `Settings` 的 live 键、`svcd.conf.example` 的文档键由本文件的
//! 测试双向守住（见 `mod tests`）。
//!
//! `default` 写的是**派生规则**而不是写死数字：预算是运行期算出来的，
//! 表格里给的是「参照预算下的期望值」（tiny@128 MiB / interactive@512 MiB /
//! streaming@2 GiB），与 `limits/specs.rs` 的标定值一致。

// 生产代码里今天只有测试守卫在读这张表；控制台设置表单（#263）落地后删掉这条
// allow，让编译器继续盯着「谁在用」。
#![allow(dead_code)]

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

    pub fn label(self) -> &'static str {
        match self {
            Kind::Enum => "枚举",
            Kind::Bytes => "字节",
            Kind::Seconds => "秒",
            Kind::Count => "计数",
            Kind::Bool => "开关",
            Kind::SocketAddr => "地址",
            Kind::Path => "路径",
            Kind::PasswordHash => "密码哈希",
        }
    }
}

/// 一行配置项元数据。
#[derive(Debug, Clone, Copy)]
pub struct ConfKey {
    pub key: &'static str,
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

/// 已从行为里删除、但解析器仍然接受（启动时告警）的键。
///
/// 它们不进 README 表，也不许被重新加回 `CONF_KEYS`：出现在旧文档里时
/// 只解释「现在实际用哪条路径」，不改任何行为。
pub const REMOVED_KEYS: &[&str] = &["boot_scan_wait_ms"];

/// 已删除的旧路径键：解析器只把它们收集成 `IgnoredPathKey` 警告。
pub const REMOVED_PATH_KEYS: &[&str] = &["plugins_dir", "services_dir", "temp_dir"];

/// `svcd.conf` 的全部 live 键，按分组与阅读顺序排列。
pub const CONF_KEYS: &[ConfKey] = &[
    ConfKey {
        key: "mem_budget_bytes",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "auto（可用内存 25%，受构建上限约束）",
        doc: "本进程可规划的峰值内存。`auto` 按可用内存折扣；写数字则按原样（只被构建上限压低）。",
        recommend: "一般留 auto；要复现一台机器的预算时写死数字。与 mem_budget_percent 二选一。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "mem_budget_percent",
        group: Group::Resources,
        kind: Kind::Count,
        default: "未设（与 mem_budget_bytes 二选一）",
        doc: "用可用内存的百分比表达同一件事，1..100，每次启动重新解析。",
        recommend: "容器/虚拟机上比写死字节更稳；宿主机内存会变时才用它。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "workload",
        group: Group::Resources,
        kind: Kind::Enum,
        default: "由预算派生（<512 MiB tiny，<2 GiB interactive，否则 streaming）",
        doc: "预算怎么花：tiny / interactive / streaming。它决定各键的派生份额，不决定预算大小。",
        recommend: "面板多的常驻场景用 interactive；大批长流/大包用 streaming；小设备用 tiny。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "cpu_budget_percent",
        group: Group::Resources,
        kind: Kind::Count,
        default: "0（不设限）",
        doc: "本进程可用的 CPU 份额，0..100；>0 时装平台硬限（cgroup cpu.max / Job Object）并在超限时拒绝新连接。",
        recommend: "0 不设限；要留余量用 70-80（90% 的排队延迟约为空闲的九倍）。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "request_stack_bytes",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "128/192/256 KiB（tiny/interactive/streaming）",
        doc: "每个请求线程的栈。不随预算变，只随 shape 变。",
        recommend: "默认即可；递归深的插件处理再调大。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "io_stack_bytes",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "64/96/128 KiB（tiny/interactive/streaming）",
        doc: "每个 IO 线程的栈。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "blocking_threads",
        group: Group::Resources,
        kind: Kind::Count,
        default: "4/8/16（tiny/interactive/streaming），2..32",
        doc: "阻塞池线程数（安装、上传、下载、API 等慢路由）。",
        recommend: "默认即可；并发管理操作多时加到 16。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "blocking_queue",
        group: Group::Resources,
        kind: Kind::Count,
        default: "由连接数解算（min(max_connections,256)），1..1024",
        doc: "阻塞池可排队的作业数；满了回 503 busy + Retry-After。",
        recommend: "默认即可；排队的作业属于已准入连接，不需要为它额外调大。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "max_body_bytes",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "8/64/512 MiB（tiny/interactive/streaming）",
        doc: "请求体在内存里的上限；更大的完整请求体会走落盘。",
        recommend: "默认即可；只在确有超大上传时调大，并同时看 max_package_bytes。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "max_json_body_bytes",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "64 KiB/256 KiB/1 MiB",
        doc: "JSON 请求体的上限，超出回 413。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "max_complete_body_bytes",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "64 KiB/1 MiB/2 MiB",
        doc: "每个连接常驻的请求体预算，是容量公式里的一项。",
        recommend: "面板很多时把它钉小（如 65536），预算让给连接数。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "max_header_bytes",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "8/16/64 KiB",
        doc: "请求头上限，超出回 431。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "max_streams",
        group: Group::Resources,
        kind: Kind::Count,
        default: "24/48/96，1..4096",
        doc: "长连接与传输流的上限：SSE、WS、raw WS、MCP SSE、上传/下载、导入导出；一次性静态资源不计。",
        recommend: "默认即可；插件面板多时调到接近连接数。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "stream_buffer_bytes",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "8/32/128 KiB",
        doc: "单个处理器的输出缓冲高水位；慢读者到点就停止输入而不是继续涨内存。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "max_connections",
        group: Group::Resources,
        kind: Kind::Count,
        default: "由预算解算（下限 8，上限 4096）",
        doc: "同时准入的连接数。显式值会被容量公式再压一次。",
        recommend: "默认让预算解算；只有要人为压并发时才写死。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "max_connections_per_ip",
        group: Group::Resources,
        kind: Kind::Count,
        default: "16/32/64",
        doc: "单个来源 IP 的连接上限。",
        recommend: "默认即可；反向代理后面调大。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "max_subscriptions",
        group: Group::Resources,
        kind: Kind::Count,
        default: "64/128/256",
        doc: "整个 daemon 的订阅数上限（每个订阅只花一个游标）。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "max_subscribers_per_plugin",
        group: Group::Resources,
        kind: Kind::Count,
        default: "16/32/64",
        doc: "单个插件的订阅者上限。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "pending_queue",
        group: Group::Resources,
        kind: Kind::Count,
        default: "32/128/512（预算/4 MiB）",
        doc: "服务器循环到监督循环之间排队的管理请求数。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "progress_queue",
        group: Group::Resources,
        kind: Kind::Count,
        default: "16/64/256（预算/8 MiB）",
        doc: "reactor 到单个 MCP 流之间排队的进度事件数。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "max_package_bytes",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "8/64/512 MiB",
        doc: "可安装 .spk 的体积上限。",
        recommend: "默认即可；交叉编译的大包再调大。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "probe_available_memory",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "未设（读操作系统）",
        doc: "声明可用内存，跳过 OS 读取；0 表示未知且不回退探测。",
        recommend: "只在 /proc 不可信的容器或复现用户机器时写；平时留空。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "probe_cgroup_remaining",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "未设（读 cgroup v2）",
        doc: "声明内存 cgroup 剩余额度，跳过 OS 读取。",
        recommend: "同上，平时留空。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "probe_cpus",
        group: Group::Resources,
        kind: Kind::Count,
        default: "未设（读 OS）",
        doc: "声明在线 CPU 数，跳过 OS 读取。",
        recommend: "同上，平时留空。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "probe_install_free_disk",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "未设（读 OS）",
        doc: "声明安装卷剩余空间，跳过 OS 读取。",
        recommend: "同上，平时留空。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "probe_temp_free_disk",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "未设（读 OS）",
        doc: "声明临时卷剩余空间，跳过 OS 读取。",
        recommend: "同上，平时留空。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "bind",
        group: Group::Network,
        kind: Kind::SocketAddr,
        default: "127.0.0.1:8787",
        doc: "HTTP 监听地址。绑定非回环地址时必须有 auth_passwd，否则回退到 127.0.0.1。",
        recommend: "默认只监听回环；要从局域网访问时先 `svc lan passwd`，再改 0.0.0.0:8787。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "http_idle_timeout_secs",
        group: Group::Network,
        kind: Kind::Seconds,
        default: "30/60/120 秒",
        doc: "空闲连接的超时。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "transfer_timeout_secs",
        group: Group::Network,
        kind: Kind::Seconds,
        default: "120 秒",
        doc: "单次传输（上传/下载/导入导出）的超时。",
        recommend: "默认即可；慢链路调大。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "control_timeout_secs",
        group: Group::Network,
        kind: Kind::Seconds,
        default: "30 秒，1..300",
        doc: "一条管理路由等控制通道回包的时限，超时回 504。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "default_timeout",
        group: Group::Network,
        kind: Kind::Count,
        default: "30000 毫秒",
        doc: "一次能力调用的默认预算（毫秒）。",
        recommend: "默认即可；插件慢时按插件调大。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "max_timeout",
        group: Group::Network,
        kind: Kind::Count,
        default: "300000 毫秒",
        doc: "调用预算的硬上限（毫秒），内核按它截断。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "ws_max_message_bytes",
        group: Group::Network,
        kind: Kind::Bytes,
        default: "1 MiB",
        doc: "单条 WebSocket 消息的上限，超出以 1009 关闭。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "ws_inflight_bytes",
        group: Group::Network,
        kind: Kind::Bytes,
        default: "2/8/64 MiB",
        doc: "所有插件未确认的 raw 字节总量。",
        recommend: "默认即可；大帧场景调大。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "ws_inflight_requests",
        group: Group::Network,
        kind: Kind::Count,
        default: "4/16/128",
        doc: "所有插件未确认的 raw 帧数。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "ws_buffer_shrink_bytes",
        group: Group::Network,
        kind: Kind::Bytes,
        default: "64 KiB",
        doc: "WebSocket 缓冲区回缩阈值。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "ws_raw_enabled",
        group: Group::Network,
        kind: Kind::Bool,
        default: "on",
        doc: "是否启用协议 3 的 raw 通道（/plugin-ui/{id}/{token}/raw）。",
        recommend: "默认开；不需要二进制通道时关掉。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "ws_raw_max_bytes",
        group: Group::Network,
        kind: Kind::Bytes,
        default: "1 MiB",
        doc: "单帧 raw 数据的上限。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "ws_raw_queue",
        group: Group::Network,
        kind: Kind::Count,
        default: "8/32/128",
        doc: "单个插件 stdin 上排队的 raw 帧数。",
        recommend: "默认即可；慢插件会填满队列而不是占线程。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "mcp_streaming",
        group: Group::Network,
        kind: Kind::Bool,
        default: "off",
        doc: "同时接受 JSON 与 SSE 的 MCP 客户端是否走流。off 时给一条 JSON（更省一个流槽与一次 reactor 跳转）。",
        recommend: "默认 off；客户端确实需要进度流时开。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "log_max_bytes",
        group: Group::Logs,
        kind: Kind::Bytes,
        default: "64 KiB，4 KiB..16 MiB",
        doc: "单个日志文件的轮转阈值。",
        recommend: "默认即可；要留长历史再调大。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "log_rotations",
        group: Group::Logs,
        kind: Kind::Count,
        default: "3（1..10）",
        doc: "保留几个轮转文件。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "log_to_file",
        group: Group::Logs,
        kind: Kind::Bool,
        default: "on",
        doc: "是否写日志文件；off 时只留 stderr/syslog。",
        recommend: "默认 on。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "log_level",
        group: Group::Logs,
        kind: Kind::Enum,
        default: "info",
        doc: "写入任何去处的最低级别：error / warn / info / debug / trace。",
        recommend: "默认 info；排查问题时临时 debug。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "log",
        group: Group::Logs,
        kind: Kind::Bool,
        default: "on",
        doc: "总开关：off 时既不写文件也不写 stderr。",
        recommend: "默认 on。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "temp_retention_secs",
        group: Group::Logs,
        kind: Kind::Seconds,
        default: "7200 秒（2 小时）",
        doc: "data/temp 下临时文件保留多久；0 关闭清理线程。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "temp_cleanup_interval_secs",
        group: Group::Logs,
        kind: Kind::Seconds,
        default: "300 秒",
        doc: "清理线程的扫描间隔。",
        recommend: "默认即可。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "rescan_interval_ms",
        group: Group::Lifecycle,
        kind: Kind::Count,
        default: "5000 毫秒（0 = 不周期重扫）",
        doc: "监督循环比对 services.d 指纹并收敛的周期；`svc reload` 仍可手动触发。",
        recommend: "默认即可；0 只在完全静态的机器上考虑。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "auth_passwd",
        group: Group::Security,
        kind: Kind::PasswordHash,
        default: "未设（用 `svc lan passwd` 设置）",
        doc: "网页/API 的口令哈希。永远不要写明文。",
        recommend: "用 `svc lan passwd <password>`；它立即生效，改文件则需要重启。",
        editable_in_console: true,
        needs_restart: false,
    },
    ConfKey {
        key: "allow_insecure_install",
        group: Group::Security,
        kind: Kind::Bool,
        default: "off",
        doc: "允许安装验签失败时降级接受未签名包。",
        recommend: "默认 off；只在可信机器上开。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "config_dir",
        group: Group::Paths,
        kind: Kind::Path,
        default: "平台默认（Linux /etc/svcd）",
        doc: "输入根：svcd.conf、services.d、trust.d、compat.conf。",
        recommend: "只读：安装时确定，改这里等于搬目录。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "data_dir",
        group: Group::Paths,
        kind: Kind::Path,
        default: "平台默认（Linux /var/lib/svcd）",
        doc: "运行状态根：plugins、temp、run。",
        recommend: "只读：安装时确定，改这里等于搬目录。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "logs_dir",
        group: Group::Paths,
        kind: Kind::Path,
        default: "平台默认（Linux /var/log/svcd）",
        doc: "日志根：svcd.log、<id>.log、<id>-hooks.log。",
        recommend: "只读：安装时确定，改这里等于搬目录。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "install_dir",
        group: Group::Paths,
        kind: Kind::Path,
        default: "未设（由 `svcd --install --prefix` 记录）",
        doc: "安装前缀：初始化时补 config/data/logs 默认值，运行时不再从它推路径。",
        recommend: "只读：用 --prefix 改，手改不会搬已存在的根。",
        editable_in_console: false,
        needs_restart: true,
    },
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::*;

    const EXAMPLE: &str = include_str!("../../svcd.conf.example");
    const README: &str = include_str!("../../README.md");

    fn table_keys() -> BTreeSet<&'static str> {
        CONF_KEYS.iter().map(|entry| entry.key).collect()
    }

    /// Every key the example file documents, commented out or not: a line whose
    /// left-hand side is a bare key name.
    fn example_keys() -> BTreeSet<String> {
        EXAMPLE
            .lines()
            .filter_map(|line| {
                let line = line.trim().trim_start_matches('#').trim_start();
                let (key, _) = line.split_once('=')?;
                let key = key.trim();
                key.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_')
                    .then(|| key.to_string())
            })
            .collect()
    }

    /// A value the parser accepts for every key of this shape; the two enum keys
    /// name their members explicitly.
    fn sample(key: &str, kind: Kind) -> &'static str {
        match (key, kind) {
            ("workload", _) => "interactive",
            ("log_level", _) => "info",
            (_, Kind::Enum) => unreachable!("every enum key is listed above"),
            (_, Kind::Bytes) => "1048576",
            (_, Kind::Seconds) => "60",
            (_, Kind::Count) => "16",
            (_, Kind::Bool) => "on",
            (_, Kind::SocketAddr) => "127.0.0.1:8787",
            (_, Kind::Path) => "/tmp/svcd-config-doc",
            (_, Kind::PasswordHash) => "pbkdf2-sha256$10000$c2FsdA==$aGFzaA==",
        }
    }

    fn parse_one(key: &str, kind: Kind) -> Result<crate::platform::settings::Settings, String> {
        let text = format!("{} = {}\n", key, sample(key, kind));
        crate::platform::settings::parse(&text, Path::new("."))
    }

    #[test]
    fn the_table_matches_the_live_settings_keys() {
        let mut live: BTreeSet<&str> = crate::platform::settings::ENV_KEYS
            .iter()
            .copied()
            .filter(|key| !REMOVED_KEYS.contains(key))
            .collect();
        let table = table_keys();

        let missing: Vec<&&str> = live.difference(&table).collect();
        let extra: Vec<&&str> = table.difference(&live).collect();
        assert!(
            missing.is_empty(),
            "these live keys have no ConfKey row: {missing:?}"
        );
        assert!(
            extra.is_empty(),
            "these ConfKey rows name keys the parser does not have: {extra:?}"
        );
        live.clear();
    }

    #[test]
    fn every_tabled_key_parses() {
        for entry in CONF_KEYS {
            parse_one(entry.key, entry.kind)
                .unwrap_or_else(|error| panic!("{} = ... does not parse: {error}", entry.key));
        }
    }

    #[test]
    fn the_example_documents_every_key_and_nothing_else() {
        let mut documented = example_keys();
        // The example still lists the removed path keys so an operator can map
        // an old document to the derived path; they are not live keys.
        for removed in REMOVED_PATH_KEYS {
            documented.remove(*removed);
        }
        let table = table_keys();
        let missing: Vec<&str> = table
            .iter()
            .copied()
            .filter(|key| !documented.contains(*key))
            .collect();
        let extra: Vec<&str> = documented
            .iter()
            .map(String::as_str)
            .filter(|key| !table.contains(*key))
            .collect();
        assert!(
            missing.is_empty(),
            "svcd.conf.example does not document: {missing:?}"
        );
        assert!(
            extra.is_empty(),
            "svcd.conf.example documents keys that are not live: {extra:?}"
        );
    }

    #[test]
    fn removed_keys_stay_out_of_the_table_but_keep_parsing() {
        for key in REMOVED_KEYS.iter().chain(REMOVED_PATH_KEYS) {
            assert!(!table_keys().contains(*key), "{key} is removed but tabled");
            crate::platform::settings::parse(&format!("{key} = 0\n"), Path::new("."))
                .unwrap_or_else(|error| {
                    panic!("an old document with {key} must still parse: {error}")
                });
        }
    }

    #[test]
    fn path_keys_are_the_read_only_roots() {
        let roots = ["config_dir", "data_dir", "logs_dir", "install_dir"];
        for entry in CONF_KEYS {
            if entry.kind == Kind::Path {
                assert!(
                    roots.contains(&entry.key),
                    "{} is a path but not a root",
                    entry.key
                );
                assert!(
                    !entry.editable_in_console,
                    "{} is a path: the console shows it read-only",
                    entry.key
                );
            }
            if roots.contains(&entry.key) {
                assert_eq!(entry.kind, Kind::Path, "{} must be Kind::Path", entry.key);
            }
        }
    }

    #[test]
    fn the_readme_table_lists_exactly_the_tabled_keys() {
        let section = README
            .split_once("### `svcd.conf` 配置项")
            .expect("README has a svcd.conf key table")
            .1;
        let section = section
            .split_once("\n## ")
            .map(|(body, _)| body)
            .unwrap_or(section);
        let rows: BTreeSet<&str> = section
            .lines()
            .filter_map(|line| {
                let rest = line.trim_start().strip_prefix("| `")?;
                let (key, _) = rest.split_once('`')?;
                Some(key)
            })
            .collect();
        let table = table_keys();
        let missing: Vec<&&str> = table.difference(&rows).collect();
        let extra: Vec<&&str> = rows.iter().filter(|key| !table.contains(**key)).collect();
        assert!(missing.is_empty(), "README table is missing: {missing:?}");
        assert!(extra.is_empty(), "README table has unknown keys: {extra:?}");
    }
}

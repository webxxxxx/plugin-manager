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

#![cfg_attr(not(feature = "http"), allow(dead_code))]
// `face/api/settings.rs` reads this table on the HTTP surface; the minimal build
// compiles the table but no endpoint reads it, so the allow stays scoped to
// `not(feature = "http")` instead of becoming crate-wide.

mod model;

pub use model::{ConfKey, Group, Kind};
/// `svcd.conf` 的全部 live 键，按分组与阅读顺序排列。
pub const CONF_KEYS: &[ConfKey] = &[
    ConfKey {
        key: "mem_budget_bytes",
        label: "内存预算",
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
        label: "内存预算百分比",
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
        label: "负载形状",
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
        label: "CPU 预算百分比",
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
        label: "请求线程栈",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "128/192/256 KiB（tiny/interactive/streaming）",
        doc: "每个请求线程的栈。不随预算变，只随 shape 变。",
        recommend: "默认即可；递归深的插件处理再调大。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "io_stack_bytes",
        label: "IO 线程栈",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "64/96/128 KiB（tiny/interactive/streaming）",
        doc: "每个 IO 线程的栈。",
        recommend: "默认即可。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "blocking_threads",
        label: "阻塞线程数",
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
        label: "阻塞队列长度",
        group: Group::Resources,
        kind: Kind::Count,
        default: "由连接数解算（min(max_connections,256)），1..1024",
        doc: "阻塞池可排队的作业数；满了回 503 busy + Retry-After。",
        recommend: "默认即可；排队的作业属于已准入连接，不需要为它额外调大。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "max_body_bytes",
        label: "请求体上限",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "8/64/512 MiB（tiny/interactive/streaming）",
        doc: "请求体在内存里的上限；更大的完整请求体会走落盘。",
        recommend: "默认即可；只在确有超大上传时调大，并同时看 max_package_bytes。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "max_json_body_bytes",
        label: "JSON 请求体上限",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "64 KiB/256 KiB/1 MiB",
        doc: "JSON 请求体的上限，超出回 413。",
        recommend: "默认即可。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "max_complete_body_bytes",
        label: "连接常驻请求体",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "64 KiB/1 MiB/2 MiB",
        doc: "每个连接常驻的请求体预算，是容量公式里的一项。",
        recommend: "面板很多时把它钉小（如 65536），预算让给连接数。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "max_header_bytes",
        label: "请求头上限",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "8/16/64 KiB",
        doc: "请求头上限，超出回 431。",
        recommend: "默认即可。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "max_streams",
        label: "长连接流上限",
        group: Group::Resources,
        kind: Kind::Count,
        default: "24/48/96，1..4096",
        doc: "长连接与传输流的上限：SSE、WS、raw WS、MCP SSE、上传/下载、导入导出；一次性静态资源不计。",
        recommend: "默认即可；插件面板多时调到接近连接数。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "stream_buffer_bytes",
        label: "输出缓冲水位",
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
        label: "最大连接数",
        group: Group::Resources,
        kind: Kind::Count,
        default: "由预算解算（下限 8，上限 4096）",
        doc: "同时准入的连接数。显式值会被容量公式再压一次。",
        recommend: "默认让预算解算；只有要人为压并发时才写死。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "max_connections_per_ip",
        label: "单 IP 连接上限",
        group: Group::Resources,
        kind: Kind::Count,
        default: "16/32/64",
        doc: "单个来源 IP 的连接上限。",
        recommend: "默认即可；反向代理后面调大。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "max_subscriptions",
        label: "订阅总数上限",
        group: Group::Resources,
        kind: Kind::Count,
        default: "64/128/256",
        doc: "整个 daemon 的订阅数上限（每个订阅只花一个游标）。",
        recommend: "默认即可。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "max_subscribers_per_plugin",
        label: "单插件订阅上限",
        group: Group::Resources,
        kind: Kind::Count,
        default: "16/32/64",
        doc: "单个插件的订阅者上限。",
        recommend: "默认即可。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "pending_queue",
        label: "管理请求队列",
        group: Group::Resources,
        kind: Kind::Count,
        default: "32/128/512（预算/4 MiB）",
        doc: "服务器循环到监督循环之间排队的管理请求数。",
        recommend: "默认即可。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "progress_queue",
        label: "进度事件队列",
        group: Group::Resources,
        kind: Kind::Count,
        default: "16/64/256（预算/8 MiB）",
        doc: "reactor 到单个 MCP 流之间排队的进度事件数。",
        recommend: "默认即可。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "max_package_bytes",
        label: "安装包体积上限",
        group: Group::Resources,
        kind: Kind::Bytes,
        default: "8/64/512 MiB（tiny/interactive/streaming）",
        doc: "可安装 .spk 的体积上限。超出这个值的包在上传阶段就被 413 拒绝。",
        recommend: "低内存宿主默认 8 MiB，装十几 MiB 的插件时调到 32 MiB（可写 32 MiB 或字节数）。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "probe_available_memory",
        label: "可用内存探测",
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
        label: "cgroup 内存探测",
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
        label: "CPU 数探测",
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
        label: "安装卷空间探测",
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
        label: "临时卷空间探测",
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
        label: "监听地址",
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
        label: "空闲连接超时",
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
        label: "传输超时",
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
        label: "控制通道超时",
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
        label: "默认调用预算",
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
        label: "调用预算上限",
        group: Group::Network,
        kind: Kind::Count,
        default: "300000 毫秒",
        doc: "调用预算的硬上限（毫秒），内核按它截断。",
        recommend: "默认即可。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "ws_max_message_bytes",
        label: "WS 单条消息上限",
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
        label: "WS 未确认字节",
        group: Group::Network,
        kind: Kind::Bytes,
        default: "2/8/64 MiB",
        doc: "所有插件未确认的 raw 字节总量。",
        recommend: "默认即可；大帧场景调大。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "ws_inflight_requests",
        label: "WS 未确认帧数",
        group: Group::Network,
        kind: Kind::Count,
        default: "4/16/128",
        doc: "所有插件未确认的 raw 帧数。",
        recommend: "默认即可。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "ws_buffer_shrink_bytes",
        label: "WS 缓冲回缩阈值",
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
        label: "原始二进制通道",
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
        label: "原始帧大小上限",
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
        label: "原始帧队列长度",
        group: Group::Network,
        kind: Kind::Count,
        default: "8/32/128",
        doc: "单个插件 stdin 上排队的 raw 帧数。",
        recommend: "默认即可；慢插件会填满队列而不是占线程。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "mcp_streaming",
        label: "MCP 流式响应",
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
        label: "日志轮转大小",
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
        label: "日志保留份数",
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
        label: "写日志文件",
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
        label: "日志级别",
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
        label: "日志总开关",
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
        label: "临时文件保留时长",
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
        label: "临时文件清理间隔",
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
        label: "服务目录重扫间隔",
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
        label: "登录口令",
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
        label: "允许未签名安装",
        group: Group::Security,
        kind: Kind::Bool,
        default: "off",
        doc: "允许安装验签失败时降级接受未签名包。",
        recommend: "默认 off；只在可信机器上开。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "vendor_login",
        label: "厂商后台免登录",
        group: Group::Security,
        kind: Kind::Bool,
        default: "off",
        doc: "允许同主机的厂商后台（:80）已登录时，用浏览器里的厂商会话换一个 svcd 会话（`POST /vendor-login`），控制台不再要求密码。厂商会话绑客户端 IP，服务端不能代查，所以信任的是浏览器里厂商同源页面的断言。",
        recommend: "只在“厂商后台是这台设备唯一管理入口”的镜像上开；开着时能访问 :8787 的非浏览器客户端可以伪造请求头拿到管理员会话，关着时控制台照常要密码。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "config_dir",
        label: "配置目录",
        group: Group::Paths,
        kind: Kind::Path,
        default: "平台默认（Linux /etc/svcd）",
        doc: "输入根：svcd.conf、services.d、trust.d、compat.conf。",
        recommend: "只读：安装时确定，改这里等于搬目录。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "plugins_dir",
        label: "插件根",
        group: Group::Paths,
        kind: Kind::Path,
        default: "未设（派生 `<data_dir>/plugins`）",
        doc: "插件根列表，逗号分隔，顺序即优先级：第一个是默认安装根，每个已装插件的实际落点记在 `<data_dir>/plugin-locations.json`。相对路径按 svcd.conf 所在目录解析；重复项去重，空项报错。控制台把第一项当默认安装根固定住，仍有插件的根不给删。",
        recommend: "多根装在不同的卷上；删掉一个还装着插件的根会被拒绝，先把它上面的插件移走或卸载。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "manual_install_root",
        label: "手动选择安装位置",
        group: Group::Paths,
        kind: Kind::Bool,
        default: "开",
        doc: "安装插件时是否让用户挑插件根。开：只在可选根多于一个时弹选择器（列空闲空间与已装插件数）；关：宿主自己选装得下的最小根，不弹任何选择框。上传安装与插件商店共用这个开关。",
        recommend: "只有一块盘时开不开都一样；多块盘、想按插件大小自己分配时保持打开。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "data_dir",
        label: "数据目录",
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
        label: "日志目录",
        group: Group::Paths,
        kind: Kind::Path,
        default: "平台默认（Linux /var/log/svcd）",
        doc: "日志根：svcd.log、<id>.log、<id>-hooks.log。",
        recommend: "只读：安装时确定，改这里等于搬目录。",
        editable_in_console: false,
        needs_restart: true,
    },
    ConfKey {
        key: "static_root",
        label: "静态站点根目录",
        group: Group::Paths,
        kind: Kind::Path,
        default: "未设（关闭）",
        doc: "只读静态站点根：目录下的文件通过 `/static/` 访问（`/static/` 与 `/static/<dir>/` 取 `index.html`）。",
        recommend: "把构建产物目录写在这里；相对路径按 svcd.conf 所在目录解析，清空即关闭 `/static/`。",
        editable_in_console: true,
        needs_restart: true,
    },
    ConfKey {
        key: "install_dir",
        label: "安装目录",
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

    /// 已从行为里删除、但解析器仍然接受（启动时告警）的键。
    ///
    /// 它们不进 README 表，也不许被重新加回 `CONF_KEYS`：出现在旧文档里时
    /// 只解释「现在实际用哪条路径」，不改任何行为。
    const REMOVED_KEYS: &[&str] = &["boot_scan_wait_ms"];

    /// 已删除的旧路径键：解析器只把它们收集成 `IgnoredPathKey` 警告。
    const REMOVED_PATH_KEYS: &[&str] = &["services_dir", "temp_dir"];

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
    fn derived_detail_keys_are_read_only() {
        // #263: budget-derived detail (max_*, queue sizes, stacks, raw in-flight
        // budgets) is shown in the settings screen but never editable there.
        for entry in CONF_KEYS {
            // `max_package_bytes` is a policy cap the operator may raise (a
            // package over it can never be installed), not detail derived from
            // the memory budget (#290).
            let derived = (entry.key.starts_with("max_") && entry.key != "max_package_bytes")
                || entry.key.ends_with("_queue")
                || entry.key.ends_with("_stack_bytes")
                || entry.key.starts_with("ws_inflight_");
            if derived {
                assert!(
                    !entry.editable_in_console,
                    "{} is derived and must stay read-only",
                    entry.key
                );
            }
        }
    }

    #[test]
    fn every_key_has_a_unique_chinese_label() {
        let mut labels = BTreeSet::new();
        for entry in CONF_KEYS {
            assert!(
                !entry.label.trim().is_empty(),
                "{} has no console label",
                entry.key
            );
            assert!(
                entry
                    .label
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{} label is not Chinese: {}",
                entry.key,
                entry.label
            );
            assert!(
                labels.insert(entry.label),
                "{} reuses the label {}",
                entry.key,
                entry.label
            );
        }
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

    /// A path key is a root the installer records (read-only: editing it would
    /// point the daemon at a tree nothing prepared) or one the operator owns.
    /// `static_root` only names where `/static/` reads from (#301), and
    /// `plugins_dir` is a list the operator maintains — the console refuses to
    /// drop a root that still holds plugins instead of hiding the key (#298).
    #[test]
    fn path_keys_are_roots_and_only_the_operator_owned_roots_are_editable() {
        let roots = [
            "config_dir",
            "plugins_dir",
            "data_dir",
            "logs_dir",
            "static_root",
            "install_dir",
        ];
        let editable_roots = ["plugins_dir", "static_root"];
        for entry in CONF_KEYS {
            if entry.kind == Kind::Path {
                assert!(
                    roots.contains(&entry.key),
                    "{} is a path but not a root",
                    entry.key
                );
                assert_eq!(
                    entry.editable_in_console,
                    editable_roots.contains(&entry.key),
                    "{} has the wrong console editability",
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

//! 失败类别与调用结果。
//!
//! 一次调用的结局只有两种形状：
//!
//! - [`Failure`]：**尝试过**，实现（或投递路径）给出了类别；
//! - [`Rejection`]：**没有被尝试，也不会被尝试**——能力不存在、参数不合规、超出上限。
//!
//! 后者不是失败类别：内核在把调用交给实现之前就拒绝，所以它没有 [`Class`]。
//! 这条区分让"重试"有意义：只有 [`Failure`] 才带重试语义。

use std::time::Duration;

/// 实现或投递路径在**尝试之后**给出的失败类别。恰好八类。
///
/// 类别是领域词汇，不是字符串；对外面的投影（HTTP 状态码、CLI 退出码、
/// MCP 错误码）都从这张表出，所以增减类别是破坏性变更。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// 服务没在运行：进程不在、已被停止、正在重启。
    NotRunning,
    /// 在途上限已满：背压，不是错误。
    Busy,
    /// 内联参数超过契约声明的上限。
    FrameTooLarge,
    /// 往服务写调用失败：管道断了，通常意味着服务正在死。
    WriteFailed,
    /// 内核计时的超时：服务还活着，但没在预算内答复。
    Timeout,
    /// 调用方主动取消。
    Cancelled,
    /// 实现内部错误：它答应了，但自己崩了/返回了不合规的结果。
    ImplementationError,
    /// 依赖缺失：被依赖的服务不在，调用没意义。
    MissingDependency,
}

impl Class {
    pub const ALL: [Class; 8] = [
        Class::NotRunning,
        Class::Busy,
        Class::FrameTooLarge,
        Class::WriteFailed,
        Class::Timeout,
        Class::Cancelled,
        Class::ImplementationError,
        Class::MissingDependency,
    ];

    /// 从稳定名字反查类别；只给协议/投影解析用，不用来从 `detail` 猜类别。
    pub fn from_str(name: &str) -> Option<Class> {
        Class::ALL.into_iter().find(|class| class.as_str() == name)
    }

    /// 稳定的名字：日志与投影用。改名字等于改契约。
    pub fn as_str(self) -> &'static str {
        match self {
            Class::NotRunning => "not_running",
            Class::Busy => "busy",
            Class::FrameTooLarge => "frame_too_large",
            Class::WriteFailed => "write_failed",
            Class::Timeout => "timeout",
            Class::Cancelled => "cancelled",
            Class::ImplementationError => "implementation_error",
            Class::MissingDependency => "missing_dependency",
        }
    }

    /// 内核按类别决定重试形状，实现只能声明；声明与类别不符会被归一。
    pub fn default_retry(self) -> Retry {
        match self {
            Class::NotRunning => Retry::After(Duration::from_secs(1)),
            Class::Busy => Retry::After(Duration::from_millis(100)),
            Class::Timeout => Retry::After(Duration::from_secs(1)),
            Class::MissingDependency => Retry::After(Duration::from_secs(5)),
            Class::FrameTooLarge => Retry::Never,
            Class::WriteFailed => Retry::Never,
            Class::Cancelled => Retry::Never,
            Class::ImplementationError => Retry::Never,
        }
    }
}

/// 重试建议。`After` 是退避，不是"立刻再试"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// 立刻再试。今天只有内核的失败类别测试构造它（用来断言 `Now` 与
    /// `After(_)` 在 `shape()` 里同属可重试）；生产只会给 `After` / `Never`。
    #[cfg_attr(not(test), allow(dead_code))]
    Now,
    Never,
    After(Duration),
}

impl Retry {
    /// 只比较**可重试与否**：`Now` 与 `After(_)` 同属可重试。
    ///
    /// 类别决定"可不可重试"（实现不许把永久失败说成可重试，也不许把
    /// `NotRunning` 说成永不重试）；"立刻还是退避"是实现的余量，
    /// 编排可以再按自己的策略调整。
    pub fn shape(self) -> RetryShape {
        match self {
            Retry::Now | Retry::After(_) => RetryShape::Retryable,
            Retry::Never => RetryShape::Never,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryShape {
    Retryable,
    Never,
}

/// 一次尝试的失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub class: Class,
    pub retry: Retry,
    /// 只给人看：日志与排障。上层**不得**解析它来判断类别。
    pub detail: String,
}

impl Failure {
    /// 归一：类别决定**可重试与否**；实现声明的"立刻 / 退避"只在同一侧时被保留。
    ///
    /// 这样实现永远无法把 `ImplementationError` 说成"可重试"，也无法把
    /// `NotRunning` 说成"永不重试"——可重试性是事实，具体退避是余量。
    pub fn new(class: Class, retry: Retry, detail: impl Into<String>) -> Failure {
        let default = class.default_retry();
        let retry = if retry.shape() == default.shape() {
            retry
        } else {
            default
        };
        Failure {
            class,
            retry,
            detail: detail.into(),
        }
    }

    /// 插件协议里的 `error` 对象归一成失败。
    ///
    /// 插件可以声明类别，但 `cancelled` 只能由调用方产生；插件声明它，
    /// 说明是插件自己的实现问题。未知类别同样归一为 `ImplementationError`。
    pub fn from_plugin_error(error: &serde_json::Value) -> Failure {
        let detail = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("plugin call failed")
            .to_string();
        let class = error
            .pointer("/data/class")
            .and_then(serde_json::Value::as_str)
            .and_then(Class::from_str)
            .filter(|class| *class != Class::Cancelled)
            .unwrap_or(Class::ImplementationError);
        Failure::new(class, class.default_retry(), detail)
    }
}

/// 内核就地拒绝的原因：调用没有被尝试，也不会被尝试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// 能力不在契约声明的 `surface` 里。
    UnknownCapability { capability: String },
    /// 参数不合契约声明的 schema，或超过内联上限。
    BadArguments { detail: String },
}

impl Rejection {
    pub fn as_str(&self) -> &'static str {
        match self {
            Rejection::UnknownCapability { .. } => "unknown_capability",
            Rejection::BadArguments { .. } => "bad_arguments",
        }
    }
}

/// 管理失败的封闭类别。恰好十三类。
///
/// 与 [`Class`] 不同，`Problem` 描述的是**管理层**的失败（未知服务、冲突、
/// 平台不支持），不是一次尝试的结果；`ProblemError` 只携带类别与给人看的
/// 细节，重试语义由调用方按 HTTP/CLI 投影决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Problem {
    UnknownService,
    UnknownPlugin,
    UnknownStrategy,
    CoreService,
    NotAPlugin,
    Conflict,
    Invalid,
    UnsupportedPlatform,
    MissingDependency,
    TooLarge,
    /// The operation needs more space than the volume has. Its own category
    /// because a caller can act on it (free space, uninstall an old version),
    /// unlike a generic internal failure.
    InsufficientSpace,
    Timeout,
    Internal,
}

impl Problem {
    /// The typed management error for this category, for raise sites that
    /// already know why they are failing (#151).
    pub fn error(self, detail: impl Into<String>) -> ProblemError {
        ProblemError::new(self, detail)
    }

    /// The category a control-channel error carries in `error.data.kind`.
    ///
    /// Absent or unrecognized is `None`: the wire is the only place a category
    /// is read back, and it is never derived from `message`.
    pub fn from_error_data(data: Option<&serde_json::Value>) -> Option<Problem> {
        data?.get("kind")?.as_str().and_then(Problem::from_str)
    }

    /// The `error.data` encoding of this category, symmetric with
    /// [`Problem::from_error_data`].
    pub fn error_data(self) -> serde_json::Value {
        serde_json::json!({ "kind": self.as_str() })
    }

    pub const ALL: [Problem; 13] = [
        Problem::UnknownService,
        Problem::UnknownPlugin,
        Problem::UnknownStrategy,
        Problem::CoreService,
        Problem::NotAPlugin,
        Problem::Conflict,
        Problem::Invalid,
        Problem::UnsupportedPlatform,
        Problem::MissingDependency,
        Problem::TooLarge,
        Problem::InsufficientSpace,
        Problem::Timeout,
        Problem::Internal,
    ];

    /// 从稳定名字反查类别；只给协议/投影解析用，不用来从 `detail` 猜类别。
    pub fn from_str(name: &str) -> Option<Problem> {
        Problem::ALL
            .into_iter()
            .find(|problem| problem.as_str() == name)
    }

    /// 稳定的名字：控制通道 `error.data.kind` 与投影读它。改名字等于改契约。
    pub fn as_str(self) -> &'static str {
        match self {
            Problem::UnknownService => "unknown_service",
            Problem::UnknownPlugin => "unknown_plugin",
            Problem::UnknownStrategy => "unknown_strategy",
            Problem::CoreService => "core_service",
            Problem::NotAPlugin => "not_a_plugin",
            Problem::Conflict => "conflict",
            Problem::Invalid => "invalid",
            Problem::UnsupportedPlatform => "unsupported_platform",
            Problem::MissingDependency => "missing_dependency",
            Problem::TooLarge => "too_large",
            Problem::InsufficientSpace => "insufficient_space",
            Problem::Timeout => "timeout",
            Problem::Internal => "internal",
        }
    }
}

/// 一次空间拒绝的机读载荷（HTTP 507）。
///
/// `detail` 是给人看的一句话，这块是同一组事实的机读形状：调用方（控制台、
/// 脚本）拿它决定卸载谁、腾多少。做成类型而不是无类型 JSON，是因为写端与
/// 读端只靠字符串键对齐时，拼错一个键只会安静地少一个字段（#331）。
///
/// 线上形状由 `Serialize` 的字段名定，改名字等于改契约；它变成 JSON 只在
/// `server::http::model::json_error_with_data` 一处。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SpaceShortfall {
    /// 该卸载哪个插件：`installed` 的字节数来自它。
    pub id: String,
    /// 这次检查要求的字节数：只算这次要写入的新树，不含盘上已有的旧版
    /// （旧版的字节已经不在 `free` 里，#329）。
    pub need: u64,
    /// 旧版目录的实际占用：卸载它能还回 `root` 的字节数；没装过就是 0。
    pub installed: u64,
    /// 检查跑在哪个根上：卸载只往这个根还空间。没选根（自动选根时全都装
    /// 不下）为 `None`，线上是 `null`。
    pub root: Option<String>,
    /// 这次检查问过的根，和它们各自量到的空闲字节。`require_room*` 只问了
    /// 一个根（记录根或显式 `--root`）时这里就一行；自动选根时是全部候选，
    /// 顺序即配置顺序。
    pub roots: Vec<RootRoom>,
}

/// `roots` 里的一行：一个根，和它还剩多少空闲字节。
///
/// `free` 为 `None`（线上 `null`）是「量不到」，不是 0：平台读不出这个卷的
/// 剩余空间时，调用方不得把它当成已满或已空。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RootRoom {
    pub root: String,
    pub free: Option<u64>,
}

/// 一次管理操作的失败：类别 + 只给人看的细节。
///
/// `detail` 会原样成为 JSON-RPC `error.message`；类别走 `error.data.kind`，
/// 对外投影只读类别。类别在**抛出处**给定（#151 起 `package`/`plugin`/
/// `trust`/`transfer`/`interpreter`/`hook`/`bundle` 都返回本类型），进入
/// `ProblemError` 之后，上层不得再解析 `detail`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProblemError {
    pub problem: Problem,
    pub detail: String,
    /// 机读载荷：`detail` 给人看，这块给界面用（例如空间拒绝的
    /// `need` / `installed` / `roots`，让调用方能弹一个「卸载再装」）。
    ///
    /// 今天只有空间拒绝带载荷，所以是具体类型；出现第二种再换成和类型。
    /// 上层照旧不得解析 `detail`；需要数字就加在这里，加在抛出处。
    pub shortfall: Option<SpaceShortfall>,
}

impl ProblemError {
    pub fn new(problem: Problem, detail: impl Into<String>) -> ProblemError {
        ProblemError {
            problem,
            detail: detail.into(),
            shortfall: None,
        }
    }

    /// 附上 507 的机读载荷。投影层有它才写 `error.data`，没有就不写这个键。
    pub fn with_shortfall(mut self, shortfall: SpaceShortfall) -> ProblemError {
        self.shortfall = Some(shortfall);
        self
    }

    /// The payload only reaches the wire through the HTTP face, so a build
    /// without that face has no reader for it (`--no-default-features`).
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    pub fn shortfall(&self) -> Option<&SpaceShortfall> {
        self.shortfall.as_ref()
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for ProblemError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ProblemError {}

/// 一次调用的完整结局。
///
/// 只有 [`Instance::lifecycle`](crate::kernel::contract::Instance::lifecycle) 用它；
/// 调用面走 `Rejection` / `Failure` 两条分开的路。
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    Rejected(Rejection),
    Failed(Failure),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retry_of(failure: &Failure) -> Retry {
        failure.retry
    }

    #[test]
    fn every_class_has_a_distinct_stable_name() {
        let mut names: Vec<&str> = Class::ALL.iter().map(|class| class.as_str()).collect();
        let total = names.len();
        assert_eq!(total, 8);
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "two classes share a name");
    }

    #[test]
    fn an_implementation_cannot_claim_a_retry_shape_its_class_forbids() {
        // ImplementationError 说自己可重试：被归一为 Never。
        let failure = Failure::new(Class::ImplementationError, Retry::Now, "bug");
        assert_eq!(retry_of(&failure), Retry::Never);

        // Cancelled 说等一会再试：同样被归一。
        let failure = Failure::new(Class::Cancelled, Retry::After(Duration::from_secs(1)), "");
        assert_eq!(retry_of(&failure), Retry::Never);

        // FrameTooLarge 说立刻重试：拒绝，因为重发同样的参数永远还是太大。
        let failure = Failure::new(Class::FrameTooLarge, Retry::Now, "");
        assert_eq!(retry_of(&failure), Retry::Never);
    }

    #[test]
    fn an_implementation_cannot_claim_a_permanent_failure_is_transient() {
        // NotRunning 说永不重试：被归一为退避。
        let failure = Failure::new(Class::NotRunning, Retry::Never, "gone");
        assert_eq!(
            retry_of(&failure),
            Class::NotRunning.default_retry(),
            "a stopped service must stay retryable"
        );

        // Busy 说永不重试：背压不等于永久失败。
        let failure = Failure::new(Class::Busy, Retry::Never, "");
        assert_eq!(retry_of(&failure), Class::Busy.default_retry());
    }

    #[test]
    fn a_consistent_retry_is_kept_verbatim() {
        let custom = Retry::After(Duration::from_secs(7));
        let failure = Failure::new(Class::NotRunning, custom, "");
        assert_eq!(
            retry_of(&failure),
            custom,
            "same shape, keep the caller's backoff"
        );

        let immediate = Failure::new(Class::NotRunning, Retry::Now, "");
        assert_eq!(retry_of(&immediate), Retry::Now);
    }

    #[test]
    fn a_plugin_cannot_claim_cancelled_or_an_unknown_class() {
        let cancelled = Failure::from_plugin_error(&serde_json::json!({
            "message": "plugin says cancelled",
            "data": { "class": "cancelled" }
        }));
        assert_eq!(cancelled.class, Class::ImplementationError);
        assert_eq!(cancelled.retry, Retry::Never);
        assert_eq!(cancelled.detail, "plugin says cancelled");

        let unknown = Failure::from_plugin_error(&serde_json::json!({
            "message": "boom",
            "data": { "class": "not_a_class" }
        }));
        assert_eq!(unknown.class, Class::ImplementationError);
        assert_eq!(unknown.detail, "boom");

        let busy = Failure::from_plugin_error(&serde_json::json!({
            "message": "busy",
            "data": { "class": "busy" }
        }));
        assert_eq!(busy.class, Class::Busy);
        assert_eq!(busy.retry, Class::Busy.default_retry());
    }

    fn shortfall(need: u64) -> SpaceShortfall {
        SpaceShortfall {
            id: "demo".into(),
            need,
            installed: 4,
            root: None,
            roots: Vec::new(),
        }
    }

    /// The 507 payload used to be a `serde_json::Value`, and `Value` is not
    /// `Eq`, so `ProblemError` lost the bound (#328) and had to drop it (#331).
    /// The typed payload brings it back: an implementation detail no longer
    /// narrows what callers may assume about the public error type.
    #[test]
    fn a_problem_error_is_equality_comparable_again() {
        fn is_eq<T: Eq>(left: &T, right: &T) -> bool {
            left == right
        }

        let same = Problem::InsufficientSpace
            .error("need 10")
            .with_shortfall(shortfall(10));
        let copy = Problem::InsufficientSpace
            .error("need 10")
            .with_shortfall(shortfall(10));
        assert!(is_eq(&same, &copy));

        // The payload takes part in equality: another shortfall is another
        // refusal even when the sentence is the same.
        let other = Problem::InsufficientSpace
            .error("need 10")
            .with_shortfall(shortfall(11));
        assert!(!is_eq(&same, &other));
    }

    #[test]
    fn every_problem_has_a_distinct_stable_name() {
        let mut names: Vec<&str> = Problem::ALL
            .iter()
            .map(|problem| problem.as_str())
            .collect();
        let total = names.len();
        assert_eq!(total, 13);
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "two problems share a name");
    }

    #[test]
    fn every_problem_round_trips_through_its_stable_name() {
        for problem in Problem::ALL {
            assert_eq!(Problem::from_str(problem.as_str()), Some(problem));
        }
        assert_eq!(Problem::from_str("no_such_problem"), None);
    }

    #[test]
    fn the_problem_names_are_the_wire_contract() {
        let names: Vec<&str> = Problem::ALL
            .iter()
            .map(|problem| problem.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "unknown_service",
                "unknown_plugin",
                "unknown_strategy",
                "core_service",
                "not_a_plugin",
                "conflict",
                "invalid",
                "unsupported_platform",
                "missing_dependency",
                "too_large",
                "insufficient_space",
                "timeout",
                "internal",
            ]
        );
    }

    #[test]
    fn a_problem_error_displays_its_detail_and_carries_its_kind() {
        let error = ProblemError::new(Problem::Conflict, "plugin id conflicts");
        assert_eq!(error.problem, Problem::Conflict);
        assert_eq!(error.problem.as_str(), "conflict");
        assert_eq!(error.detail(), "plugin id conflicts");
        assert_eq!(error.to_string(), "plugin id conflicts");

        let as_error: &dyn std::error::Error = &error;
        assert_eq!(as_error.to_string(), "plugin id conflicts");
    }

    #[test]
    fn a_problem_builds_its_own_error() {
        let error = Problem::TooLarge.error("package exceeds 512 MiB");
        assert_eq!(error.problem, Problem::TooLarge);
        assert_eq!(error.detail(), "package exceeds 512 MiB");
    }

    #[test]
    fn the_wire_category_is_read_from_data_kind_only() {
        let data = serde_json::json!({ "kind": "conflict" });
        assert_eq!(
            Problem::from_error_data(Some(&data)),
            Some(Problem::Conflict)
        );

        // The caller-side category lives in a different field; a management
        // projection must not read it.
        let class = serde_json::json!({ "class": "busy" });
        assert_eq!(Problem::from_error_data(Some(&class)), None);

        let unknown = serde_json::json!({ "kind": "not_a_problem" });
        assert_eq!(Problem::from_error_data(Some(&unknown)), None);
        assert_eq!(Problem::from_error_data(None), None);

        // A message that names a category is not a category.
        let message_only = serde_json::json!({ "message": "conflict" });
        assert_eq!(Problem::from_error_data(Some(&message_only)), None);
    }

    #[test]
    fn rejected_calls_carry_no_failure_class() {
        let rejected = CallError::Rejected(Rejection::UnknownCapability {
            capability: "status".into(),
        });
        assert!(matches!(rejected, CallError::Rejected(_)));
        assert_eq!(
            Rejection::UnknownCapability {
                capability: "x".into()
            }
            .as_str(),
            "unknown_capability"
        );
        assert_eq!(
            Rejection::BadArguments { detail: "x".into() }.as_str(),
            "bad_arguments"
        );
    }
}

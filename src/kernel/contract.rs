//! 服务契约：一个值（[`Contract`]）+ 两个入口（[`Instance::call`] 与
//! [`Instance::lifecycle`]）。
//!
//! 契约里没有进程、pid、端口、重启次数——那是实现（adapter）的事。四种运行方式
//! （本机进程、内置、复合、远程）只实现同一个 [`Wire`]，由装配点按
//! [`ImplRef::tag`] 注册的 [`Launcher`] 创建。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::failure::{CallError, Class, Failure, Rejection, Retry};

/// 服务 id。与能力名分开，避免编排里把两者混用。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServiceId(String);

impl ServiceId {
    pub fn new(id: impl Into<String>) -> ServiceId {
        ServiceId(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 能力名。在一条 `surface` 内唯一。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CapabilityName(String);

impl CapabilityName {
    pub fn new(name: impl Into<String>) -> CapabilityName {
        CapabilityName(name.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 一次调用会改变什么。编排的重试决策先看它，再看失败类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// 无副作用：重试总是安全的。
    Pure,
    /// 只读：重试安全。
    Read,
    /// 有副作用：只有 `idempotent` 为真时才能自动重试。
    Write,
}

/// 一项能力。能力面是**显式声明**的，不从被调服务推导。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub name: CapabilityName,
    pub desc: String,
    /// 入参的 JSON Schema（可选：没声明就不校验）。
    pub input_schema: Option<Value>,
    pub effect: Effect,
    /// 同一 `idempotency` 键重复调用是否等价于一次（决定能不能自动重试）。
    pub idempotent: bool,
}

/// 健康探测方式。默认 `ExitOnly`：进程退出即不健康，不需要实现作答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthCheck {
    ExitOnly,
    Ping,
    Status,
}

/// 生命周期义务。默认 `Process`：进程在即生命周期，不需要实现声明动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleObligation {
    Process,
    Hooks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obligations {
    pub health: HealthCheck,
    pub lifecycle: LifecycleObligation,
    pub one_shot: bool,
}

impl Default for Obligations {
    fn default() -> Self {
        Obligations {
            health: HealthCheck::ExitOnly,
            lifecycle: LifecycleObligation::Process,
            one_shot: false,
        }
    }
}

/// 调用与并发的硬上限。来自 `kernel` 的默认值，可被契约覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// 一次调用的内联参数上限；更大的内容必须走 `$blob` 引用。
    pub max_inline: usize,
    pub max_in_flight: usize,
    pub default_timeout: Duration,
    /// 调用方给的 timeout 超过它就被压到这里。
    pub max_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_inline: 1024 * 1024,
            max_in_flight: 64,
            default_timeout: Duration::from_secs(30),
            max_timeout: Duration::from_secs(300),
        }
    }
}

/// 契约：一个服务对外承诺的全部内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contract {
    pub id: ServiceId,
    pub version: String,
    surface: Vec<Capability>,
    pub obligations: Obligations,
    /// 它**可能**给出的失败类别子集；越界的类别会被归一为
    /// [`Class::ImplementationError`]。
    pub failures: Vec<Class>,
    pub limits: Limits,
}

impl Contract {
    /// 规范化：`surface` 按能力名排序，保证三处投影的输出稳定。
    pub fn new(
        id: ServiceId,
        version: impl Into<String>,
        surface: Vec<Capability>,
        obligations: Obligations,
        failures: Vec<Class>,
        limits: Limits,
    ) -> Contract {
        let mut surface = surface;
        surface.sort_by(|left, right| left.name.cmp(&right.name));
        Contract {
            id,
            version: version.into(),
            surface,
            obligations,
            failures,
            limits,
        }
    }

    pub fn surface(&self) -> &[Capability] {
        &self.surface
    }

    pub fn capability(&self, name: &CapabilityName) -> Option<&Capability> {
        self.surface.iter().find(|capability| &capability.name == name)
    }

    /// 契约自身是否合法：能力名唯一、失败类别子集非空、上限单调。
    pub fn validate(&self) -> Result<(), String> {
        if self.id.as_str().is_empty() {
            return Err("contract: service id is empty".into());
        }
        for pair in self.surface.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(format!(
                    "contract {}: duplicate capability {}",
                    self.id.as_str(),
                    pair[0].name.as_str()
                ));
            }
        }
        if self.failures.is_empty() {
            return Err(format!(
                "contract {}: declares no failure class",
                self.id.as_str()
            ));
        }
        if self.limits.max_inline == 0 {
            return Err(format!("contract {}: max_inline is zero", self.id.as_str()));
        }
        if self.limits.default_timeout > self.limits.max_timeout {
            return Err(format!(
                "contract {}: default_timeout exceeds max_timeout",
                self.id.as_str()
            ));
        }
        Ok(())
    }
}

/// 实现引用：`tag` 决定由哪个 [`Launcher`] 创建，`binding` 只有那个 launcher 看得懂。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplRef {
    pub tag: String,
    pub binding: Value,
}

/// 生命周期动词。健康走 `Probe`，不是第三个入口，也不是保留能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Ensure,
    Quiesce,
    Probe,
}

/// 生命周期动词的结局。它是**状态**，不是值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    Live,
    Ready,
    Degraded(String),
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Value(Value),
    Stream(StreamId),
}

/// 协作式取消：调用方置位，实现自行决定何时观察。取消后不得返回 `Ok`。
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// 一次调用：能力名、参数、预算，以及可选的幂等键与 trace。
#[derive(Debug, Clone)]
pub struct Invocation {
    pub capability: CapabilityName,
    pub arguments: Value,
    /// 调用方给的预算；内核到点即判 [`Class::Timeout`]，实现不得延长。
    pub timeout: Duration,
    /// 自动重试的幂等键：同一个键重复调用等价于一次（仅当能力 `idempotent`）。
    pub idempotency: Option<String>,
    /// 排障用的串联 id；实现可以忽略。
    pub trace: Option<String>,
    pub cancel: CancelToken,
}

impl Invocation {
    pub fn new(capability: impl Into<String>, arguments: Value, timeout: Duration) -> Invocation {
        Invocation {
            capability: CapabilityName::new(capability),
            arguments,
            timeout,
            idempotency: None,
            trace: None,
            cancel: CancelToken::default(),
        }
    }
}

/// 运行实现唯一需要实现的两个入口。
pub trait Wire {
    fn call(&mut self, invocation: Invocation) -> Result<Reply, Failure>;
    fn lifecycle(&mut self, control: Control) -> Result<Settled, Failure>;
}

/// 一个运行中的服务：契约 + 实现。内核在两个入口上做本地拒绝与归一。
pub struct Instance {
    contract: Contract,
    wire: Box<dyn Wire>,
}

impl Instance {
    /// 创建前先校验契约：不合法的契约不该有实例。
    pub fn new(contract: Contract, wire: Box<dyn Wire>) -> Result<Instance, String> {
        contract.validate()?;
        Ok(Instance { contract, wire })
    }

    pub fn contract(&self) -> &Contract {
        &self.contract
    }

    /// 调用。三种情况**不会**到达实现：能力不在能力面、参数超内联上限、
    /// timeout 为零——它们返回 [`CallError::Rejected`]，不带失败类别。
    pub fn call(&mut self, mut invocation: Invocation) -> Result<Reply, CallError> {
        if self.contract.capability(&invocation.capability).is_none() {
            return Err(CallError::Rejected(Rejection::UnknownCapability {
                capability: invocation.capability.as_str().to_string(),
            }));
        }
        if invocation.timeout.is_zero() {
            return Err(CallError::Rejected(Rejection::BadArguments {
                detail: "timeout must be greater than zero".into(),
            }));
        }
        let inline = inline_len(&invocation.arguments);
        if inline > self.contract.limits.max_inline {
            return Err(CallError::Rejected(Rejection::BadArguments {
                detail: format!(
                    "arguments are {} bytes inline, limit is {}",
                    inline, self.contract.limits.max_inline
                ),
            }));
        }
        // 超出上限按上限计：防呆，不是调用方错误。
        if invocation.timeout > self.contract.limits.max_timeout {
            invocation.timeout = self.contract.limits.max_timeout;
        }
        match self.wire.call(invocation) {
            Ok(reply) => Ok(reply),
            Err(failure) if self.contract.failures.contains(&failure.class) => {
                Err(CallError::Failed(failure))
            }
            // 越界类别归一：实现报了契约没声明的类别，说明它在撒谎或契约写漏了。
            // 对调用方统一成 ImplementationError，避免"未声明的类别"泄漏到投影层。
            Err(failure) => Err(CallError::Failed(Failure::new(
                Class::ImplementationError,
                Retry::Never,
                format!(
                    "undeclared failure class {}: {}",
                    failure.class.as_str(),
                    failure.detail
                ),
            ))),
        }
    }

    pub fn lifecycle(&mut self, control: Control) -> Result<Settled, CallError> {
        self.wire.lifecycle(control).map_err(CallError::Failed)
    }
}

/// 内联参数的字节数。用序列化长度，因为它就是线格式的代价。
pub fn inline_len(arguments: &Value) -> usize {
    serde_json::to_vec(arguments).map(|bytes| bytes.len()).unwrap_or(usize::MAX)
}

/// 装配点注册：`tag` → 创建实现的工厂。新增运行载体只加一个 launcher。
pub trait Launcher: Send + Sync {
    fn launch(&self, contract: &Contract, spec: &ImplRef) -> Result<Box<dyn Wire>, Failure>;
}

#[derive(Default)]
pub struct Launchers {
    by_tag: HashMap<String, Box<dyn Launcher>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LaunchError {
    UnknownTag(String),
    Failed(Failure),
}

impl Launchers {
    pub fn register(&mut self, tag: impl Into<String>, launcher: Box<dyn Launcher>) {
        self.by_tag.insert(tag.into(), launcher);
    }

    pub fn launch(&self, contract: Contract, spec: &ImplRef) -> Result<Instance, LaunchError> {
        let launcher = self
            .by_tag
            .get(&spec.tag)
            .ok_or_else(|| LaunchError::UnknownTag(spec.tag.clone()))?;
        let wire = launcher
            .launch(&contract, spec)
            .map_err(LaunchError::Failed)?;
        Instance::new(contract, wire).map_err(|detail| {
            LaunchError::Failed(Failure::new(
                Class::ImplementationError,
                super::failure::Retry::Never,
                detail,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    /// 记录调用次数与最后一次 timeout：用来证明"内核拒绝时实现一次都没被叫到"，
    /// 以及"超上限的 timeout 被压到上限"。
    struct RecordingWire {
        calls: Arc<AtomicUsize>,
        last_timeout: Arc<std::sync::Mutex<Option<Duration>>>,
        reply: Value,
    }

    impl RecordingWire {
        fn boxed(reply: Value) -> (Box<dyn Wire>, Arc<AtomicUsize>, Arc<std::sync::Mutex<Option<Duration>>>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let last_timeout = Arc::new(std::sync::Mutex::new(None));
            let wire = RecordingWire {
                calls: calls.clone(),
                last_timeout: last_timeout.clone(),
                reply,
            };
            (Box::new(wire), calls, last_timeout)
        }
    }

    impl Wire for RecordingWire {
        fn call(&mut self, invocation: Invocation) -> Result<Reply, Failure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_timeout.lock().expect("timeout slot") = Some(invocation.timeout);
            Ok(Reply::Value(self.reply.clone()))
        }

        fn lifecycle(&mut self, _control: Control) -> Result<Settled, Failure> {
            Ok(Settled::Ready)
        }
    }

    fn contract_with(limits: Limits) -> Contract {
        Contract::new(
            ServiceId::new("demo"),
            "1.0.0",
            vec![Capability {
                name: CapabilityName::new("status"),
                desc: "sample".into(),
                input_schema: None,
                effect: Effect::Read,
                idempotent: true,
            }],
            Obligations::default(),
            vec![Class::NotRunning, Class::ImplementationError],
            limits,
        )
    }

    fn instance_with(limits: Limits) -> (Instance, Arc<AtomicUsize>, Arc<std::sync::Mutex<Option<Duration>>>) {
        let (wire, calls, last_timeout) = RecordingWire::boxed(json!({"ok": true}));
        let instance = Instance::new(contract_with(limits), wire).expect("valid contract");
        (instance, calls, last_timeout)
    }

    #[test]
    fn surface_is_sorted_so_projections_are_stable() {
        let capability = |name: &str| Capability {
            name: CapabilityName::new(name),
            desc: String::new(),
            input_schema: None,
            effect: Effect::Pure,
            idempotent: true,
        };
        let contract = Contract::new(
            ServiceId::new("demo"),
            "1.0.0",
            vec![capability("zeta"), capability("alpha")],
            Obligations::default(),
            vec![Class::ImplementationError],
            Limits::default(),
        );
        let names: Vec<&str> = contract
            .surface()
            .iter()
            .map(|capability| capability.name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    #[test]
    fn a_contract_with_duplicate_capabilities_is_invalid() {
        let mut contract = contract_with(Limits::default());
        contract.surface.push(contract.surface[0].clone());
        let error = contract.validate().expect_err("duplicate must be rejected");
        assert!(error.contains("duplicate capability"), "{error}");
    }

    #[test]
    fn an_unknown_capability_is_rejected_without_touching_the_implementation() {
        let (mut instance, calls, _) = instance_with(Limits::default());
        let error = instance
            .call(Invocation::new("nope", json!({}), Duration::from_secs(1)))
            .expect_err("must be rejected");
        assert!(
            matches!(
                error,
                CallError::Rejected(Rejection::UnknownCapability { ref capability })
                    if capability == "nope"
            ),
            "{error:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "the implementation must not be called");
    }

    #[test]
    fn arguments_over_the_inline_limit_are_rejected_before_the_call() {
        let limits = Limits { max_inline: 16, ..Limits::default() };
        let (mut instance, calls, _) = instance_with(limits);
        let error = instance
            .call(Invocation::new(
                "status",
                json!({"pad": "x".repeat(64)}),
                Duration::from_secs(1),
            ))
            .expect_err("too large");
        assert!(matches!(error, CallError::Rejected(Rejection::BadArguments { .. })), "{error:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "the implementation must not be called");
    }

    #[test]
    fn a_zero_timeout_is_rejected() {
        let (mut instance, calls, _) = instance_with(Limits::default());
        let error = instance
            .call(Invocation::new("status", json!({}), Duration::ZERO))
            .expect_err("zero timeout");
        assert!(matches!(error, CallError::Rejected(Rejection::BadArguments { .. })), "{error:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_timeout_above_the_ceiling_is_clamped_not_rejected() {
        let limits = Limits { default_timeout: Duration::from_secs(5), max_timeout: Duration::from_secs(5), ..Limits::default() };
        let (mut instance, calls, last_timeout) = instance_with(limits);
        instance
            .call(Invocation::new("status", json!({}), Duration::from_secs(60)))
            .expect("clamped, not rejected");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let observed = last_timeout.lock().expect("timeout slot").expect("observed");
        assert_eq!(observed, Duration::from_secs(5), "clamped to max_timeout");
    }

    #[test]
    fn an_undeclared_failure_class_is_normalized_to_implementation_error() {
        // 契约只声明 NotRunning 与 ImplementationError；实现却报了 Busy。
        let wire = crate::kernel::memory::MemoryWire::new().with_failure(
            "status",
            Failure::new(Class::Busy, Retry::Never, "busy"),
        );
        let mut instance = Instance::new(contract_with(Limits::default()), Box::new(wire)).expect("valid");
        match instance.call(Invocation::new("status", json!({}), Duration::from_secs(1))) {
            Err(CallError::Failed(failure)) => assert_eq!(
                failure.class,
                Class::ImplementationError,
                "an undeclared class must be normalized, not leaked"
            ),
            other => panic!("expected a normalized failure, got {other:?}"),
        }
    }

    #[test]
    fn lifecycle_is_forwarded_to_the_implementation() {
        let (mut instance, _, _) = instance_with(Limits::default());
        assert_eq!(instance.lifecycle(Control::Ensure), Ok(Settled::Ready));
        assert_eq!(instance.lifecycle(Control::Probe), Ok(Settled::Ready));
    }

    #[test]
    fn an_unregistered_tag_is_a_launch_error_not_a_call_failure() {
        let launchers = Launchers::default();
        let outcome = launchers.launch(
            contract_with(Limits::default()),
            &ImplRef { tag: "wasm".into(), binding: json!({}) },
        );
        match outcome {
            Err(LaunchError::UnknownTag(tag)) => assert_eq!(tag, "wasm"),
            Err(other) => panic!("unexpected launch error: {other:?}"),
            Ok(_) => panic!("an unregistered tag must fail to launch"),
        }
    }
}
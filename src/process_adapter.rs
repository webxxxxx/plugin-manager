//! 进程 adapter：\`tag=process\`，把一个子进程当作服务实现。
//!
//! 这是 \`kernel::contract::Wire\` 的第一种真实运行载体：它只负责「把 Invocation
//! 写成插件协议帧、等应答、把失败归到 kernel 的 8 类」，不负责监督状态机。
//! 生命周期动作映射为进程的起停；监督仍由 \`slot\` 负责。
//!
//! 线格式与现有插件协议一致：protocol 1 是 NDJSON，protocol >= 2 是 4 字节长度帧。
//! \`ImplRef.binding\` 的形状：
//!
//! \`\`\`json
//! { "path": "/path/to/agent", "args": ["..."], "env": {"K": "V"}, "protocol": 1 }
//! \`\`\`

use std::collections::BTreeMap;
use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::ipc;
use crate::kernel::contract::{
    Control, Contract, ImplRef, Invocation, Launcher, Reply, Settled, Wire,
};
use crate::kernel::failure::{Class, Failure, Retry};

/// 进程 adapter 的装配入口：\`tag=process\`。
pub struct ProcessLauncher;

impl Launcher for ProcessLauncher {
    fn launch(&self, contract: &Contract, spec: &ImplRef) -> Result<Box<dyn Wire>, Failure> {
        let spec = ProcessSpec::parse(&spec.binding)?;
        Ok(Box::new(ProcessWire::new(contract.clone(), spec)))
    }
}

/// 一个子进程的启动参数。\`binding\` 只在 launcher 里解释，不进内核。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSpec {
    pub path: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub protocol: u32,
}

impl ProcessSpec {
    pub fn parse(binding: &Value) -> Result<ProcessSpec, Failure> {
        let path = binding
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                Failure::new(
                    Class::MissingDependency,
                    Retry::Never,
                    "process binding is missing a non-empty path",
                )
            })?
            .to_string();
        let args = match binding.get("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_string).ok_or_else(|| {
                        Failure::new(
                            Class::MissingDependency,
                            Retry::Never,
                            "process binding args must be strings",
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err(Failure::new(
                    Class::MissingDependency,
                    Retry::Never,
                    "process binding args must be an array",
                ))
            }
        };
        let env = match binding.get("env") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(Value::Object(values)) => values
                .iter()
                .map(|(key, value)| {
                    value
                        .as_str()
                        .map(|value| (key.clone(), value.to_string()))
                        .ok_or_else(|| {
                            Failure::new(
                                Class::MissingDependency,
                                Retry::Never,
                                "process binding env values must be strings",
                            )
                        })
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?,
            Some(_) => {
                return Err(Failure::new(
                    Class::MissingDependency,
                    Retry::Never,
                    "process binding env must be an object",
                ))
            }
        };
        let protocol = match binding.get("protocol") {
            None | Some(Value::Null) => 1,
            Some(value) => value
                .as_u64()
                .filter(|value| *value >= 1 && *value <= 3)
                .ok_or_else(|| {
                    Failure::new(
                        Class::MissingDependency,
                        Retry::Never,
                        "process binding protocol must be 1, 2 or 3",
                    )
                })? as u32,
        };
        Ok(ProcessSpec {
            path,
            args,
            env,
            protocol,
        })
    }
}

/// 一个进程实现。\`call\` 是同步的：写帧、等对应 id 的应答；超时/进程死/帧过大
/// 都在这里归到 kernel 的失败类别。
pub struct ProcessWire {
    contract: Contract,
    spec: ProcessSpec,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    frames: Option<Receiver<ipc::FrameEvent>>,
    next_id: u64,
    degraded: Option<String>,
    forced_failure: Option<Class>,
    #[cfg(windows)]
    job: Option<crate::proc::Job>,
}

impl ProcessWire {
    pub fn new(contract: Contract, spec: ProcessSpec) -> ProcessWire {
        ProcessWire {
            contract,
            spec,
            child: None,
            stdin: None,
            frames: None,
            next_id: 1,
            degraded: None,
            forced_failure: None,
            #[cfg(windows)]
            job: None,
        }
    }

    #[cfg(test)]
    pub fn forced_failure(mut self, class: Class) -> ProcessWire {
        self.forced_failure = Some(class);
        self
    }

    fn ensure_started(&mut self) -> Result<(), Failure> {
        if self.child_alive() {
            return Ok(());
        }
        self.stop_child();
        let mut command = Command::new(&self.spec.path);
        command.args(&self.spec.args);
        for (key, value) in &self.spec.env {
            command.env(key, value);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::proc::configure_service(&mut command);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        if self.job.is_none() {
            self.job = crate::proc::Job::new();
        }

        let mut child = command.spawn().map_err(|error| {
            Failure::new(
                Class::NotRunning,
                Retry::After(Duration::from_secs(1)),
                format!("cannot spawn {}: {error}", self.spec.path),
            )
        })?;
        if let Err(error) = crate::proc::assign(&child) {
            crate::log::svcd(&format!("process adapter: parent-death protection: {error}"));
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            let _ = job.assign(&child);
        }

        let stdin = child.stdin.take();
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Failure::new(
                Class::ImplementationError,
                Retry::Never,
                "piped stdout is missing",
            ));
        };
        if let Some(stderr) = child.stderr.take() {
            let name = format!("process-adapter-stderr-{}", self.spec.path);
            let _ = thread::Builder::new()
                .name(name)
                .stack_size(crate::threads::io_stack())
                .spawn(move || {
                    use std::io::BufRead;
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        crate::log::svcd(&format!("process adapter stderr: {line}"));
                    }
                });
        }

        let (sender, receiver) = mpsc::channel();
        let protocol = self.spec.protocol;
        thread::Builder::new()
            .name("process-adapter-reader".into())
            .stack_size(crate::threads::io_stack())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let frame = if protocol >= 2 {
                        ipc::read_length_frame_limit(&mut reader, ipc::CALL_FRAME_MAX)
                    } else {
                        ipc::read_frame_limit(&mut reader, ipc::CALL_FRAME_MAX)
                    };
                    let event = match frame {
                        Ok(Some(line)) => ipc::FrameEvent::Frame(Ok(line)),
                        Ok(None) => ipc::FrameEvent::Closed,
                        Err(error) => ipc::FrameEvent::Frame(Err(error)),
                    };
                    let closed = matches!(
                        event,
                        ipc::FrameEvent::Closed | ipc::FrameEvent::Frame(Err(_))
                    );
                    if sender.send(event).is_err() || closed {
                        break;
                    }
                }
            })
            .map_err(|error| {
                Failure::new(
                    Class::ImplementationError,
                    Retry::Never,
                    format!("cannot start reader thread: {error}"),
                )
            })?;

        self.child = Some(child);
        self.stdin = stdin;
        self.frames = Some(receiver);
        self.next_id = 1;
        self.degraded = None;
        Ok(())
    }

    fn child_alive(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) | Err(_) => {
                self.stop_child();
                false
            }
        }
    }

    fn stop_child(&mut self) {
        self.stdin = None;
        self.frames = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn write_frame(&mut self, frame: &str) -> Result<(), Failure> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(Failure::new(
                Class::WriteFailed,
                Retry::Never,
                "process stdin is closed",
            ));
        };
        let result = if self.spec.protocol >= 2 {
            ipc::write_length_frame_limit(stdin, frame, ipc::CALL_FRAME_MAX)
        } else {
            ipc::write_frame_limit(stdin, frame, ipc::CALL_FRAME_MAX)
        };
        result.map_err(|error| {
            Failure::new(
                Class::WriteFailed,
                Retry::Never,
                format!("cannot write to process: {error}"),
            )
        })
    }

    fn failure_from_error(error: &Value) -> Failure {
        let detail = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("process call failed")
            .to_string();
        let class = error
            .pointer("/data/class")
            .and_then(Value::as_str)
            .and_then(Class::from_str)
            .unwrap_or(Class::ImplementationError);
        Failure::new(class, class.default_retry(), detail)
    }

    fn mark_degraded(&mut self, failure: &Failure) {
        if matches!(
            failure.class,
            Class::ImplementationError | Class::Timeout | Class::WriteFailed
        ) {
            self.degraded = Some(failure.detail.clone());
        }
    }
}

impl Wire for ProcessWire {
    fn call(&mut self, invocation: Invocation) -> Result<Reply, Failure> {
        if let Some(class) = self.forced_failure {
            return Err(Failure::new(class, class.default_retry(), "forced failure"));
        }
        if invocation.cancel.is_cancelled() {
            return Err(Failure::new(
                Class::Cancelled,
                Retry::Never,
                "call was cancelled before dispatch",
            ));
        }
        self.ensure_started()?;

        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut params = json!({
            "name": invocation.capability.as_str(),
            "arguments": invocation.arguments,
        });
        if let Some(token) = &invocation.progress {
            params["_meta"] = json!({ "progressToken": token.as_str() });
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": params,
        })
        .to_string();
        let inline = crate::kernel::contract::inline_len(&invocation.arguments);
        if inline > self.contract.limits.max_inline || frame.len() > ipc::CALL_FRAME_MAX {
            return Err(Failure::new(
                Class::FrameTooLarge,
                Retry::Never,
                format!(
                    "request is {} bytes inline (frame {}), contract max_inline is {}",
                    inline,
                    frame.len(),
                    self.contract.limits.max_inline
                ),
            ));
        }
        self.write_frame(&frame)?;

        let started = Instant::now();
        loop {
            let remaining = invocation.timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                let _ = self.write_frame(
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/cancelled",
                        "params": { "requestId": id, "reason": "timeout" },
                    })
                    .to_string(),
                );
                let failure = Failure::new(
                    Class::Timeout,
                    Retry::After(Duration::from_secs(1)),
                    format!("call timed out after {:?}", invocation.timeout),
                );
                self.mark_degraded(&failure);
                return Err(failure);
            }
            let Some(frames) = self.frames.as_ref() else {
                return Err(Failure::new(
                    Class::NotRunning,
                    Retry::After(Duration::from_secs(1)),
                    "process stdout is closed",
                ));
            };
            let event = match frames.recv_timeout(remaining) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Failure::new(
                        Class::NotRunning,
                        Retry::After(Duration::from_secs(1)),
                        "process reader stopped",
                    ))
                }
            };
            match event {
                ipc::FrameEvent::Closed => {
                    self.stop_child();
                    return Err(Failure::new(
                        Class::NotRunning,
                        Retry::After(Duration::from_secs(1)),
                        "process exited before replying",
                    ));
                }
                ipc::FrameEvent::Frame(Err(ipc::FrameError::TooLarge { max })) => {
                    let failure = Failure::new(
                        Class::FrameTooLarge,
                        Retry::Never,
                        format!("process frame exceeds {max} bytes"),
                    );
                    self.stop_child();
                    return Err(failure);
                }
                ipc::FrameEvent::Frame(Err(error)) => {
                    self.stop_child();
                    return Err(Failure::new(
                        Class::NotRunning,
                        Retry::After(Duration::from_secs(1)),
                        format!("process frame error: {error}"),
                    ));
                }
                ipc::FrameEvent::Frame(Ok(line)) => {
                    let message: Value = match serde_json::from_str(&line) {
                        Ok(message) => message,
                        Err(_) => continue,
                    };
                    if message.get("method").is_some() {
                        // Progress/notifications are out-of-band; the call only
                        // waits for its own final response.
                        continue;
                    }
                    if message.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    if let Some(error) = message.get("error") {
                        let failure = Self::failure_from_error(error);
                        self.mark_degraded(&failure);
                        return Err(failure);
                    }
                    if let Some(result) = message.get("result") {
                        self.degraded = None;
                        return Ok(Reply::Value(result.clone()));
                    }
                    let failure = Failure::new(
                        Class::ImplementationError,
                        Retry::Never,
                        "process returned neither result nor error",
                    );
                    self.mark_degraded(&failure);
                    return Err(failure);
                }
            }
        }
    }

    fn lifecycle(&mut self, control: Control) -> Result<Settled, Failure> {
        match control {
            Control::Ensure => {
                self.ensure_started()?;
                Ok(Settled::Ready)
            }
            Control::Quiesce => {
                self.stop_child();
                Ok(Settled::Stopped)
            }
            Control::Probe => {
                if !self.child_alive() {
                    return Ok(Settled::Stopped);
                }
                Ok(match &self.degraded {
                    Some(reason) => Settled::Degraded(reason.clone()),
                    None => Settled::Ready,
                })
            }
        }
    }
}

impl Drop for ProcessWire {
    fn drop(&mut self) {
        self.stop_child();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::conformance::{assert_conforming, sample_contract};
    use crate::kernel::contract::{
        Capability, CapabilityName, Effect, Instance, Limits, Obligations, ServiceId,
    };
    use crate::kernel::failure::CallError;

    #[cfg(windows)]
    fn fixture() -> ProcessSpec {
        let path = std::env::temp_dir().join(format!(
            "svcd-process-fixture-{}-{}.ps1",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::write(
            &path,
            r#"while ($true) {
  $line = [Console]::ReadLine()
  if ($null -eq $line) { break }
  try { $obj = $line | ConvertFrom-Json } catch { continue }
  $id = $obj.id
  $name = [string]$obj.params.name
  if ($name -like '*fail.*') {
    $class = $name.Substring($name.LastIndexOf('.') + 1)
    $out = '{"jsonrpc":"2.0","id":' + $id + ',"error":{"code":-32000,"message":"' + $class + '","data":{"class":"' + $class + '"}}}'
  } else {
    $out = '{"jsonrpc":"2.0","id":' + $id + ',"result":{"ok":true}}'
  }
  [Console]::Out.WriteLine($out)
  [Console]::Out.Flush()
}
"#,
        )
        .expect("fixture script");
        ProcessSpec {
            path: "powershell.exe".into(),
            args: vec![
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                path.to_string_lossy().to_string(),
            ],
            env: BTreeMap::new(),
            protocol: 1,
        }
    }

    #[cfg(unix)]
    fn fixture() -> ProcessSpec {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "svcd-process-fixture-{}-{}.sh",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::write(
            &path,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  name=$(printf '%s' "$line" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
  case "$name" in
    *fail.*)
      class=\${name##*.}
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"%s","data":{"class":"%s"}}}\n' "$id" "$class" "$class"
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
      ;;
  esac
done
"#,
        )
        .expect("fixture script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fixture");
        ProcessSpec {
            path: "/bin/sh".into(),
            args: vec![path.to_string_lossy().to_string()],
            env: BTreeMap::new(),
            protocol: 1,
        }
    }

    fn contract() -> Contract {
        // The process transport has a 1 MiB *frame* cap; keep the inline limit
        // below it so the exact-boundary check fits the envelope.
        sample_contract(Limits {
            max_inline: 256 * 1024,
            ..Limits::default()
        })
    }

    fn failing(class: Class) -> Instance {
        Instance::new(
            contract(),
            Box::new(ProcessWire::new(contract(), fixture()).forced_failure(class)),
        )
        .expect("valid contract")
    }

    fn normal() -> Instance {
        Instance::new(contract(), Box::new(ProcessWire::new(contract(), fixture())))
            .expect("valid contract")
    }

    #[test]
    fn the_process_adapter_conforms() {
        assert_conforming(&contract(), normal, failing);
    }

    #[test]
    fn the_process_adapter_maps_a_plugin_error_class() {
        let contract = Contract::new(
            ServiceId::new("process"),
            "1.0.0",
            vec![Capability {
                name: CapabilityName::new("fail.busy"),
                desc: String::new(),
                input_schema: None,
                effect: Effect::Write,
                idempotent: false,
            }],
            Obligations::default(),
            Class::ALL.to_vec(),
            Limits::default(),
        );
        let mut instance = Instance::new(
            contract.clone(),
            Box::new(ProcessWire::new(contract, fixture())),
        )
        .expect("valid contract");
        let error = instance
            .call(Invocation::new(
                "fail.busy",
                json!({}),
                Duration::from_secs(5),
            ))
            .expect_err("script returns an error");
        match error {
            CallError::Failed(failure) => assert_eq!(failure.class, Class::Busy),
            other => panic!("expected a classified failure, got {other:?}"),
        }
    }

    #[test]
    fn the_process_tag_is_registered_at_the_test_assembly_point() {
        use crate::kernel::contract::Launchers;
        let spec = fixture();
        let binding = json!({
            "path": spec.path,
            "args": spec.args,
            "protocol": spec.protocol,
        });
        let mut launchers = Launchers::default();
        launchers.register("process", Box::new(ProcessLauncher));
        let mut instance = match launchers.launch(
            contract(),
            &ImplRef {
                tag: "process".into(),
                binding,
            },
        ) {
            Ok(instance) => instance,
            Err(error) => panic!("the process tag must launch: {error:?}"),
        };
        let reply = instance
            .call(Invocation::new(
                "status",
                json!({}),
                Duration::from_secs(5),
            ))
            .expect("process call");
        match reply {
            Reply::Value(value) => assert_eq!(value["ok"], true),
            other => panic!("expected a value reply, got {other:?}"),
        }
    }

    #[test]
    fn a_bad_binding_is_a_launch_error() {
        let launcher = ProcessLauncher;
        match launcher.launch(
            &contract(),
            &ImplRef {
                tag: "process".into(),
                binding: json!({}),
            },
        ) {
            Err(error) => assert_eq!(error.class, Class::MissingDependency),
            Ok(_) => panic!("a binding without a path must not launch"),
        }
    }
}

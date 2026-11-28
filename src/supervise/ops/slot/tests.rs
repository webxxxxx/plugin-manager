#![cfg(test)]

use super::calls::{test_reply_sink, PendingCall};
use std::collections::BTreeMap;

use crate::kernel::contract::Wire;
use crate::kernel::failure::Class;
use crate::kernel::registry::{Hook, Interpreter};
use std::sync::{Arc, Mutex};

use super::*;
use std::time::{Duration, Instant};

fn exec() -> Exec {
    Exec {
        path: "/bin/true".into(),
        args: Vec::new(),
        env: BTreeMap::new(),
        ready: Ready::Notify,
        interpreter: None,
    }
}

/// Every slot test owns its tree; nothing here reads or writes the
/// process environment.
fn test_paths() -> Arc<Paths> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = crate::platform::test_support::temp_dir().join(format!(
        "svcd-slot-unit-{}-{}",
        std::process::id(),
        nanos
    ));
    let paths = Paths::under(&root);
    std::fs::create_dir_all(&paths.logs_dir).expect("logs");
    Arc::new(paths)
}

fn test_limits() -> Arc<crate::platform::limits::Limits> {
    crate::platform::test_support::limits()
}

fn slot() -> Slot {
    Slot::new(
        "demo".into(),
        exec(),
        None,
        Restart::OnFailure,
        test_paths(),
        test_limits(),
    )
}

fn queue(slot: &mut Slot, id: u64, deadline: u64) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    slot.pending.insert(
        id,
        PendingCall {
            capability: "echo".into(),
            deadline,
            progress_token: None,
            trace: None,
            client: Some(crate::wire::control::channel::Reply::plain(tx.clone())),
            sink: test_reply_sink(
                crate::wire::control::channel::Reply::plain(tx),
                Some(json!(100 + id)),
            ),
        },
    );
    rx
}

fn elapsed(started: &Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn wait_response(slot: &mut Slot, rx: &Receiver<String>, started: &Instant) -> rpc::Response {
    loop {
        slot.pump(elapsed(started));
        if let Ok(line) = rx.try_recv() {
            return serde_json::from_str(&line).expect("response json");
        }
        assert!(started.elapsed() < Duration::from_secs(5), "no response");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// --- 共享 fixture -----------------------------------------------------------
//
// 这些 helper 用测试二进制本身当子进程：跑一个命名的 fixture 测试，退出状态在
// 每个平台都一样，不需要 shell。`current_exe` 让 fixture 保持平台中立。

/// A live process standing in for the supervised `exec`; the slot holds it, so
/// `check_exit` can reap it like a real child.
fn dummy_process() -> std::process::Child {
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .arg("hook_fixture_sleeps")
        .env("SVCD_HOOK_FIXTURE", "1");
    command.spawn().expect("dummy process")
}

fn reap_exec(slot: &mut Slot) {
    if let Some(mut child) = slot.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

// 每个子模块自己写 `#![cfg(test)]`：相对父模块的测试门它是冗余的，但 dep-lint 只看
// 文件内的内层属性，缺了它会把测试里的文案断言当成生产代码的分支（#344）。
mod calls;
mod hooks;
mod lifecycle;
mod readiness;
mod spawn;
mod wire;

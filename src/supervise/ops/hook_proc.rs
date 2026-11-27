//! 短命钩子进程的公共部分：一个临时输出文件，以及「等到退出 / 超时 / 取消」。
//!
//! 两个使用者是 `probe.rs` 的 `status` 探针与 `slot/call_hook.rs` 的能力钩子：都是
//! 「起一次进程、把 stdout 收进临时文件、等它退出」。stdout 落文件而不是接管道，是
//! 因为钩子若留下一个占着 stdout 的孙子进程，那条管道会逼出一个一直陪着的读线程；
//! 文件没有这个代价，读的时候也就知道自己读到哪儿为止。
//!
//! 临时文件一律 `0o600`：钩子的参数与结果可能是口令、订阅地址或代理形态，不能落到
//! 别的用户能读的地方。

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant, SystemTime};

use crate::kernel::contract::CancelToken;
use crate::platform::paths::Paths;

/// 一次钩子运行独占的临时文件；`Drop` 时删掉，任何早退路径都不留垃圾。
pub(crate) struct Capture {
    path: PathBuf,
}

impl Capture {
    pub(crate) fn new(id: &str, tag: &str, paths: &Paths) -> std::io::Result<Self> {
        let directory = &paths.temp_dir;
        fs::create_dir_all(directory)?;
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Ok(Capture {
            path: directory.join(format!(
                "{}-{}-{}-{}.out",
                tag,
                id,
                std::process::id(),
                nanos
            )),
        })
    }

    /// 每次都截断：拿到的总是空文件，上一次运行的字节不会漏进这次的结果。
    pub(crate) fn open(&self) -> std::io::Result<File> {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&self.path)
    }

    /// 读成文本，最多 `limit` 字节；第二个值表示文件比 `limit` 还长（读 `limit + 1`
    /// 字节判定，不把整个文件读进内存）。
    pub(crate) fn read(&self, limit: usize) -> (String, bool) {
        let Ok(file) = File::open(&self.path) else {
            return (String::new(), false);
        };
        let mut bytes = Vec::new();
        let overflow = match file.take(limit as u64 + 1).read_to_end(&mut bytes) {
            Ok(_) => bytes.len() > limit,
            Err(_) => false,
        };
        if overflow {
            bytes.truncate(limit);
        }
        (String::from_utf8_lossy(&bytes).to_string(), overflow)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 一次等待没有等到进程自己正常退出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unfinished {
    Cancelled,
    TimedOut,
    Failed(String),
}

/// 等 `child` 退出；超时或取消就把整棵树杀掉再返回，钩子的子孙不许活过这次调用。
///
/// `poll` 在每次等待之间跑一次：钩子运行时监督循环还得照看插件 stdio，不能让一次
/// 同步等待把管道卡住。
pub(crate) fn wait(
    child: &mut Child,
    timeout: Duration,
    cancel: Option<&CancelToken>,
    poll: &mut dyn FnMut(),
    step: Duration,
) -> Result<ExitStatus, Unfinished> {
    let deadline = Instant::now() + timeout;
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => break Err(Unfinished::Failed(error.to_string())),
        }
        if cancel.is_some_and(CancelToken::is_cancelled) {
            break Err(Unfinished::Cancelled);
        }
        if Instant::now() >= deadline {
            break Err(Unfinished::TimedOut);
        }
        poll();
        std::thread::sleep(step);
    };
    if outcome.is_err() {
        // 钩子在自己的进程组里：停整棵树，别只停 leader。
        crate::platform::host::process::kill_now(child);
        let _ = child.wait();
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        Paths::under(&crate::platform::test_support::temp_dir().join(format!(
            "svcd-hook-proc-{}-{}",
            std::process::id(),
            nanos
        )))
    }

    #[test]
    fn a_capture_refuses_to_read_past_its_limit() {
        let capture = Capture::new("demo", "read", &paths()).expect("capture");
        std::fs::write(&capture.path, "0123456789").expect("write");
        let (text, overflow) = capture.read(4);
        assert_eq!(text, "0123");
        assert!(overflow, "a longer file is reported, not silently cut");
        let (whole, overflow) = capture.read(10);
        assert_eq!(whole, "0123456789");
        assert!(!overflow);
    }

    #[test]
    fn a_capture_cleans_up_after_itself() {
        let capture = Capture::new("demo", "cleanup", &paths()).expect("capture");
        let path = capture.path.clone();
        capture.open().expect("open");
        assert!(path.exists());
        drop(capture);
        assert!(!path.exists(), "the temp file goes with the capture");
    }
}

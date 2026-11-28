//! 调用号：一次管理请求的身份。
//!
//! 只在进程内有意义。svcd 收到一条请求时分配，重启从 1 重来；不落盘、不进配置、
//! 不做幂等。它存在的理由只有一个：让一次安装（或一次能力调用）在错误与日志里
//! 能被同一个号指出来（#343）。

use std::fmt;

/// One management request, as svcd counted it.
///
/// A restart starts again at 1: the number names a request inside one process,
/// not across time. The `starting` line in `svcd.log` is the boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(transparent)]
pub struct CallId(u64);

impl CallId {
    /// The first number a boot hands out.
    pub const FIRST: CallId = CallId(1);

    /// The next number. Saturating: a daemon that somehow ran through `u64`
    /// must not wrap back to 1 and hand out an identity it already used.
    pub const fn next(self) -> CallId {
        CallId(self.0.saturating_add(1))
    }

    /// The number a peer sent back, e.g. the `call` key in `error.data`. Only
    /// the HTTP face reads it back, so a build without that face sees it unused.
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    pub const fn from_raw(value: u64) -> CallId {
        CallId(value)
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::CallId;

    #[test]
    fn a_call_id_prints_and_serializes_as_the_bare_number() {
        let call = CallId::FIRST.next();
        assert_eq!(call.to_string(), "2");
        assert_eq!(serde_json::to_string(&call).expect("json"), "2");
    }

    #[test]
    fn numbers_start_at_one_and_saturate_instead_of_wrapping() {
        assert_eq!(CallId::FIRST.to_string(), "1");
        let last = CallId(u64::MAX);
        assert_eq!(last.next(), last);
    }
}

//! S9 准入：一个请求能不能进来、以什么身份进来。
//!
//! 四个 adapter 全部在这里收口：
//!
//! - **本机免认证**：LAN 状态说「这个监听不需要认证」，`admit` 直接给
//!   [`AuthKind::None`]；
//! - **bearer**：`Authorization: Bearer <口令>` 与 `auth_passwd` 哈希比对；
//! - **cookie 会话**：`svcd_session` / `svcd_api` 里的会话值仍在有效期内；
//! - **plugin token**：短期、绑定单个插件 id，走 [`Admission::admit_plugin`]。
//!
//! 服务器（`server/http`）与回退面（`api.rs`）都只问 [`Admission`]，不再各自
//! `match lan.authenticate`；`lan` 退回「会话与限速」。控制通道的端点 ACL 是另一条
//! 准入路（ADR-0014），不在这里。
//!
//! 拒绝按**类别**返回，方便调用方投影成同一种 HTTP 错误；文案只用于展示。

use std::sync::Mutex;

use super::lan::{AuthKind, Lan};

/// 一次准入为什么被拒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rejected {
    /// 没有任何凭证匹配上：HTTP 层是 401。
    Unauthorized,
    /// LAN 状态读不出来（锁中毒）：按失败关闭处理，HTTP 层是 500。
    Unavailable,
}

/// 一次请求的准入判断。持有共享的 LAN 状态，不拥有它。
pub(crate) struct Admission<'a> {
    lan: &'a Mutex<Lan>,
}

impl<'a> Admission<'a> {
    pub(crate) fn new(lan: &'a Mutex<Lan>) -> Self {
        Admission { lan }
    }

    /// 本机 / 公开路由的准入结果。它是一条真实分支（LAN 的 `require_auth`
    /// 为假），不是调用方的隐式早退。
    pub(crate) fn local() -> AuthKind {
        AuthKind::None
    }

    /// bearer 与 cookie 两条 adapter 的判定入口。
    pub(crate) fn admit(&self, headers: &[(String, String)]) -> Result<AuthKind, Rejected> {
        let mut state = self.lan.lock().map_err(|_| Rejected::Unavailable)?;
        state
            .authenticate(headers)
            .map_err(|()| Rejected::Unauthorized)
    }

    /// plugin token adapter：只对铸出它的那个插件 id 有效，锁中毒按拒绝处理。
    ///
    /// The plugin UI is an HTTP surface, so this adapter and the free function
    /// below are only driven with that feature (#326).
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    pub(crate) fn admit_plugin(&self, token: &str, plugin: &str) -> bool {
        match self.lan.lock() {
            Ok(mut state) => state.validate_plugin_token(token, plugin),
            Err(_) => false,
        }
    }
}

/// 对外面的 plugin token 检查。face 只持有共享的 LAN 状态，所以保留自由函数，
/// 内部仍走同一个 [`Admission`]。
#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub(crate) fn plugin_token_valid(lan: &Mutex<Lan>, token: &str, plugin: &str) -> bool {
    Admission::new(lan).admit_plugin(token, plugin)
}

#[cfg(test)]
mod tests {
    use super::{Admission, Rejected};
    use crate::server::auth::lan::{AuthKind, Lan, COOKIE_SESSION};
    use std::net::SocketAddr;
    use std::sync::Mutex;

    fn addr() -> SocketAddr {
        "127.0.0.1:8787".parse().expect("addr")
    }

    fn lan(require_auth: bool) -> Mutex<Lan> {
        let hash = crate::platform::auth::hash_password("secret").expect("hash");
        Mutex::new(Lan::new(addr(), addr(), require_auth, Some(hash)))
    }

    fn bearer(token: &str) -> Vec<(String, String)> {
        vec![("authorization".to_string(), format!("Bearer {token}"))]
    }

    #[test]
    fn the_local_adapter_admits_without_credentials() {
        let lan = lan(false);
        let admission = Admission::new(&lan);

        assert_eq!(admission.admit(&[]), Ok(AuthKind::None));
        // Credentials are irrelevant when the bind does not require auth.
        assert_eq!(admission.admit(&bearer("whatever")), Ok(AuthKind::None));
        assert_eq!(Admission::local(), AuthKind::None);
    }

    #[test]
    fn the_bearer_adapter_checks_the_password_hash() {
        let lan = lan(true);
        let admission = Admission::new(&lan);

        assert_eq!(admission.admit(&bearer("secret")), Ok(AuthKind::Bearer));
        assert_eq!(
            admission.admit(&bearer("wrong")),
            Err(Rejected::Unauthorized)
        );
        assert_eq!(admission.admit(&[]), Err(Rejected::Unauthorized));
    }

    #[test]
    fn the_cookie_adapter_admits_a_live_session_and_rejects_a_logged_out_one() {
        let lan = lan(true);
        let admission = Admission::new(&lan);

        let session = lan.lock().expect("lan").login("secret").expect("login");
        let cookie = vec![("cookie".to_string(), format!("{COOKIE_SESSION}={session}"))];
        assert_eq!(admission.admit(&cookie), Ok(AuthKind::Cookie));

        lan.lock().expect("lan").logout(&cookie);
        assert_eq!(admission.admit(&cookie), Err(Rejected::Unauthorized));
    }

    #[test]
    fn the_plugin_token_adapter_is_bound_to_one_plugin() {
        let lan = lan(true);
        let admission = Admission::new(&lan);
        let token = lan.lock().expect("lan").issue_plugin_token("alpha");

        assert!(admission.admit_plugin(&token, "alpha"));
        assert!(!admission.admit_plugin(&token, "beta"));
        assert!(!admission.admit_plugin("no-such-token", "alpha"));

        // The free function the face uses answers the same way.
        assert!(super::plugin_token_valid(&lan, &token, "alpha"));
        assert!(!super::plugin_token_valid(&lan, &token, "beta"));
    }

    #[test]
    fn a_poisoned_lan_state_fails_closed() {
        let lan = lan(true);
        let admission = Admission::new(&lan);
        // Poison the lock the same way a panicking holder would.
        let _ = std::panic::catch_unwind(|| {
            let _guard = lan.lock().expect("lan");
            panic!("poison the admission state");
        });

        assert_eq!(
            admission.admit(&bearer("secret")),
            Err(Rejected::Unavailable)
        );
        assert!(!admission.admit_plugin("token", "alpha"));
        assert!(!super::plugin_token_valid(&lan, "token", "alpha"));
    }
}

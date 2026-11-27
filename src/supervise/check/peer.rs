//! 通道对端：调用方的话要说进它的 stdio 通道。
//!
//! 通知型插件用 JSON-RPC 走这条通道，protocol 3 的插件把原始传输放在上面。
//! 通道跟着进程走，进程退出通道就没了（见 GLOSSARY「通道对端」）。定义加载与
//! 运行期判定都从这里读同一个结论，避免两处各自解释「什么是通道对端」。

use crate::kernel::registry::Ready;
use crate::wire::contract::RAW_PROTOCOL;

/// 调用方是否依赖这个服务的 stdio 通道活着。`ready: port` 不建通道（宿主只探
/// 端口），所以它不是通道对端——即使 protocol 3 也不：端口就绪与 raw 传输不能
/// 同时成立（#337）。
pub(crate) fn is_channel_peer(ready: Ready, protocol: u32) -> bool {
    match ready {
        Ready::Port(_) => false,
        Ready::Notify => true,
        Ready::Exec => protocol == RAW_PROTOCOL,
    }
}

#[cfg(test)]
mod tests {
    use super::is_channel_peer;
    use crate::kernel::registry::Ready;
    use crate::wire::contract::RAW_PROTOCOL;

    #[test]
    fn notify_ready_and_raw_transport_services_are_channel_peers() {
        assert!(is_channel_peer(Ready::Notify, 1));
        assert!(is_channel_peer(Ready::Notify, 2));
        assert!(is_channel_peer(Ready::Exec, RAW_PROTOCOL));
    }

    #[test]
    fn fire_and_forget_services_own_no_channel() {
        assert!(!is_channel_peer(Ready::Exec, 1));
        assert!(!is_channel_peer(Ready::Exec, 2));
    }

    #[test]
    fn a_port_ready_service_owns_no_channel_either() {
        // Readiness is a connection, not a message: nothing speaks on stdio, so
        // a port-ready service is not a channel peer even at protocol 3 (#337).
        let addr = "127.0.0.1:7000".parse().expect("addr");
        assert!(!is_channel_peer(Ready::Port(addr), 1));
        assert!(!is_channel_peer(Ready::Port(addr), 2));
        assert!(!is_channel_peer(Ready::Port(addr), RAW_PROTOCOL));
    }
}

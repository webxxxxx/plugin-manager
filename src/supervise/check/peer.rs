//! 通道对端：调用方的话要说进它的 stdio 通道。
//!
//! 通知型插件用 JSON-RPC 走这条通道，protocol 3 的插件把原始传输放在上面。
//! 通道跟着进程走，进程退出通道就没了（见 GLOSSARY「通道对端」）。定义加载与
//! 运行期判定都从这里读同一个结论，避免两处各自解释「什么是通道对端」。

use crate::kernel::registry::Ready;
use crate::wire::contract::RAW_PROTOCOL;

/// 调用方是否依赖这个服务的 stdio 通道活着。
pub(crate) fn is_channel_peer(ready: Ready, protocol: u32) -> bool {
    ready == Ready::Notify || protocol == RAW_PROTOCOL
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
}

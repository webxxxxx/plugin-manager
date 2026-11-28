//! `exec.ready` 的三种取值与其探针地址。
//!
//! 加载期把定义里的字符串解析成运行时模式（`#126`），运行时不再读定义；
//! 三个 loader（core 定义、manifest 校验、manifest 扫描）共用同一个入口。

use std::net::SocketAddr;

/// How the host decides that one spawn is ready. The default is the protocol
/// channel: a process that never answers is never ready.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Ready {
    /// The first legal response on the protocol channel (#126).
    #[default]
    Notify,
    /// The spawn itself.
    Exec,
    /// A TCP connect to this address succeeds. Stronger than `Exec` (the
    /// process accepts connections) and weaker than `Notify` (it answers no
    /// message): the host probes instead of trusting the spawn.
    Port(SocketAddr),
}

/// The probe address for `ready: port`: an explicit port and an optional
/// address (loopback by default). Load-time validation, so the runtime never
/// parses a definition again.
pub fn probe_address(port: Option<u16>, addr: Option<&str>) -> Result<SocketAddr, String> {
    let port = port.ok_or("port is required when ready = port")?;
    if port == 0 {
        return Err("port 0 is not a probe target".into());
    }
    let text = addr.unwrap_or("127.0.0.1");
    let ip: std::net::IpAddr = text
        .parse()
        .map_err(|_| format!("probe address {:?} is not an IP address", text))?;
    Ok(SocketAddr::new(ip, port))
}

/// Why `exec.ready` (plus its probe fields) could not become a [`Ready`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadyError {
    /// `exec.ready` names a mode this build does not know.
    UnknownMode(String),
    /// `exec.port` / `exec.probe_addr` without `exec.ready = "port"`.
    ProbeWithoutPortMode,
    /// `exec.ready = "port"` but the probe target is missing or invalid.
    Probe(String),
}

impl std::fmt::Display for ReadyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadyError::UnknownMode(mode) => write!(
                f,
                "exec.ready \"{}\" is invalid; expected notify, exec or port",
                mode
            ),
            ReadyError::ProbeWithoutPortMode => write!(
                f,
                "exec.port / exec.probe_addr are only used with exec.ready = \"port\""
            ),
            ReadyError::Probe(reason) => write!(f, "exec: {}", reason),
        }
    }
}

/// Read `exec.ready` and its probe fields into the runtime mode. The three
/// loaders (core definition, manifest verification, manifest scan) share this
/// so a new mode cannot land on one path only.
pub fn ready_from(
    ready: Option<&str>,
    port: Option<u16>,
    probe_addr: Option<&str>,
) -> Result<Ready, ReadyError> {
    let mode = match ready {
        None | Some("notify") => Ready::Notify,
        Some("exec") => Ready::Exec,
        Some("port") => Ready::Port(probe_address(port, probe_addr).map_err(ReadyError::Probe)?),
        Some(other) => return Err(ReadyError::UnknownMode(other.to_string())),
    };
    if !matches!(mode, Ready::Port(_)) && (port.is_some() || probe_addr.is_some()) {
        return Err(ReadyError::ProbeWithoutPortMode);
    }
    Ok(mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_address_needs_a_port_and_defaults_to_loopback() {
        assert_eq!(
            probe_address(Some(7893), None).expect("default"),
            "127.0.0.1:7893".parse().expect("addr")
        );
        assert_eq!(
            probe_address(Some(7893), Some("::1")).expect("v6"),
            "[::1]:7893".parse().expect("addr")
        );
        // No port is not a probe target, and port 0 is not either.
        assert!(probe_address(None, None).is_err());
        assert!(probe_address(Some(0), None).is_err());
        // A host name is not resolved at load time; the definition must be
        // explicit.
        assert!(probe_address(Some(7893), Some("localhost")).is_err());
    }
}

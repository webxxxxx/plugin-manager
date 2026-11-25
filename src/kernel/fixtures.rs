//! Shared test fixtures for the kernel's own unit tests and the CLI renderers.
//!
//! Test-only, like `conformance` and `memory`. It lives in `kernel` rather than
//! a crate-level helper so the dependency edge stays downward: `cli` may use a
//! kernel fixture, while `kernel` must not reach into a test helper module that
//! itself imports the kernel.

use crate::kernel::registry::{contract_for, Carrier, Exec, Kind, Ready, Restart, Service, State};

/// A service definition carrying every field the status projection reads, so
/// the shape tests pin real values instead of nulls.
pub(crate) fn test_service() -> Service {
    Service {
        id: "demo".into(),
        kind: Kind::Core,
        version: "1.2.3".into(),
        protocol: 1,
        desc: "demo service".into(),
        display_name: None,
        deps: vec!["base".into()],
        contract: contract_for(
            "demo",
            "1.2.3",
            Vec::new(),
            Carrier::Slot,
            &crate::platform::limits::test_default(),
        ),
        impl_ref: None,
        exec: Some(Exec {
            path: "/bin/demo".into(),
            args: Vec::new(),
            env: Default::default(),
            ready: Ready::Exec,
            interpreter: None,
        }),
        lifecycle: None,
        enabled: true,
        restart: Restart::OnFailure,
        remain_after_exit: false,
        compat: None,
        state: State::Running,
        pid: Some(42),
        since: Some(1_700_000_000),
        ready_since: Some(1_700_000_002),
        next_at: None,
        failures: 0,
        last_exit: Some(3),
        config: Some("/var/lib/svcd/demo/config".into()),
        install_dir: None,
    }
}

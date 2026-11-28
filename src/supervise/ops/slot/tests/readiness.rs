#![cfg(test)]
//! 就绪判定：端口探针在什么条件下 settle（`ready = "port"`）。

use super::*;

#[test]
fn a_port_probe_settles_ready_when_the_address_accepts() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let addr = listener.local_addr().expect("addr");
    let mut slot = slot();
    let pid = 4242;
    slot.machine.state = State::Starting;
    slot.machine.pid = Some(pid);
    slot.start_port_probe(pid, addr);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut now = 0u64;
    loop {
        now += 20;
        slot.pump(now);
        if slot.machine.state == State::Running {
            break;
        }
        assert!(Instant::now() < deadline, "the probe never settled a Ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(slot.port.is_none(), "a settled probe is dropped");
}

#[test]
fn a_port_probe_keeps_waiting_while_nothing_accepts() {
    // The listener is dropped before the probe starts: the address refuses
    // connections, so the instance must not be reported ready.
    let addr = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        listener.local_addr().expect("addr")
    };
    let mut slot = slot();
    slot.machine.state = State::Starting;
    slot.machine.pid = Some(7);
    slot.start_port_probe(7, addr);

    for now in (20..600).step_by(20) {
        slot.pump(now);
    }

    assert_eq!(slot.machine.state, State::Starting);
    assert!(slot.port.is_some(), "the probe is still waiting");
}

#[test]
fn a_port_probe_from_an_older_instance_never_settles_ready() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let addr = listener.local_addr().expect("addr");
    let mut slot = slot();
    slot.machine.state = State::Starting;
    slot.machine.pid = Some(7);
    slot.start_port_probe(7, addr);

    // The instance is gone before the probe connects; the verdict belongs to it.
    slot.machine.state = State::Backoff;
    slot.machine.pid = None;
    for now in (20..400).step_by(20) {
        slot.pump(now);
    }
    assert_eq!(slot.machine.state, State::Backoff);
    assert!(slot.port.is_none(), "the stale probe is dropped");
}

#[test]
fn a_port_that_never_accepts_names_the_address_and_is_a_timeout() {
    let addr = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        listener.local_addr().expect("addr")
    };
    let mut slot = slot();
    slot.machine.state = State::Starting;
    let child = dummy_process();
    let pid = child.id();
    slot.machine.pid = Some(pid);
    slot.child = Some(child);
    slot.start_deadline = Some(100);
    slot.start_port_probe(pid, addr);

    for now in (20..=100).step_by(20) {
        slot.pump(now);
    }

    let failure = slot.take_start_failure().expect("a reason for the start");
    assert!(failure.timeout, "a window that ran out is a 504");
    assert!(
        failure.reason.contains(&addr.to_string()),
        "{}",
        failure.reason
    );
    reap_exec(&mut slot);
}

#[test]
fn a_port_ready_instance_that_dies_before_listening_names_the_address() {
    let addr = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        listener.local_addr().expect("addr")
    };
    let mut slot = slot();
    slot.machine.state = State::Starting;
    let child = dummy_process();
    let pid = child.id();
    slot.machine.pid = Some(pid);
    slot.child = Some(child);
    slot.start_port_probe(pid, addr);

    slot.apply(Event::Exited { pid, code: Some(1) }, 40);

    let failure = slot.take_start_failure().expect("a reason for the start");
    assert!(!failure.timeout, "an exit is not a timeout");
    assert!(
        failure.reason.contains(&addr.to_string()),
        "{}",
        failure.reason
    );
    assert_eq!(slot.machine.state, State::Backoff);
    reap_exec(&mut slot);
}

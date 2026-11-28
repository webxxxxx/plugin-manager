#![cfg(test)]
//! 槽的 wire 面：channel peer、调用面 conformance、生命周期归属。

use super::*;

#[test]
fn notify_and_raw_transport_services_are_channel_peers() {
    // #183: a channel peer must stay alive for callers; a clean exit is
    // death, not completion. Protocol 1/2 exec services own no channel.
    let mut slot = slot();
    assert!(
        slot.is_channel_peer(),
        "a notify service must stay reachable"
    );

    let mut fire_and_forget = exec();
    fire_and_forget.ready = Ready::Exec;
    slot.set_exec(fire_and_forget);
    assert!(
        !slot.is_channel_peer(),
        "exec protocol 1 is fire-and-forget"
    );

    slot.set_protocol(3);
    assert!(
        slot.is_channel_peer(),
        "protocol 3 carries the raw transport"
    );
}

/// The conformance suite lives in the kernel, so the slot-backed adapter's
/// conformance test lives here: a domain module may reference the kernel,
/// the reverse edge would be a layering violation (tools/dep-lint.mjs).
fn conforming_wire(
    respond: impl Fn(&Invocation) -> Result<Value, Failure> + Send + 'static,
) -> crate::kernel::contract::Instance {
    let contract =
        crate::kernel::conformance::sample_contract(crate::kernel::contract::Limits::default());
    crate::kernel::contract::Instance::new(
        contract,
        Box::new(SlotWire::new(SlotHandle::scripted(respond), None)),
    )
    .expect("valid contract")
}

#[test]
fn the_slot_wire_conforms_on_the_call_surface_and_delegates_lifecycle() {
    let contract =
        crate::kernel::conformance::sample_contract(crate::kernel::contract::Limits::default());
    crate::kernel::conformance::assert_call_conforming(
        &contract,
        || conforming_wire(|_| Ok(json!({"ok": true}))),
        |class| {
            conforming_wire(move |_| Err(Failure::new(class, Retry::Never, "injected failure")))
        },
    );
    crate::kernel::conformance::assert_delegates_lifecycle(&contract, || {
        conforming_wire(|_| Ok(json!({})))
    });
}

#[test]
fn the_slot_wire_cannot_pass_the_owned_lifecycle_suite() {
    // 钉住「为什么套件要按入口拆」：槽的生命周期属于 daemon（ADR-0015），
    // SlotWire 只能拒绝 Ensure / Quiesce，所以它永远过不了生命周期那一套。
    let contract =
        crate::kernel::conformance::sample_contract(crate::kernel::contract::Limits::default());
    let violations = crate::kernel::conformance::check(
        &contract,
        || conforming_wire(|_| Ok(json!({}))),
        |_| conforming_wire(|_| Ok(json!({}))),
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("Ensure")),
        "the slot wire must fail the owned lifecycle suite: {violations:#?}"
    );
}

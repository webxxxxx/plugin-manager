#![cfg(test)]

use super::*;
use crate::platform::resources::{BudgetSource, MIN_BUDGET};
use crate::platform::settings::Settings;

const KIB: u64 = 1024;
const GIB: u64 = 1024 * MIB;

fn derive_for(budget: u64, workload: Option<Workload>, settings: Settings) -> Derived {
    let settings = Settings {
        mem_budget_bytes: Some(budget),
        workload,
        ..settings
    };
    derive_with(&settings, &ResourceProfile::default())
}

/// A percentage is resolved from the probe, so what the ceiling tightens is
/// the resolved number — and the report still names the key the operator wrote.
#[test]
fn a_capped_percentage_is_reported_under_the_key_it_was_written_as() {
    let settings = Settings {
        mem_budget_percent: Some(90),
        ..Settings::default()
    };
    let profile = ResourceProfile {
        available_memory: Some(64 * GIB),
        ..ResourceProfile::default()
    };

    let derived = derive_with(&settings, &profile);

    // 90% of 64 GiB is 57.6 GiB; every level caps it.
    assert_eq!(derived.report.budget.raw, BUDGET_CAP);
    assert_eq!(derived.report.budget.source, BudgetSource::Configured);
    let note = derived
        .report
        .notes
        .iter()
        .find(|note| note.key == "mem_budget_percent")
        .expect("a build-cap row for the percentage");
    assert_eq!(note.configured, 90);
    assert_eq!(note.effective, BUDGET_CAP);
    assert_eq!(note.reason, OverrideReason::BuildCap);
}
/// A shape's reference budget, clipped to what this build may spend. A
/// `mem_low` binary cannot express the streaming shape's 2 GiB reference,
/// so its tests compare against the budget the binary really gets.
fn reference_budget(workload: Workload) -> u64 {
    workload.reference_budget().min(BUDGET_CAP)
}

fn at_reference(workload: Workload) -> Derived {
    derive_for(
        reference_budget(workload),
        Some(workload),
        Settings::default(),
    )
}

fn note<'a>(report: &'a ResourceReport, key: &str) -> Option<&'a Override> {
    report.notes.iter().find(|note| note.key == key)
}

#[test]
fn every_shape_fits_its_reference_budget() {
    for workload in Workload::ALL {
        let budget = reference_budget(workload);
        let derived = at_reference(workload);
        assert_eq!(
            derived.report.refusal,
            None,
            "{} refuses at its reference budget",
            workload.as_str()
        );
        assert!(
            derived.limits.worst_case_bytes() <= budget,
            "{}: worst case {} > budget {}",
            workload.as_str(),
            derived.limits.worst_case_bytes(),
            budget
        );
        assert!(
            derived.report.notes.is_empty(),
            "{} rewrites its own defaults: {:?}",
            workload.as_str(),
            derived.report.notes
        );
    }
}

#[test]
fn every_shape_fits_the_floor_budget() {
    // The smallest budget any process may plan for must still produce a
    // runnable configuration for every shape.
    for workload in Workload::ALL {
        let derived = derive_for(MIN_BUDGET, Some(workload), Settings::default());
        assert_eq!(derived.report.refusal, None, "{}", workload.as_str());
        assert!(derived.limits.worst_case_bytes() <= MIN_BUDGET);
        assert!(derived.limits.max_connections >= 1);
    }
}

#[test]
fn the_shapes_never_collapse_into_one_result() {
    // The old absolute table was flattened by the invariant: after the
    // same shrink, the three levels looked alike. Connections are now solved
    // against the budget instead of being a share of it, so the axis is the
    // resident body: the smaller the body, the more slots the same budget
    // holds. The shapes stay pairwise distinct and tiny always leads.
    for budget in [64 * MIB, 128 * MIB, 256 * MIB, 512 * MIB, GIB, 2 * GIB] {
        let tiny = derive_for(budget, Some(Workload::Tiny), Settings::default());
        let interactive = derive_for(budget, Some(Workload::Interactive), Settings::default());
        let streaming = derive_for(budget, Some(Workload::Streaming), Settings::default());

        assert!(
            tiny.limits.max_connections > interactive.limits.max_connections,
            "tiny/interactive at {budget}"
        );
        assert!(
            tiny.limits.max_connections > streaming.limits.max_connections,
            "tiny/streaming at {budget}"
        );
        for (left, right) in [
            (&tiny, &interactive),
            (&tiny, &streaming),
            (&interactive, &streaming),
        ] {
            assert!(
                left.limits.max_connections != right.limits.max_connections
                    || left.limits.max_complete_body != right.limits.max_complete_body,
                "two shapes collapse at {budget}"
            );
        }
        assert!(
            streaming.limits.max_complete_body <= interactive.limits.max_complete_body,
            "streaming spends the budget on connections instead of a big body"
        );
    }
}

#[test]
fn the_reference_values_stay_close_to_the_historical_table() {
    // The three-level table is now a calibration anchor, not a constant. The
    // derived values at each reference budget stay within one order of
    // magnitude of it, and the deviations are recorded here.
    let historical: &[(&str, [u64; 3])] = &[
        ("max_body_bytes", [8 * MIB, 64 * MIB, 512 * MIB]),
        ("max_json_body_bytes", [64 * KIB, 256 * KIB, MIB]),
        // Historical 128 KiB / 1 MiB / 8 MiB. Streaming drops to 2 MiB:
        // 512 connections at 8 MiB each never fit a 2 GiB budget.
        ("max_complete_body_bytes", [128 * KIB, MIB, 8 * MIB]),
        // Connections are not on this list: they are solved against the
        // budget now (Limits::fitting_connections), so there is no historical
        // anchor to stay close to. The budget bound below is the anchor.
        // max_streams is solved from the connection count now, so it has no
        // historical anchor either.
        ("ws_raw_queue", [16, 64, 128]),
    ];
    for (key, row) in historical {
        for (index, workload) in Workload::ALL.into_iter().enumerate() {
            if workload.reference_budget() > BUDGET_CAP {
                // The cap is below this shape's reference budget, so the
                // historical anchor is not reachable in this build: the
                // shape runs on a smaller budget and derives from that.
                continue;
            }
            let derived = at_reference(workload);
            let value = match *key {
                "max_body_bytes" => derived.limits.max_body,
                "max_json_body_bytes" => derived.limits.max_json_body,
                "max_complete_body_bytes" => derived.limits.max_complete_body,
                "ws_raw_queue" => derived.limits.ws_raw_queue as u64,
                other => panic!("no accessor for {other}"),
            };
            let expected = row[index] as f64;
            let ratio = value as f64 / expected;
            assert!(
                (0.25..=4.0).contains(&ratio),
                "{key} for {}: derived {value} vs historical {} (ratio {ratio:.2})",
                workload.as_str(),
                row[index]
            );
        }
    }
}

#[test]
fn the_boundary_matrix_holds_for_every_shape() {
    // {shape} x {in band, below floor, above upper bound}, at the shape's
    // own reference budget. The fourth column, "over budget", is
    // `every_shape_refuses_when_the_fixed_terms_alone_do_not_fit` below.
    for workload in Workload::ALL {
        let budget = reference_budget(workload);

        let in_band = derive_for(
            budget,
            Some(workload),
            Settings {
                max_connections: Some(8),
                ..Settings::default()
            },
        );
        assert_eq!(in_band.limits.max_connections, 8, "{}", workload.as_str());
        assert!(
            in_band.report.notes.is_empty(),
            "{}: {:?}",
            workload.as_str(),
            in_band.report.notes
        );

        let below = derive_for(
            budget,
            Some(workload),
            Settings {
                max_streams: Some(0),
                ..Settings::default()
            },
        );
        assert_eq!(below.limits.max_streams, 1, "{}", workload.as_str());
        assert_eq!(
            note(&below.report, "max_streams").map(|note| note.reason),
            Some(OverrideReason::BelowFloor),
            "{}",
            workload.as_str()
        );

        let above = derive_for(
            budget,
            Some(workload),
            Settings {
                max_connections: Some(usize::MAX),
                ..Settings::default()
            },
        );
        // Above the ceiling the value is clamped to it, then the invariant
        // shrinks it to what the budget holds; it no longer falls back to the
        // shape's solved count, which is not an upper bound any more.
        assert!(
            above.limits.max_connections <= crate::platform::limits::MAX_CONNECTIONS_CEILING,
            "{}",
            workload.as_str()
        );
        assert_eq!(
            note(&above.report, "max_connections").map(|note| note.reason),
            Some(OverrideReason::AboveUpperBound),
            "{}",
            workload.as_str()
        );

        for derived in [in_band, below, above] {
            assert_eq!(derived.report.refusal, None, "{}", workload.as_str());
            assert!(
                derived.limits.worst_case_bytes() <= budget,
                "{}",
                workload.as_str()
            );
        }
    }
}

#[test]
fn every_shape_refuses_when_the_fixed_terms_alone_do_not_fit() {
    // 16 MiB frames at the floor budget: the queue term alone is larger
    // than the budget, and it is not a term the shrink order may touch, so
    // every shape has to refuse rather than run until the OOM killer.
    for workload in Workload::ALL {
        let derived = derive_for(
            MIN_BUDGET,
            Some(workload),
            Settings {
                ws_raw_max_bytes: Some(16 * MIB as usize),
                ws_raw_queue: Some(4),
                ..Settings::default()
            },
        );
        let reason = derived
            .report
            .refusal
            .unwrap_or_else(|| panic!("{} must refuse", workload.as_str()));
        assert!(reason.contains("does not fit"), "{reason}");
        assert!(reason.contains("queued frames"), "{reason}");
    }
}

#[test]
fn a_single_core_suggests_the_tiny_shape_whatever_the_budget() {
    // Cores are part of the suggestion: one core cannot drive the
    // concurrency a larger shape asks for (user story 1).
    let settings = Settings::default();
    let profile = ResourceProfile {
        available_memory: Some(16 * GIB),
        cpus: Some(1),
        ..ResourceProfile::default()
    };
    let derived = derive_with(&settings, &profile);

    assert_eq!(derived.report.budget.quantized, BUDGET_CAP.min(4 * GIB));
    assert_eq!(derived.report.workload, Workload::Tiny);
    assert_eq!(
        derived.report.workload_source,
        crate::platform::resources::WorkloadSource::Derived
    );

    // Many cores, same budget: the budget decides — but only up to the
    // cap this build has. When the cap pulls the budget below a shape's
    // threshold, sixteen cores cannot buy the shape back.
    let profile = ResourceProfile {
        cpus: Some(16),
        ..profile
    };
    let derived = derive_with(&settings, &profile);
    let quantized = derived.report.budget.quantized;
    let expected = if quantized < 512 * MIB {
        Workload::Tiny
    } else if quantized < 2 * GIB {
        Workload::Interactive
    } else {
        Workload::Streaming
    };
    assert_eq!(derived.report.workload, expected);
}

#[test]
fn an_explicit_value_inside_the_band_is_taken_as_written() {
    // User story 6: a tuned value inside the band must not be "optimised"
    // away, even when it is well below the shape's reference.
    let settings = Settings {
        max_connections: Some(4),
        ..Settings::default()
    };
    let derived = derive_for(2 * GIB, Some(Workload::Streaming), settings);

    assert_eq!(derived.limits.max_connections, 4);
    assert_eq!(note(&derived.report, "max_connections"), None);
}

#[test]
fn a_value_above_the_upper_bound_falls_back_to_the_shape_reference() {
    // Not to the ceiling: a typo returns to a sane number, not to the
    // largest value that happens to fit.
    let settings = Settings {
        max_body_bytes: Some(GIB),
        ..Settings::default()
    };
    let derived = derive_for(128 * MIB, Some(Workload::Tiny), settings);
    let default = derive_for(128 * MIB, Some(Workload::Tiny), Settings::default());

    assert_eq!(
        derived.limits.max_body, default.limits.max_body,
        "falls back to the tiny shape's reference body"
    );
    let note = note(&derived.report, "max_body_bytes").expect("a note");
    assert_eq!(note.configured, GIB);
    assert_eq!(note.effective, derived.limits.max_body);
    assert_eq!(note.reason, OverrideReason::AboveUpperBound);
}

#[test]
fn a_value_below_the_floor_is_raised_with_a_note() {
    let settings = Settings {
        max_streams: Some(0),
        request_stack_bytes: Some(1),
        ..Settings::default()
    };
    let derived = derive_for(2 * GIB, None, settings);

    assert_eq!(derived.limits.max_streams, 1);
    assert_eq!(derived.limits.request_stack_bytes, 64 * KIB as usize);
    assert_eq!(
        note(&derived.report, "max_streams").map(|note| note.reason),
        Some(OverrideReason::BelowFloor)
    );
    assert_eq!(
        note(&derived.report, "request_stack_bytes").map(|note| note.reason),
        Some(OverrideReason::BelowFloor)
    );
}

#[test]
fn the_build_cap_only_tightens_the_budget() {
    // User stories 9 and 10, and the machine-independent assertion the
    // spec asks for: no document and no profile can push the budget over
    // the build's ceiling.
    let profile = ResourceProfile {
        available_memory: Some(256 * GIB),
        cgroup_remaining: Some(128 * GIB),
        cpus: Some(64),
        ..ResourceProfile::default()
    };
    for configured in [None, Some(64 * GIB), Some(u64::MAX)] {
        let settings = Settings {
            mem_budget_bytes: configured,
            ..Settings::default()
        };
        let derived = derive_with(&settings, &profile);
        assert_eq!(derived.report.refusal, None);
        assert!(
            derived.report.budget.quantized <= BUDGET_CAP,
            "configured {configured:?}: {} > cap {}",
            derived.report.budget.quantized,
            BUDGET_CAP
        );
        assert!(derived.limits.worst_case_bytes() <= BUDGET_CAP);
    }

    // A deliberately tiny explicit budget is taken as written, so the
    // invariant refuses startup instead of silently raising it to 64 MiB.
    let settings = Settings {
        mem_budget_bytes: Some(1),
        ..Settings::default()
    };
    let derived = derive_with(&settings, &profile);
    assert!(derived.report.refusal.is_some());

    // An explicit budget above the ceiling is reported, not silently
    // rewritten.
    let settings = Settings {
        mem_budget_bytes: Some(64 * GIB),
        ..Settings::default()
    };
    let derived = derive_with(&settings, &profile);
    assert_eq!(
        note(&derived.report, "mem_budget_bytes").map(|note| note.reason),
        Some(OverrideReason::BuildCap)
    );
}

#[test]
fn a_configuration_that_cannot_fit_is_refused_with_the_terms() {
    // The stack term is fixed per shape and cannot be shrunk, so a large
    // stack on a small budget has no legal answer.
    // Pinned connections keep the solver from absorbing the overshoot: with
    // an unconfigured count the budget would simply hold fewer slots, which is
    // the intended answer, not a refusal.
    let settings = Settings {
        request_stack_bytes: Some(8 * MIB as usize),
        blocking_threads: Some(8),
        max_connections: Some(64),
        ..Settings::default()
    };
    let derived = derive_for(MIN_BUDGET, Some(Workload::Streaming), settings);

    let reason = derived.report.refusal.expect("refuses");
    assert!(reason.contains("does not fit"), "{reason}");
    assert!(reason.contains("thread stacks"), "{reason}");
}

/// Not in a `mem_low` build: the fixed terms alone (128 × 2 MiB of queued
/// frames) need more than its 128 MiB budget, so the shrink has nowhere to
/// go and the derivation refuses instead — which is
/// `every_shape_refuses_when_the_fixed_terms_alone_do_not_fit`.
#[cfg(not(mem_low))]
#[test]
fn a_maxed_out_legal_configuration_is_shrunk_until_the_invariant_holds() {
    // Every value here is inside its band, but together they cannot fit:
    // the derivation shrinks the single request body first, then the
    // concurrency, and reports both.
    // The connection count is pinned: an unconfigured one is solved to fit,
    // which would shrink the answer instead of the configuration.
    let settings = Settings {
        max_streams: Some(1024),
        ws_raw_max_bytes: Some(2 * MIB as usize),
        ws_raw_queue: Some(128),
        request_stack_bytes: Some(8 * MIB as usize),
        blocking_threads: Some(8),
        max_connections: Some(2048),
        ..Settings::default()
    };
    let derived = derive_for(2 * GIB, Some(Workload::Streaming), settings);
    let default = derive_for(2 * GIB, Some(Workload::Streaming), Settings::default());

    assert_eq!(derived.report.refusal, None);
    assert!(derived.limits.worst_case_bytes() <= 2 * GIB);
    assert!(
        derived.limits.max_complete_body < default.limits.max_complete_body
            || derived.limits.max_connections < default.limits.max_connections,
        "the invariant did not shrink anything: {:?}",
        derived.report.notes
    );
    let shrunk = derived
        .report
        .notes
        .iter()
        .any(|note| note.reason == OverrideReason::InvariantShrink);
    assert!(shrunk, "no invariant note: {:?}", derived.report.notes);
}

#[test]
fn the_blocking_queue_follows_the_connection_count_and_can_be_lowered() {
    // Unconfigured: the connection count (a queued job belongs to one).
    let defaults = derive_for(2 * GIB, Some(Workload::Streaming), Settings::default()).limits;
    assert_eq!(
        defaults.blocking_queue, defaults.max_connections,
        "queue {} connections {}",
        defaults.blocking_queue, defaults.max_connections
    );

    // A queue larger than the admitted connections can never fill, so it is
    // clamped instead of being advertised (900 is inside its band; above the
    // band the key falls back to the shape default like every other key).
    let huge = derive_for(
        128 * MIB,
        Some(Workload::Tiny),
        Settings {
            blocking_queue: Some(900),
            ..Settings::default()
        },
    )
    .limits;
    assert_eq!(huge.blocking_queue, huge.max_connections.min(900));

    // An explicit smaller queue is taken as written: an operator may want the
    // pool to refuse earlier than the connection cap.
    let small = derive_for(
        2 * GIB,
        Some(Workload::Streaming),
        Settings {
            blocking_queue: Some(8),
            ..Settings::default()
        },
    )
    .limits;
    assert_eq!(small.blocking_queue, 8);
}

#[test]
fn every_limit_has_a_floor() {
    let settings = Settings {
        request_stack_bytes: Some(0),
        io_stack_bytes: Some(0),
        blocking_threads: Some(0),
        max_body_bytes: Some(0),
        max_json_body_bytes: Some(0),
        max_complete_body_bytes: Some(0),
        max_header_bytes: Some(0),
        max_streams: Some(0),
        stream_buffer_bytes: Some(0),
        max_connections: Some(0),
        max_connections_per_ip: Some(0),
        max_subscriptions: Some(0),
        max_subscribers_per_plugin: Some(0),
        http_idle_timeout_secs: Some(0),
        log_max_bytes: Some(0),
        log_rotations: Some(0),
        max_package_bytes: Some(0),
        transfer_timeout_secs: Some(0),
        control_timeout_secs: Some(0),
        default_timeout_ms: Some(0),
        max_timeout_ms: Some(0),
        ws_max_message_bytes: Some(0),
        ws_inflight_bytes: Some(0),
        ws_inflight_requests: Some(0),
        ws_buffer_shrink_bytes: Some(0),
        ws_raw_max_bytes: Some(0),
        ws_raw_queue: Some(0),
        pending_queue: Some(0),
        progress_queue: Some(0),
        ..Settings::default()
    };
    let limits = derive_for(2 * GIB, None, settings).limits;

    assert_eq!(limits.request_stack_bytes, 64 * KIB as usize);
    assert_eq!(limits.io_stack_bytes, 32 * KIB as usize);
    assert_eq!(limits.blocking_threads, 2);
    assert_eq!(limits.max_body, 64 * KIB);
    assert_eq!(limits.max_json_body, 4 * KIB);
    assert_eq!(limits.max_complete_body, 16 * KIB);
    assert_eq!(limits.max_header, 1024_usize);
    assert_eq!(limits.max_streams, 1_usize);
    assert_eq!(limits.stream_buffer, 4 * KIB as usize);
    assert_eq!(limits.max_connections, 1_usize);
    assert_eq!(limits.max_connections_per_ip, 1_usize);
    assert_eq!(limits.max_subscriptions, 0_usize);
    assert_eq!(limits.max_subscribers_per_plugin, 0_usize);
    assert_eq!(limits.idle_timeout, Duration::from_secs(1));
    assert_eq!(limits.log_max_bytes, 4 * KIB);
    assert_eq!(limits.log_rotations, 1_usize);
    assert_eq!(limits.max_package_bytes, MIB);
    assert_eq!(limits.transfer_timeout, Duration::from_secs(10));
    assert_eq!(limits.control_timeout, Duration::from_secs(1));
    assert_eq!(limits.default_timeout, Duration::from_millis(100));
    assert_eq!(limits.max_timeout, Duration::from_millis(100));
    assert_eq!(limits.ws_max_message_bytes, 1024_usize);
    assert_eq!(limits.ws_inflight_bytes, 64 * KIB as usize);
    assert_eq!(limits.ws_inflight_requests, 1_usize);
    assert_eq!(limits.ws_buffer_shrink_bytes, 4 * KIB as usize);
    assert_eq!(limits.ws_raw_max_bytes, 1024_usize);
    assert_eq!(limits.ws_raw_queue, 4_usize);
    assert_eq!(limits.pending_queue, 8_usize);
    assert_eq!(limits.progress_queue, 4_usize);
}

#[test]
fn a_huge_value_is_clamped_to_the_ceiling_and_kept_legal() {
    let settings = Settings {
        max_streams: Some(usize::MAX),
        max_connections: Some(usize::MAX),
        ..Settings::default()
    };
    let derived = derive_for(2 * GIB, Some(Workload::Streaming), settings);
    // The contract for a value above its upper bound: clamp to the absolute
    // ceiling, record it, and let the invariant keep the result legal. Streams
    // follow the connection count now, so the shape's solved value is not a
    // fallback any more.
    assert!(derived.limits.max_streams <= 4096);
    assert_eq!(
        note(&derived.report, "max_streams").map(|note| note.reason),
        Some(OverrideReason::AboveUpperBound)
    );
    assert!(derived.limits.max_connections <= crate::platform::limits::MAX_CONNECTIONS_CEILING);
    assert_eq!(
        note(&derived.report, "max_connections").map(|note| note.reason),
        Some(OverrideReason::AboveUpperBound)
    );
    // The ceiling is only reachable when this build may plan for the requested
    // budget: a lower MEM_LEVEL caps the budget below 2 GiB and the invariant
    // legitimately solves fewer connections than the ceiling.
    if crate::platform::limits::BUDGET_CAP >= 2 * GIB {
        assert_eq!(
            derived.limits.max_connections,
            crate::platform::limits::MAX_CONNECTIONS_CEILING
        );
    }
    assert!(derived.limits.worst_case_bytes() <= 2 * GIB);
}

#[test]
fn blocking_threads_scale_with_the_shape_and_stay_in_two_through_thirty_two() {
    // Streaming at 2 GiB would default to 16, but a lower MEM_LEVEL caps the
    // budget and the budget selects the shape: compare against the shape the
    // binary really derives instead of the one the reference names.
    let effective = reference_budget(Workload::Streaming);
    let default =
        specs::BLOCKING_THREADS.default_value(Workload::of_budget(effective), effective) as usize;
    let defaults = derive_for(2 * GIB, None, Settings::default()).limits;
    assert_eq!(defaults.blocking_threads, default);

    for (configured, expected) in [
        (0, 2),
        (1, 2),
        (2, 2),
        (5, 5),
        (16, 16),
        (32, 32),
        (33, default),
        (64, default),
    ] {
        let settings = Settings {
            blocking_threads: Some(configured),
            ..Settings::default()
        };
        assert_eq!(
            derive_for(2 * GIB, None, settings).limits.blocking_threads,
            expected,
            "configured {configured}"
        );
    }
}

#[test]
fn a_large_default_never_exceeds_the_max_timeout() {
    let settings = Settings {
        default_timeout_ms: Some(300_000),
        max_timeout_ms: Some(1),
        ..Settings::default()
    };
    let limits = derive_for(2 * GIB, None, settings).limits;

    assert_eq!(limits.default_timeout, Duration::from_millis(300_000));
    assert_eq!(limits.max_timeout, Duration::from_millis(300_000));
}

#[test]
fn a_lower_budget_never_raises_a_derived_value() {
    // Per shape: the unconfigured budget also picks the shape, and a shape
    // change legitimately moves the connection count (a smaller body holds
    // more slots), so comparing across shapes would test the wrong thing.
    for workload in Workload::ALL {
        let mut previous: Option<Limits> = None;
        for budget in [2 * GIB, GIB, 512 * MIB, 256 * MIB, 128 * MIB, 64 * MIB] {
            let limits = derive_for(budget, Some(workload), Settings::default()).limits;
            if let Some(previous) = previous {
                assert!(
                    limits.max_connections <= previous.max_connections,
                    "{} at {budget}",
                    workload.as_str()
                );
                assert!(limits.max_body <= previous.max_body);
                assert!(limits.ws_inflight_bytes <= previous.ws_inflight_bytes);
                assert!(limits.pending_queue <= previous.pending_queue);
            }
            previous = Some(limits);
        }
    }
}

#[test]
fn the_notes_are_readable() {
    let note = Override::new(
        "max_body_bytes",
        1024,
        8192,
        OverrideReason::AboveUpperBound,
    );

    assert_eq!(
        note.to_string(),
        "max_body_bytes: 1024 -> 8192 (above-upper-bound)"
    );
    assert_eq!(note.reason.as_str(), "above-upper-bound");
}

#[test]
fn the_report_carries_the_four_status_fields_and_the_notes() {
    let settings = Settings {
        max_body_bytes: Some(GIB),
        ..Settings::default()
    };
    let derived = derive_for(128 * MIB, Some(Workload::Tiny), settings);
    let json = derived.report.to_json();

    assert_eq!(json["budget_quantized"], 128 * MIB);
    assert_eq!(json["budget_source"], "configured");
    assert_eq!(json["build_cap"], BUDGET_CAP);
    assert_eq!(json["workload"], "tiny");
    assert_eq!(json["workload_source"], "configured");
    assert_eq!(json["notes"][0]["key"], "max_body_bytes");
    assert_eq!(json["notes"][0]["reason"], "above-upper-bound");
    assert!(json["worst_case"].as_u64().unwrap() <= 128 * MIB);

    let lines = derived.report.lines();
    assert!(lines
        .iter()
        .any(|line| line.contains("workload: tiny (configured)")));
    assert!(lines.iter().any(|line| line.contains("worst case")));
}

#[test]
fn a_derived_workload_is_labelled_as_derived() {
    let derived = derive_for(2 * GIB, None, Settings::default());
    let quantized = derived.report.budget.quantized;
    let expected = if quantized < 512 * MIB {
        Workload::Tiny
    } else if quantized < 2 * GIB {
        Workload::Interactive
    } else {
        Workload::Streaming
    };

    assert_eq!(derived.report.workload, expected);
    assert_eq!(derived.report.workload_source, WorkloadSource::Derived);
    assert_eq!(
        derived.report.budget.source,
        crate::platform::resources::BudgetSource::Configured
    );
}

#[test]
fn the_worst_case_is_what_the_report_says_it_is() {
    let derived = at_reference(Workload::Interactive);

    assert_eq!(derived.report.worst_case, derived.limits.worst_case_bytes());
    // Adding a connection cannot lower the worst case.
    let mut raised = derived.limits.clone();
    raised.max_connections += 1;
    assert!(raised.worst_case_bytes() > derived.limits.worst_case_bytes());
}

#[test]
fn the_thread_term_follows_the_hosts_topology() {
    let limits = at_reference(Workload::Interactive).limits;

    if cfg!(windows) {
        assert_eq!(limits.threads(), limits.max_connections);
    } else {
        assert_eq!(limits.threads(), LOOP_THREADS + limits.blocking_threads);
    }
}

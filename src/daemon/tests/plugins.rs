//! Plugin-install tests: the temp payload cleanup (#286/#287) and the wire
//! projection of a typed management error. Split out of `daemon/tests.rs` to
//! keep that file inside its recorded size baseline.

use super::*;

use serde_json::json;

use crate::kernel::failure::Problem;

#[test]
fn a_typed_management_error_projects_its_wire_kind() {
    // The category is raised by the layer that rejected the input (#151);
    // the wire projection only reads it.
    let response = status_response(
        Some(json!(8)),
        -32006,
        Err(Problem::TooLarge.error("package exceeds 8 MiB")),
    );
    assert_eq!(
        response.error.expect("error").data.expect("data")["kind"],
        "too_large"
    );
}

/// #286: the payload the store staged in `<temp>` is a one-shot input. It must
/// not survive the attempt, whichever way the attempt ends.
#[test]
fn a_temp_payload_is_removed_after_a_refused_install() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );
    let temp = daemon.config.paths.temp_dir.clone();
    std::fs::create_dir_all(&temp).expect("temp dir");
    let payload = temp.join("6f".repeat(32));
    std::fs::write(
        &payload,
        crate::manage::test_support::test_signed_package("other", "other-triple"),
    )
    .expect("write package");

    let response = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.install",
        json!({"path": payload.to_string_lossy(), "strategy": "replace"}),
    );
    assert_error_kind(&response, -32006, "unsupported_platform");
    assert!(
        !payload.exists(),
        "a refused install must not keep its temp payload"
    );
}

#[test]
fn a_temp_payload_is_removed_after_a_successful_install() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );
    let temp = daemon.config.paths.temp_dir.clone();
    std::fs::create_dir_all(&temp).expect("temp dir");
    let payload = temp.join("3a".repeat(32));
    std::fs::write(
        &payload,
        crate::manage::test_support::test_signed_package("demo", crate::platform::target()),
    )
    .expect("write package");

    let response = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.install",
        json!({"path": payload.to_string_lossy(), "strategy": "replace"}),
    );
    assert!(response["error"].is_null(), "{response}");
    assert!(
        !payload.exists(),
        "an installed package must not keep its temp payload"
    );
    assert!(
        daemon
            .config
            .paths
            .default_plugin_root()
            .join("demo")
            .is_dir(),
        "the install itself must still have happened"
    );
}

/// The store may hand over a file-bundle part (`temp/files/<id>/payload.spk`):
/// the manifest and the other parts belong to the same consumed entry.
#[test]
fn a_temp_file_bundle_is_removed_whole() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );
    let temp = daemon.config.paths.temp_dir.clone();
    let bundle = temp.join("files").join("ab".repeat(16));
    std::fs::create_dir_all(&bundle).expect("bundle dir");
    let payload = bundle.join("payload.spk");
    std::fs::write(
        &payload,
        crate::manage::test_support::test_signed_package("other", "other-triple"),
    )
    .expect("write package");

    let response = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.install",
        json!({"path": payload.to_string_lossy(), "strategy": "replace"}),
    );
    assert_error_kind(&response, -32006, "unsupported_platform");
    assert!(!bundle.exists(), "the whole temp bundle is one payload");
}

/// Only the area the daemon owns is cleaned: a package the caller keeps
/// somewhere else is the caller's to delete.
#[test]
fn a_package_outside_temp_is_not_removed() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );
    let dir = crate::platform::test_support::TempDir::new();
    let path = dir.path().join("other.spk");
    std::fs::write(
        &path,
        crate::manage::test_support::test_signed_package("other", "other-triple"),
    )
    .expect("write package");

    let response = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.install",
        json!({"path": path.to_string_lossy(), "strategy": "replace"}),
    );
    assert_error_kind(&response, -32006, "unsupported_platform");
    assert!(path.exists(), "a package outside temp is the caller's file");
}

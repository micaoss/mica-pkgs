//! Contract tests for the positive MQTT application enrollment boundary.
// Tests and fixtures answer a broken expectation by panicking.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mica_mqttd::enrollment::Enrollment;

#[test]
fn a_direct_mica_service_is_admitted_only_when_explicitly_enrolled() {
    let enrollment = Enrollment::from_names(["com.mica.sensor.abc123"])
        .expect("the application package enrolled a valid direct service name");

    let application = enrollment
        .application("com.mica.sensor.abc123")
        .expect("the exact enrolled name is admitted");
    assert_eq!(application.bus_name(), "com.mica.sensor.abc123");
    assert_eq!(application.class(), "sensor");
    assert!(
        enrollment.application("com.mica.sensor.other").is_none(),
        "a sibling name is not covered by an exact enrollment"
    );
}

#[test]
fn micad_can_never_be_enrolled_in_the_mqtt_application_data_plane() {
    let error = Enrollment::from_names(["com.mica.micad"])
        .expect_err("micad must be structurally excluded from MQTT");

    assert!(
        error.to_string().contains("com.mica.micad"),
        "the refusal should name the unsafe enrollment: {error}"
    );
}

#[test]
fn enrollment_rejects_wildcards_and_non_bus_names() {
    for name in ["com.mica.sensor.*", "com.mica.sensor.+", "com.mica.1sensor"] {
        assert!(
            Enrollment::from_names([name]).is_err(),
            "{name} must not become an exact application enrollment"
        );
    }
}

#[tokio::test]
async fn directory_entries_are_exact_enrollments() {
    let directory = tempfile::tempdir().expect("create enrollment directory");
    std::fs::write(directory.path().join("com.mica.sensor.abc123"), "ignored\n")
        .expect("write enrollment");

    let enrollment = Enrollment::load(directory.path())
        .await
        .expect("load application package enrollment");

    assert!(enrollment.application("com.mica.sensor.abc123").is_some());
    assert!(enrollment.application("com.mica.sensor.other").is_none());
}

#[tokio::test]
async fn a_directory_entry_cannot_enroll_micad() {
    let directory = tempfile::tempdir().expect("create enrollment directory");
    std::fs::write(directory.path().join("com.mica.micad"), "").expect("write unsafe enrollment");

    let error = Enrollment::load(directory.path())
        .await
        .expect_err("micad must remain outside MQTT even when a file names it");

    assert!(error.to_string().contains("com.mica.micad"));
}

#[cfg(unix)]
#[tokio::test]
async fn non_regular_directory_entries_fail_closed() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("create enrollment directory");
    symlink(
        directory.path().join("missing"),
        directory.path().join("com.mica.sensor.abc123"),
    )
    .expect("create enrollment symlink");

    let error = Enrollment::load(directory.path())
        .await
        .expect_err("symlinks cannot enroll an application");

    assert!(error.to_string().contains("not a regular file"));
}

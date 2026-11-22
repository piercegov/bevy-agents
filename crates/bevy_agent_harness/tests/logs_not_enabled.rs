//! Separate test executable: nothing here installs `log_layer`, so the
//! process-wide "capture installed" flag stays false.

use bevy::ecs::system::RunSystemOnce;
use bevy::prelude::*;
use bevy_agent_harness::LogBuffer;
use bevy_agent_harness::harness::logs::{capture_installed, get_logs};

#[test]
fn logs_report_missing_capture_instead_of_an_empty_list() {
    let mut app = App::new();
    let buffer = LogBuffer::new(16);
    app.insert_resource(buffer.clone());
    assert!(!capture_installed());

    let err = app
        .world_mut()
        .run_system_once_with(get_logs, None)
        .unwrap()
        .unwrap_err();
    assert!(
        err.message.contains("log capture is not enabled"),
        "{}",
        err.message
    );

    // Entries pushed by hand are still served.
    buffer.push("ERROR", "game", "boom".into());
    let ok = app
        .world_mut()
        .run_system_once_with(get_logs, None)
        .unwrap()
        .unwrap();
    assert_eq!(ok["entries"][0]["message"], "boom");
}

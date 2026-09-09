use bevy::prelude::*;
use bevy_agent_harness::testing::TestApp;

// Separate integration-test executable: global subscriber setup must be fresh.
#[test]
fn existing_subscriber_is_not_silently_treated_as_successful_capture() {
    tracing::subscriber::set_global_default(tracing_subscriber::registry()).unwrap();
    let mut app = TestApp::new();
    app.add_plugins(|app: &mut App| {
        app.add_systems(Update, || error!("uncaptured"));
    });
    app.advance(1);
    let error =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| app.assert_no_log_errors()));
    assert!(error.is_err());
    let mut disabled = TestApp::builder().capture_logs(false).build();
    disabled.advance(1);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| disabled.logs())).is_err());
}

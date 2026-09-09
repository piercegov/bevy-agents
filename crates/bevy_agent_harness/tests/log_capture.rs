use bevy::prelude::*;
use bevy_agent_harness::testing::TestApp;

#[test]
fn overlapping_apps_do_not_share_errors_or_filters() {
    let mut first = TestApp::builder().log_filter("info").build();
    let mut second = TestApp::builder().log_filter("error").build();
    first.add_plugins(|app: &mut App| {
        info!("plugin setup");
        app.add_systems(Update, || error!("first only"));
    });
    second.add_plugins(|app: &mut App| {
        app.add_systems(Update, || warn!("filtered"));
    });
    first.advance(1);
    second.advance(1);
    second.assert_no_log_errors();
    assert!(second.logs().is_empty());
    assert!(first.logs().iter().any(|e| e.message == "plugin setup"));
    assert!(first.logs().iter().any(|e| e.message == "first only"));
    assert!(
        !first
            .logs()
            .iter()
            .any(|e| e.message.contains("already set"))
    );
}

#[test]
fn concurrent_apps_capture_worker_logs_in_their_own_scopes() {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|marker| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut app = TestApp::new();
                app.add_plugins(move |app: &mut App| {
                    app.add_systems(Update, move || {
                        let caller = std::thread::current().id();
                        // Explicitly exercise a Bevy pool worker, not only the
                        // thread executing the schedule. Wait before leaving scope.
                        let (send, receive) = std::sync::mpsc::channel();
                        bevy::tasks::IoTaskPool::get()
                            .spawn(async move {
                                let worker = std::thread::current().id();
                                error!("{marker}");
                                send.send(worker).unwrap();
                            })
                            .detach();
                        assert_ne!(
                            caller,
                            receive
                                .recv_timeout(std::time::Duration::from_secs(5))
                                .unwrap()
                        );
                    });
                });
                barrier.wait();
                app.advance(2);
                let logs = app.logs();
                assert_eq!(logs.len(), 2, "{logs:?}");
                assert!(logs.iter().all(|e| e.message == marker));
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}

#[test]
fn panic_releases_capture_and_overflow_is_not_success() {
    let mut app = TestApp::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        app.with_app(|_| panic!("intentional"));
    }));
    assert!(result.is_err());
    app.with_app(|_| {
        for _ in 0..10_001 {
            error!("overflow");
        }
    });
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| app.assert_no_log_errors()))
            .is_err()
    );
    TestApp::new().assert_no_log_errors();
}

#[test]
fn entity_exists_and_headless_mesh_setup() {
    #[derive(Component)]
    struct Dead;
    let mut app = TestApp::new();
    app.with_app(|app| {
        app.init_asset::<Mesh>();
    });
    let entity = app.spawn(Dead);
    assert!(app.entity_exists(entity) && app.is_alive(entity));
    app.advance(2);
    assert!(
        app.world()
            .contains_resource::<Messages<AssetEvent<Mesh>>>()
    );
    app.world_mut().despawn(entity);
    let replacement = app.spawn(Dead);
    assert!(app.entity_exists(replacement));
    assert!(!app.entity_exists(entity) && !app.is_alive(entity));
}

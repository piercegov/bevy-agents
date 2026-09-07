use bevy::prelude::*;
use bevy_agent_harness::testing::TestApp;

#[derive(Component, Debug)]
struct Player;

#[derive(Component, Debug)]
struct Health(f32);

#[derive(Component, Debug)]
struct ClickMarker(Vec2);

#[derive(Resource, Default, Debug)]
struct Counter(u32);

fn game(app: &mut App) {
    app.init_resource::<Counter>()
        .add_systems(Startup, |mut commands: Commands| {
            commands.spawn((
                Name::new("Player"),
                Player,
                Health(100.0),
                Transform::default(),
            ));
        })
        .add_systems(
            Update,
            (
                |keys: Res<ButtonInput<KeyCode>>, mut q: Query<&mut Health, With<Player>>| {
                    if keys.just_pressed(KeyCode::Space) {
                        q.single_mut().unwrap().0 -= 10.0;
                    }
                },
                |keys: Res<ButtonInput<KeyCode>>,
                 time: Res<Time>,
                 mut q: Query<&mut Transform, With<Player>>| {
                    if keys.pressed(KeyCode::ArrowRight) {
                        q.single_mut().unwrap().translation.x += 60.0 * time.delta_secs();
                    }
                },
                |buttons: Res<ButtonInput<MouseButton>>,
                 windows: Query<&Window>,
                 mut commands: Commands| {
                    if buttons.just_pressed(MouseButton::Left) {
                        let pos = windows.single().unwrap().cursor_position().unwrap();
                        commands.spawn(ClickMarker(pos));
                    }
                },
                |mut counter: ResMut<Counter>| counter.0 += 1,
                |counter: Res<Counter>| {
                    if counter.0 == 3 {
                        warn!("counter hit three");
                    }
                },
            ),
        );
}

#[test]
fn tap_space_damages_player() {
    let mut app = TestApp::new();
    app.add_plugins(game);
    app.advance(1);
    let player = app.single::<Player>();
    app.assert_component::<Health>(player, |h| h.0 == 100.0);
    app.tap(KeyCode::Space);
    app.assert_component::<Health>(player, |h| h.0 == 90.0);
    // A held key does not re-trigger just_pressed.
    app.press(KeyCode::Space).advance(5).release(KeyCode::Space);
    app.assert_component::<Health>(player, |h| h.0 == 80.0);
}

#[test]
fn holding_a_key_moves_with_fixed_dt() {
    let mut app = TestApp::new();
    app.add_plugins(game);
    app.advance(1);
    let player = app.single::<Player>();
    app.hold(KeyCode::ArrowRight, 30);
    // 30 frames at 1/60 s with 60 px/s => 30 px (the release frame doesn't count).
    let x = app.component::<Transform>(player).translation.x;
    assert!((x - 30.0).abs() < 0.01, "x = {x}");
}

#[test]
fn click_reports_cursor_position() {
    let mut app = TestApp::new();
    app.add_plugins(game);
    app.advance(1);
    app.click(Vec2::new(123.0, 45.0));
    let marker = app.single::<ClickMarker>();
    app.assert_component::<ClickMarker>(marker, |m| m.0 == Vec2::new(123.0, 45.0));
}

#[test]
fn logs_are_captured() {
    let mut app = TestApp::new();
    app.add_plugins(game);
    app.advance(5);
    let problems = app.log_problems();
    assert!(
        problems
            .iter()
            .any(|e| e.level == "WARN" && e.message.contains("counter hit three")),
        "logs: {problems:?}"
    );
    app.assert_no_log_errors();
}

#[test]
fn run_until_and_names() {
    let mut app = TestApp::new();
    app.add_plugins(game);
    let frames = app
        .run_until(100, |world| world.resource::<Counter>().0 >= 10)
        .unwrap();
    assert_eq!(frames, 10);
    assert!(app.find_by_name("Player").is_some());
    assert!(app.find_by_name("Nobody").is_none());
    let player = app.find_by_name("Player").unwrap();
    assert!(app.component_names(player).contains(&"Health".to_string()));
}

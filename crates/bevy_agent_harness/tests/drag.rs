//! Smooth cursor movement: `TestApp::drag` and the `Drag` path.

use bevy::input::ButtonState;
use bevy::input::mouse::{MouseButtonInput, MouseMotion};
use bevy::prelude::*;
use bevy::window::CursorMoved;
use bevy_agent_harness::harness::input::{Drag, Ease};
use bevy_agent_harness::testing::TestApp;

#[derive(Resource, Default)]
struct Seen {
    cursor: Vec<Vec2>,
    motion: Vec2,
    /// Cursor moves seen while the button was down.
    held_moves: usize,
    buttons: Vec<(MouseButton, ButtonState)>,
}

fn watch(app: &mut App) {
    app.init_resource::<Seen>().add_systems(
        Update,
        |mut seen: ResMut<Seen>,
         mut cursor: MessageReader<CursorMoved>,
         mut motion: MessageReader<MouseMotion>,
         mut buttons: MessageReader<MouseButtonInput>,
         held: Res<ButtonInput<MouseButton>>| {
            for b in buttons.read() {
                seen.buttons.push((b.button, b.state));
            }
            for c in cursor.read() {
                seen.cursor.push(c.position);
                if held.pressed(MouseButton::Right) {
                    seen.held_moves += 1;
                }
            }
            for m in motion.read() {
                seen.motion += m.delta;
            }
        },
    );
}

#[test]
fn drag_moves_one_step_per_frame_with_the_button_held() {
    let mut app = TestApp::new();
    app.add_plugins(watch);
    let (from, to) = (Vec2::new(100.0, 100.0), Vec2::new(300.0, 180.0));
    let before = app.frame();
    app.drag_with(
        Drag::new(from, to)
            .frames(20)
            .button(Some(MouseButton::Right)),
    );
    assert_eq!(app.frame() - before, 22);

    let seen = app.resource::<Seen>();
    assert_eq!(seen.cursor.len(), 21, "start plus one move per frame");
    assert_eq!(seen.cursor[0], from);
    assert_eq!(*seen.cursor.last().unwrap(), to);
    assert_eq!(seen.held_moves, 21, "every move happens during the hold");
    assert!(
        (seen.motion - (to - from)).length() < 0.01,
        "{}",
        seen.motion
    );
    assert_eq!(
        seen.buttons,
        [
            (MouseButton::Right, ButtonState::Pressed),
            (MouseButton::Right, ButtonState::Released)
        ]
    );
    // Eased: the first step is much shorter than one in the middle.
    let step = |i: usize| seen.cursor[i].distance(seen.cursor[i - 1]);
    assert!(step(1) * 4.0 < step(10), "{} vs {}", step(1), step(10));
    // No jumps: no step is longer than 1.6x the average.
    let average = from.distance(to) / 20.0;
    assert!((1..=20).all(|i| step(i) < average * 1.6));
}

#[test]
fn glide_without_a_button_presses_nothing() {
    let mut app = TestApp::new();
    app.add_plugins(watch);
    app.drag_with(
        Drag::new(Vec2::ZERO, Vec2::new(90.0, 0.0))
            .frames(9)
            .button(None)
            .ease(Ease::Linear),
    );
    let seen = app.resource::<Seen>();
    assert!(seen.buttons.is_empty());
    assert_eq!(seen.cursor.len(), 10);
    for (i, p) in seen.cursor.iter().enumerate() {
        assert!((p.x - 10.0 * i as f32).abs() < 0.01, "{:?}", seen.cursor);
    }
}

#[test]
fn waypoints_bend_the_path_through_them() {
    let waypoint = Vec2::new(100.0, 80.0);
    let drag = Drag::new(Vec2::ZERO, Vec2::new(200.0, 0.0))
        .via(waypoint)
        .frames(200)
        .ease(Ease::Linear);
    let positions = drag.positions();
    assert_eq!(positions.len(), 200);
    assert_eq!(*positions.last().unwrap(), Vec2::new(200.0, 0.0));
    let nearest = positions
        .iter()
        .map(|p| p.distance(waypoint))
        .fold(f32::MAX, f32::min);
    assert!(nearest < 1.5, "passes {nearest} px from the waypoint");
    // Constant speed along the curve.
    let steps: Vec<f32> = positions.windows(2).map(|w| w[0].distance(w[1])).collect();
    let (min, max) = steps
        .iter()
        .fold((f32::MAX, 0.0f32), |(lo, hi), s| (lo.min(*s), hi.max(*s)));
    assert!(max < min * 1.2, "steps vary from {min} to {max}");
}

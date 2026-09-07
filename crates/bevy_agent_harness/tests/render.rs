//! Headless GPU rendering + golden screenshots. Needs a working wgpu adapter
//! (Vulkan/Metal/DX12, or lavapipe/SwiftShader in CI).

use bevy::prelude::*;
use bevy_agent_harness::testing::{GoldenOptions, TestApp};

fn scene(app: &mut App) {
    app.add_systems(Startup, |mut commands: Commands| {
        commands.spawn(Camera2d);
        commands.spawn((
            Sprite::from_color(Color::srgb(1.0, 0.0, 0.0), Vec2::splat(100.0)),
            Transform::from_xyz(0.0, 0.0, 0.0),
        ));
    });
    app.insert_resource(ClearColor(Color::srgb(0.0, 0.0, 1.0)));
}

#[test]
fn renders_a_red_square_on_blue() {
    let mut app = TestApp::with_rendering(200, 100);
    app.add_plugins(scene);
    app.advance(3);
    let img = app.screenshot();
    assert_eq!(img.dimensions(), (200, 100));
    let center = img.get_pixel(100, 50);
    let corner = img.get_pixel(2, 2);
    assert!(center[0] > 200 && center[2] < 50, "center {center:?}");
    assert!(corner[2] > 200 && corner[0] < 50, "corner {corner:?}");

    // Golden comparison against tests/golden/red_square.png (created on first run).
    app.assert_golden_with(
        "red_square",
        GoldenOptions {
            max_diff_fraction: 0.01,
            channel_tolerance: 16,
        },
    );
}

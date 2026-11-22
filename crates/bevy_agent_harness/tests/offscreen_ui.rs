//! With cameras rendering offscreen (TestApp::with_rendering, headless runs)
//! UI must still be drawn and injected clicks must reach it through picking.
//! Needs a GPU.

use bevy::prelude::*;
use bevy_agent_harness::testing::TestApp;

#[derive(Resource, Default)]
struct Clicks(u32);

fn ui(app: &mut App) {
    app.init_resource::<Clicks>();
    app.add_systems(Startup, |mut commands: Commands| {
        commands.spawn(Camera2d);
        commands
            .spawn((
                Button,
                BackgroundColor(Color::srgb(1.0, 0.0, 0.0)),
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(100.0),
                    top: Val::Px(100.0),
                    width: Val::Px(100.0),
                    height: Val::Px(50.0),
                    ..default()
                },
            ))
            .observe(|_: On<Pointer<Click>>, mut clicks: ResMut<Clicks>| clicks.0 += 1);
    });
}

#[test]
fn ui_is_drawn_and_clickable_when_rendering_offscreen() {
    let mut app = TestApp::with_rendering(400, 300);
    app.add_plugins(ui);
    app.advance(5);

    // UI without an explicit camera still gets drawn (Bevy's default UI camera
    // is "the one targeting the primary window", which no longer exists).
    let shot = app.screenshot();
    let inside = shot.get_pixel(150, 125);
    let outside = shot.get_pixel(350, 250);
    assert!(inside[0] > 200 && inside[1] < 50, "button pixel {inside:?}");
    assert!(outside[0] < 100, "background pixel {outside:?}");

    app.click(Vec2::new(350.0, 250.0)); // outside the button
    assert_eq!(app.resource::<Clicks>().0, 0);

    // Processed by the time `click` returns, like every other input helper.
    app.click(Vec2::new(150.0, 125.0));
    assert_eq!(app.resource::<Clicks>().0, 1);

    // Known limit: bevy_ui's `Interaction` only follows cameras that target a
    // window, so it does not react here. If this starts failing, Bevy changed
    // and the caveat in the skill can go.
    let button = app.single::<Button>();
    app.move_cursor(Vec2::new(150.0, 125.0)).advance(2);
    assert_eq!(*app.component::<Interaction>(button), Interaction::None);
}

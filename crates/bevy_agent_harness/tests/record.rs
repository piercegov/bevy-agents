//! Video recording through TestApp. Needs a wgpu adapter and `ffmpeg`; the
//! test is skipped when ffmpeg is not installed.

use bevy::prelude::*;
use bevy_agent_harness::testing::TestApp;

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[derive(Component)]
struct Mover;

fn scene(app: &mut App) {
    app.add_systems(Startup, |mut commands: Commands| {
        commands.spawn(Camera2d);
        commands.spawn((
            Mover,
            Sprite::from_color(Color::srgb(1.0, 0.0, 0.0), Vec2::splat(40.0)),
            Transform::from_xyz(-80.0, 0.0, 0.0),
        ));
    });
    app.add_systems(
        Update,
        |time: Res<Time>, mut q: Query<&mut Transform, With<Mover>>| {
            for mut t in &mut q {
                t.translation.x += 160.0 * time.delta_secs();
            }
        },
    );
}

#[test]
fn records_one_video_frame_per_game_frame() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not found");
        return;
    }
    let path = std::env::temp_dir().join(format!("bevy_agent_record_{}.mp4", std::process::id()));
    let _ = std::fs::remove_file(&path);

    // Odd height: the encoder has to pad it to an even size.
    let mut app = TestApp::with_rendering(200, 101);
    app.add_plugins(scene);
    app.advance(3);
    app.start_recording(&path);
    app.advance(60);
    let info = app.stop_recording();

    // One video frame per 1/60 s game frame: exactly one second of video.
    assert_eq!(info["frames"], 60, "{info}");
    assert_eq!(info["seconds"], 1.0, "{info}");
    assert_eq!(info["width"], 200);
    assert_eq!(info["height"], 101);
    assert!(info["bytes"].as_u64().unwrap() > 500, "{info}");
    assert!(path.exists());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cursor_overlay_is_drawn_on_the_video_only() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not found");
        return;
    }
    let dir = std::env::temp_dir().join(format!("bevy_agent_cursor_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (video, frame) = (dir.join("cursor.mp4"), dir.join("frame.png"));

    let mut app = TestApp::with_rendering(200, 100);
    app.add_plugins(|app: &mut App| {
        app.insert_resource(ClearColor(Color::srgb(0.0, 0.0, 1.0)))
            .add_systems(Startup, |mut commands: Commands| {
                commands.spawn(Camera2d);
            });
    });
    app.advance(3);
    app.move_cursor(Vec2::new(150.0, 30.0));
    app.start_recording_with(
        bevy_agent_harness::harness::record::RecordOptions::new(&video).cursor(true),
    );
    app.advance(5);
    let info = app.stop_recording();
    assert_eq!(info["frames"], 5, "{info}");

    // The game's own picture has no cursor in it.
    let clean = app.screenshot();
    let body = clean.get_pixel(152, 36);
    assert!(body[2] > 200 && body[0] < 50, "screenshot {body:?}");

    // The video does: white arrow body just below the cursor position.
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(&video)
        .args(["-frames:v", "1", "-update", "1"])
        .arg(&frame)
        .status()
        .unwrap();
    assert!(status.success());
    let shot = image::open(&frame).unwrap().to_rgba8();
    let body = shot.get_pixel(152, 36);
    assert!(body[0] > 180 && body[1] > 180, "video {body:?}");
    let far = shot.get_pixel(40, 70);
    assert!(far[2] > 180 && far[0] < 70, "video background {far:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_unknown_formats() {
    let mut app = TestApp::with_rendering(64, 64);
    app.add_plugins(scene);
    let target = app.render_target().unwrap();
    let err = app.with_app(|app| {
        bevy_agent_harness::harness::record::start(
            app.world_mut(),
            target.into(),
            bevy_agent_harness::harness::record::RecordOptions::new("demo.avi"),
        )
    });
    assert!(err.unwrap_err().contains("unsupported video extension"));
}

//! A tiny 2D game to try `bevy-agent` against.
//!
//! ```text
//! cargo run -p bevy_agent_harness --example demo
//! bevy-agent status
//! bevy-agent ls --short
//! bevy-agent key Space          # player takes damage
//! bevy-agent click 200 100      # spawns a coin at the cursor
//! bevy-agent event trigger Heal '{"amount": 5}'
//! bevy-agent message write SpawnCoin '{"x": 0, "y": 50}'
//! bevy-agent event read PlayerDamaged   # start recording, then `key space`, then read again
//! bevy-agent screenshot -o /tmp/demo.png
//! bevy-agent logs
//! ```

use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy_agent_harness::AgentHarnessPlugin;

#[derive(Component, Reflect, Debug, Default)]
#[reflect(Component)]
struct Player;

#[derive(Component, Reflect, Debug)]
#[reflect(Component)]
struct Health(f32);

#[derive(Component, Reflect, Debug)]
#[reflect(Component)]
struct Speed(f32);

#[derive(Component, Reflect, Debug, Default)]
#[reflect(Component)]
struct Coin;

#[derive(Resource, Reflect, Debug, Default)]
#[reflect(Resource)]
struct Score(u32);

/// Trigger to restore health: `bevy-agent event trigger Heal '{"amount": 5}'`.
#[derive(Event, Reflect, Debug)]
#[reflect(Event)]
struct Heal {
    amount: f32,
}

/// Triggered by the game whenever the player loses health.
#[derive(Event, Reflect, Debug)]
#[reflect(Event)]
struct PlayerDamaged {
    health: f32,
}

/// Write to spawn a coin at a world position.
#[derive(Message, Reflect, Debug)]
#[reflect(Message)]
struct SpawnCoin {
    x: f32,
    y: f32,
}

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(LogPlugin {
            custom_layer: bevy_agent_harness::log_layer,
            ..default()
        }))
        .add_plugins(AgentHarnessPlugin::default())
        .register_type::<Player>()
        .register_type::<Health>()
        .register_type::<Speed>()
        .register_type::<Coin>()
        .register_type::<Score>()
        .register_type::<Heal>()
        .register_type::<PlayerDamaged>()
        .register_type::<SpawnCoin>()
        .add_message::<SpawnCoin>()
        .add_observer(heal)
        .init_resource::<Score>()
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                movement,
                take_damage,
                spawn_coin_on_click,
                spawn_requested_coins,
                collect_coins,
            ),
        )
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn((Name::new("Camera"), Camera2d));
    commands
        .spawn((
            Name::new("Player"),
            Player,
            Health(100.0),
            Speed(200.0),
            Sprite::from_color(Color::srgb(0.2, 0.7, 1.0), Vec2::splat(48.0)),
            Transform::default(),
        ))
        .with_children(|p| {
            p.spawn((
                Name::new("Hat"),
                Sprite::from_color(Color::srgb(1.0, 0.8, 0.2), Vec2::new(32.0, 10.0)),
                Transform::from_xyz(0.0, 30.0, 1.0),
            ));
        });
    for i in 0..3 {
        commands.spawn((
            Name::new(format!("Coin {i}")),
            Coin,
            Sprite::from_color(Color::srgb(1.0, 0.9, 0.1), Vec2::splat(16.0)),
            Transform::from_xyz(-150.0 + 150.0 * i as f32, 120.0, 0.0),
        ));
    }
    info!("demo ready: arrows/WASD move, Space damages, click spawns a coin");
}

fn movement(
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    mut player: Query<(&mut Transform, &Speed), With<Player>>,
) {
    let mut dir = Vec2::ZERO;
    if keys.any_pressed([KeyCode::ArrowLeft, KeyCode::KeyA]) {
        dir.x -= 1.0;
    }
    if keys.any_pressed([KeyCode::ArrowRight, KeyCode::KeyD]) {
        dir.x += 1.0;
    }
    if keys.any_pressed([KeyCode::ArrowUp, KeyCode::KeyW]) {
        dir.y += 1.0;
    }
    if keys.any_pressed([KeyCode::ArrowDown, KeyCode::KeyS]) {
        dir.y -= 1.0;
    }
    for (mut t, speed) in &mut player {
        t.translation += (dir.normalize_or_zero() * speed.0 * time.delta_secs()).extend(0.0);
    }
}

fn heal(heal: On<Heal>, mut player: Query<&mut Health, With<Player>>) {
    for mut h in &mut player {
        h.0 = (h.0 + heal.amount).min(100.0);
        info!("healed {}, health now {}", heal.amount, h.0);
    }
}

fn spawn_requested_coins(mut requests: MessageReader<SpawnCoin>, mut commands: Commands) {
    for request in requests.read() {
        commands.spawn((
            Name::new("Coin (requested)"),
            Coin,
            Sprite::from_color(Color::srgb(1.0, 0.9, 0.1), Vec2::splat(16.0)),
            Transform::from_xyz(request.x, request.y, 0.0),
        ));
    }
}

fn take_damage(
    keys: Res<ButtonInput<KeyCode>>,
    mut player: Query<&mut Health, With<Player>>,
    mut commands: Commands,
) {
    if keys.just_pressed(KeyCode::Space) {
        for mut h in &mut player {
            h.0 -= 10.0;
            commands.trigger(PlayerDamaged { health: h.0 });
            if h.0 <= 0.0 {
                error!("player died");
            } else {
                warn!("player took damage, health now {}", h.0);
            }
        }
    }
}

fn spawn_coin_on_click(
    buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    camera: Query<(&Camera, &GlobalTransform)>,
    mut commands: Commands,
) {
    if !buttons.just_pressed(MouseButton::Left) {
        return;
    }
    let Ok(window) = windows.single() else { return };
    let Some(cursor) = window.cursor_position() else {
        warn!("click with no cursor position");
        return;
    };
    let Ok((camera, cam_tf)) = camera.single() else {
        return;
    };
    let Ok(world_pos) = camera.viewport_to_world_2d(cam_tf, cursor) else {
        return;
    };
    info!("spawning coin at {world_pos}");
    commands.spawn((
        Name::new("Coin (clicked)"),
        Coin,
        Sprite::from_color(Color::srgb(1.0, 0.9, 0.1), Vec2::splat(16.0)),
        Transform::from_translation(world_pos.extend(0.0)),
    ));
}

fn collect_coins(
    mut commands: Commands,
    mut score: ResMut<Score>,
    player: Query<&Transform, With<Player>>,
    coins: Query<(Entity, &Transform), With<Coin>>,
) {
    let Ok(p) = player.single() else { return };
    for (e, t) in &coins {
        if p.translation.distance(t.translation) < 32.0 {
            commands.entity(e).despawn();
            score.0 += 1;
            info!("collected a coin, score {}", score.0);
        }
    }
}

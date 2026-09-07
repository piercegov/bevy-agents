//! Pause / resume / frame stepping of the virtual clock.
//!
//! Pausing uses `Time<Virtual>::pause()`, so systems keep running (rendering,
//! BRP, UI) but anything driven by `Time` or `FixedUpdate` stands still.
//! Stepping temporarily switches to a fixed `TimeUpdateStrategy` so each
//! stepped frame advances by exactly `dt`.

use std::time::Duration;

use bevy::diagnostic::FrameCount;
use bevy::prelude::*;
use bevy::remote::BrpResult;
use bevy::time::{TimeSystems, TimeUpdateStrategy};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{parse_params, status_value};

#[derive(Resource, Default)]
pub struct Stepper {
    /// Whether the harness wants the virtual clock paused when not stepping.
    pub paused: bool,
    /// Frames still to run before pausing again.
    pub remaining: u32,
    /// Virtual delta to use for stepped frames.
    pub dt: Duration,
    saved_strategy: Option<TimeUpdateStrategy>,
    stepping: bool,
}

impl Stepper {
    pub fn step(&mut self, frames: u32, dt: Option<Duration>) {
        self.remaining = self.remaining.saturating_add(frames);
        if let Some(dt) = dt {
            self.dt = dt;
        }
        if self.dt.is_zero() {
            self.dt = Duration::from_secs_f64(1.0 / 60.0);
        }
    }
}

pub(crate) fn plugin(app: &mut App) {
    app.init_resource::<Stepper>()
        .add_systems(First, apply_stepper.before(TimeSystems));
}

fn apply_stepper(
    mut stepper: ResMut<Stepper>,
    mut time: ResMut<Time<Virtual>>,
    mut strategy: ResMut<TimeUpdateStrategy>,
) {
    if stepper.remaining > 0 {
        stepper.remaining -= 1;
        if !stepper.stepping {
            stepper.stepping = true;
            stepper.saved_strategy = Some(std::mem::replace(
                &mut *strategy,
                TimeUpdateStrategy::ManualDuration(stepper.dt),
            ));
        } else {
            *strategy = TimeUpdateStrategy::ManualDuration(stepper.dt);
        }
        time.unpause();
        return;
    }

    if stepper.stepping {
        stepper.stepping = false;
        if let Some(saved) = stepper.saved_strategy.take() {
            *strategy = saved;
        }
    }
    if stepper.paused && !time.is_paused() {
        time.pause();
    }
}

/// `harness.pause`
pub fn pause(In(_): In<Option<Value>>, world: &mut World) -> BrpResult {
    world.resource_mut::<Stepper>().paused = true;
    world.resource_mut::<Time<Virtual>>().pause();
    Ok(status_value(world))
}

/// `harness.resume`
pub fn resume(In(_): In<Option<Value>>, world: &mut World) -> BrpResult {
    {
        let mut s = world.resource_mut::<Stepper>();
        s.paused = false;
        s.remaining = 0;
    }
    world.resource_mut::<Time<Virtual>>().unpause();
    Ok(status_value(world))
}

#[derive(Deserialize, Default)]
struct StepParams {
    #[serde(default)]
    frames: Option<u32>,
    /// Virtual seconds per stepped frame (default 1/60).
    #[serde(default)]
    dt: Option<f64>,
}

/// `harness.step` – pauses (if not already) and runs `frames` frames.
/// Returns the status including `until_frame`, which can be awaited with
/// `harness.wait_frame+watch`.
pub fn step(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: StepParams = parse_params(params)?;
    let frames = p.frames.unwrap_or(1).max(1);
    let dt = p.dt.map(Duration::from_secs_f64);
    let current = world.resource::<FrameCount>().0;
    {
        let mut s = world.resource_mut::<Stepper>();
        s.paused = true;
        s.step(frames, dt);
    }
    let mut status = status_value(world);
    // FrameCount increments in `Last`; the current frame's `Last` has already run
    // by the time BRP handlers execute (RemoteLast), so `frames` more full frames
    // means FrameCount reaches current + frames + 1 once they have completed.
    status["until_frame"] = json!(current.wrapping_add(frames).wrapping_add(1));
    Ok(status)
}

#[derive(Deserialize, Default)]
struct TimeParams {
    #[serde(default)]
    speed: Option<f64>,
    #[serde(default)]
    paused: Option<bool>,
}

/// `harness.time`
pub fn time(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: TimeParams = parse_params(params)?;
    if let Some(speed) = p.speed {
        if speed <= 0.0 || !speed.is_finite() {
            return Err(super::invalid_params("`speed` must be a positive number"));
        }
        world
            .resource_mut::<Time<Virtual>>()
            .set_relative_speed_f64(speed);
    }
    if let Some(paused) = p.paused {
        world.resource_mut::<Stepper>().paused = paused;
        let mut t = world.resource_mut::<Time<Virtual>>();
        if paused {
            t.pause();
        } else {
            t.unpause();
        }
    }
    Ok(status_value(world))
}

#[derive(Deserialize, Default)]
struct WaitParams {
    until_frame: u32,
}

/// `harness.wait_frame+watch` – responds once `FrameCount >= until_frame`.
pub fn wait_frame(In(params): In<Option<Value>>, world: &mut World) -> BrpResult<Option<Value>> {
    let p: WaitParams = parse_params(params)?;
    let current = world.resource::<FrameCount>().0;
    // Wrapping-aware "reached" test.
    if current.wrapping_sub(p.until_frame) < u32::MAX / 2 {
        Ok(Some(status_value(world)))
    } else {
        Ok(None)
    }
}

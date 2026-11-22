//! # bevy_agent_harness
//!
//! Tooling that lets an AI agent (or a human at a terminal) inspect, drive and
//! verify a running Bevy game the way web developers verify a page in a browser.
//!
//! The crate has three layers that can be used independently:
//!
//! 1. [`AgentHarnessPlugin`] – add it to your game. It enables the Bevy Remote
//!    Protocol (BRP) over HTTP and registers extra `harness.*` methods for
//!    screenshots, input injection, log retrieval, pausing and frame stepping.
//! 2. [`client::BrpClient`] and the `bevy-agent` CLI (feature `cli`, on by default) –
//!    talk to a running game from outside the process.
//! 3. [`testing::TestApp`] – an in-process harness for integration tests:
//!    `advance(frames)`, `tap(KeyCode::Space)`, `assert_component::<Health>(..)`
//!    and golden screenshot diffs using a headless GPU renderer.
//!
//! ## Game setup
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy::log::LogPlugin;
//! use bevy_agent_harness::AgentHarnessPlugin;
//!
//! App::new()
//!     .add_plugins(DefaultPlugins.set(LogPlugin {
//!         // Optional: lets `bevy-agent logs` read your game's log output.
//!         custom_layer: bevy_agent_harness::log_layer,
//!         ..default()
//!     }))
//!     .add_plugins(AgentHarnessPlugin::default())
//!     .run();
//! ```
//!
//! Then, from a shell:
//!
//! ```text
//! bevy-agent status
//! bevy-agent ls
//! bevy-agent get 4294967298
//! bevy-agent key Space
//! bevy-agent screenshot -o shot.png
//! bevy-agent logs --level warn
//! ```

pub mod harness;
pub mod testing;

#[cfg(feature = "client")]
pub mod client;

#[cfg(feature = "cli")]
pub mod share;

pub use harness::AgentHarnessPlugin;
pub use harness::headless::agent_plugins;
pub use harness::logs::{LogBuffer, LogEntry, log_layer};

/// Fully-qualified type paths of components the tools use frequently.
pub mod type_paths {
    pub const NAME: &str = "bevy_ecs::name::Name";
    pub const CHILD_OF: &str = "bevy_ecs::hierarchy::ChildOf";
    pub const CHILDREN: &str = "bevy_ecs::hierarchy::Children";
    pub const TRANSFORM: &str = "bevy_transform::components::transform::Transform";
    pub const GLOBAL_TRANSFORM: &str =
        "bevy_transform::components::global_transform::GlobalTransform";
    pub const CAMERA: &str = "bevy_camera::Camera";
}

/// `a::b::Foo<c::Bar>` → `Foo<Bar>` (available without the `client` feature).
pub fn client_free_short_name(path: &str) -> String {
    let mut out = String::new();
    let mut ident = String::new();
    fn flush(ident: &mut String, out: &mut String) {
        if !ident.is_empty() {
            out.push_str(ident.rsplit("::").next().unwrap_or(ident));
            ident.clear();
        }
    }
    for ch in path.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == ':' {
            ident.push(ch);
        } else {
            flush(&mut ident, &mut out);
            out.push(ch);
        }
    }
    flush(&mut ident, &mut out);
    out
}

/// Parse an entity written either as the raw `u64` BRP form (`4294967298`)
/// or as `index v generation` (`2v1`, the form Bevy prints).
pub fn parse_entity(s: &str) -> Option<bevy::ecs::entity::Entity> {
    use bevy::ecs::entity::Entity;
    let s = s.trim();
    if let Some((index, generation)) = s.split_once('v') {
        let index: u32 = index.parse().ok()?;
        let generation: u64 = generation.parse().ok()?;
        // Bevy stores the index bitwise-inverted (NonMaxU32 niche).
        let bits = (!index) as u64 | (generation << 32);
        return Entity::try_from_bits(bits).filter(|e| e.index().index() == index);
    }
    let bits: u64 = s.parse().ok()?;
    Entity::try_from_bits(bits)
}

/// Format BRP entity bits as `index v generation` (matches Bevy's `Display`).
pub fn format_entity(bits: u64) -> String {
    match bevy::ecs::entity::Entity::try_from_bits(bits) {
        Some(e) => e.to_string(),
        None => format!("invalid({bits})"),
    }
}

/// Entity index (row) from BRP entity bits; useful for sorting.
pub fn entity_index(bits: u64) -> u32 {
    bevy::ecs::entity::Entity::try_from_bits(bits)
        .map(|e| e.index().index())
        .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_roundtrip() {
        let e = parse_entity("2v1").unwrap();
        assert_eq!(e.index().index(), 2);
        assert_eq!(e.to_string(), "2v1");
        assert_eq!(format_entity(e.to_bits()), "2v1");
        assert_eq!(parse_entity(&e.to_bits().to_string()), Some(e));
        assert_eq!(entity_index(e.to_bits()), 2);
        assert!(parse_entity("nope").is_none());
    }
}

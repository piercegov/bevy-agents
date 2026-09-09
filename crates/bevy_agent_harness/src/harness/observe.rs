//! Compatibility replacement for Bevy 0.19.1's observer watcher, whose
//! serialization-error placeholder panics when drained. Keep the wire format
//! for successful events, bound retained data, and report failures as data.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use bevy::ecs::reflect::ReflectEvent;
use bevy::prelude::*;
use bevy::reflect::serde::TypedReflectSerializer;
use bevy::remote::{BrpError, BrpResult, builtin_methods::BrpObserveParams};
use serde_json::{Value, json};

const MAX_OBSERVERS: usize = 64;
const MAX_EVENTS: usize = 1024;

#[derive(Default)]
struct Pending {
    events: VecDeque<Value>,
    dropped: usize,
}

#[derive(Resource, Default)]
struct Observers(HashMap<(String, Option<Entity>), Arc<Mutex<Pending>>>);

pub(super) fn watch(In(params): In<Option<Value>>, world: &mut World) -> BrpResult<Option<Value>> {
    let params: BrpObserveParams = bevy::remote::builtin_methods::parse_some(params)?;
    let key = (params.event.clone(), params.entity);
    world.init_resource::<Observers>();
    if !world.resource::<Observers>().0.contains_key(&key) {
        if world.resource::<Observers>().0.len() >= MAX_OBSERVERS {
            return Err(BrpError::resource_error(
                "event observer limit reached (64)",
            ));
        }
        if let Some(entity) = params.entity {
            world.get_entity(entity).map_err(BrpError::resource_error)?;
        }
        let registry = world.resource::<AppTypeRegistry>().clone();
        let event = registry
            .read()
            .get_with_type_path(&params.event)
            .and_then(|r| r.data::<ReflectEvent>())
            .cloned()
            .ok_or_else(|| {
                BrpError::resource_error(format!(
                    "Event `{}` needs registration with ReflectEvent metadata",
                    params.event
                ))
            })?;
        let pending = Arc::new(Mutex::new(Pending::default()));
        let sink = pending.clone();
        let event_name = params.event;
        let observer = event.create_observer(Box::new(move |payload, _world| {
            let value =
                serde_json::to_value(TypedReflectSerializer::new(payload, &registry.read()))
                    .unwrap_or_else(
                        |error| json!({"$error": error.to_string(), "$event": event_name}),
                    );
            let mut pending = sink.lock().unwrap();
            if pending.events.len() == MAX_EVENTS {
                pending.events.pop_front();
                pending.dropped += 1;
            }
            pending.events.push_back(value);
        }));
        world.spawn(match params.entity {
            Some(entity) => observer.with_entity(entity),
            None => observer,
        });
        world
            .resource_mut::<Observers>()
            .0
            .insert(key.clone(), pending);
    }
    let observers = world.resource::<Observers>();
    let mut pending = observers.0[&key].lock().unwrap();
    if pending.events.is_empty() {
        return Ok(None);
    }
    let mut events = Vec::new();
    if pending.dropped > 0 {
        events.push(json!({"$error": "event buffer overflow", "$dropped": pending.dropped}));
        pending.dropped = 0;
    }
    events.extend(pending.events.drain(..));
    Ok(Some(Value::Array(events)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Reflect, Clone)]
    #[reflect(opaque)]
    struct Opaque;
    #[derive(Event, Reflect)]
    #[reflect(Event)]
    struct Broken(Opaque);
    #[derive(Event, Reflect)]
    #[reflect(Event)]
    struct Good {
        value: u32,
    }

    #[derive(EntityEvent, Reflect)]
    #[reflect(Event)]
    struct Targeted {
        entity: Entity,
        value: u32,
    }

    #[test]
    fn targeted_observation_and_registration_limits() {
        let mut app = App::new();
        app.register_type::<Targeted>();
        let entity = app.world_mut().spawn_empty().id();
        let other = app.world_mut().spawn_empty().id();
        let params = Some(json!({"event": Targeted::type_path(), "entity": entity}));
        watch(In(params.clone()), app.world_mut()).unwrap();
        app.world_mut().trigger(Targeted {
            entity: other,
            value: 1,
        });
        assert!(
            watch(In(params.clone()), app.world_mut())
                .unwrap()
                .is_none()
        );
        app.world_mut().trigger(Targeted { entity, value: 2 });
        assert_eq!(
            watch(In(params), app.world_mut()).unwrap().unwrap()[0]["value"],
            2
        );
        for _ in 1..MAX_OBSERVERS {
            let entity = app.world_mut().spawn_empty().id();
            watch(
                In(Some(
                    json!({"event": Targeted::type_path(), "entity": entity}),
                )),
                app.world_mut(),
            )
            .unwrap();
        }
        let entity = app.world_mut().spawn_empty().id();
        assert!(
            watch(
                In(Some(
                    json!({"event": Targeted::type_path(), "entity": entity})
                )),
                app.world_mut()
            )
            .is_err()
        );
        assert_eq!(app.world().resource::<Observers>().0.len(), MAX_OBSERVERS);
    }

    #[test]
    fn errors_are_data_and_overflow_is_visible() {
        let mut app = App::new();
        app.register_type::<Broken>().register_type::<Good>();
        let bad = Some(json!({"event": Broken::type_path()}));
        assert!(watch(In(bad.clone()), app.world_mut()).unwrap().is_none());
        app.world_mut().trigger(Broken(Opaque));
        let result = watch(In(bad), app.world_mut()).unwrap().unwrap();
        assert!(
            result[0]["$error"]
                .as_str()
                .unwrap()
                .contains("ReflectSerialize")
        );
        let good = Some(json!({"event": Good::type_path()}));
        watch(In(good.clone()), app.world_mut()).unwrap();
        for value in 0..1025 {
            app.world_mut().trigger(Good { value });
        }
        let result = watch(In(good.clone()), app.world_mut()).unwrap().unwrap();
        assert_eq!(result[0]["$dropped"], 1);
        assert_eq!(result[1]["value"], 1);
        assert_eq!(result[1024]["value"], 1024);
        assert!(watch(In(good), app.world_mut()).unwrap().is_none());
    }
}

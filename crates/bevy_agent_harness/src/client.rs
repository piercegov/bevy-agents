//! A small blocking client for the Bevy Remote Protocol (JSON-RPC over HTTP),
//! with typed helpers for the built-in `world.*` methods and the `harness.*`
//! methods added by [`AgentHarnessPlugin`](crate::AgentHarnessPlugin).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::type_paths;

/// Default BRP endpoint (`RemoteHttpPlugin` default address and port).
pub const DEFAULT_URL: &str = "http://127.0.0.1:15702";

/// An error object returned by the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BRP error {}: {}", self.code, self.message)?;
        if let Some(d) = &self.data {
            write!(f, " ({d})")?;
        }
        Ok(())
    }
}
impl std::error::Error for RpcError {}

#[derive(Deserialize)]
struct RpcResponse {
    #[allow(dead_code)]
    id: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

impl RpcResponse {
    fn into_result(self) -> Result<Value, RpcError> {
        if let Some(e) = self.error {
            Err(e)
        } else {
            Ok(self.result.unwrap_or(Value::Null))
        }
    }
}

/// Blocking BRP client.
pub struct BrpClient {
    url: String,
    agent: ureq::Agent,
    next_id: AtomicU64,
    components: Mutex<Option<Vec<String>>>,
    resources: Mutex<Option<Vec<String>>>,
    /// Timeout for ordinary (non-watch) calls.
    pub timeout: Duration,
}

impl BrpClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            agent: ureq::AgentBuilder::new().build(),
            next_id: AtomicU64::new(1),
            components: Mutex::new(None),
            resources: Mutex::new(None),
            timeout: Duration::from_secs(10),
        }
    }

    /// Connect to `http://127.0.0.1:<port>`.
    pub fn local(port: u16) -> Self {
        Self::new(format!("http://127.0.0.1:{port}"))
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn request_body(&self, method: &str, params: Option<Value>) -> Value {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut body = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(p) = params {
            body["params"] = p;
        }
        body
    }

    /// Perform one JSON-RPC call.
    pub fn call(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let body = self.request_body(method, params);
        let resp = self
            .agent
            .post(&self.url)
            .timeout(self.timeout)
            .send_json(body)
            .map_err(|e| match e {
                ureq::Error::Status(code, r) => {
                    let text = r.into_string().unwrap_or_default();
                    anyhow!("HTTP {code} from {}: {text}", self.url)
                }
                ureq::Error::Transport(t) => anyhow!(
                    "could not reach {} ({t}). Is the game running with AgentHarnessPlugin?",
                    self.url
                ),
            })?;
        let parsed: RpcResponse = resp.into_json().context("parsing BRP response")?;
        Ok(parsed.into_result()?)
    }

    /// Perform several calls in one HTTP request. Each result is independent.
    pub fn call_batch(
        &self,
        calls: Vec<(String, Option<Value>)>,
    ) -> Result<Vec<Result<Value, RpcError>>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let bodies: Vec<Value> = calls
            .into_iter()
            .map(|(m, p)| self.request_body(&m, p))
            .collect();
        let resp = self
            .agent
            .post(&self.url)
            .timeout(self.timeout)
            .send_json(Value::Array(bodies))
            .map_err(|e| anyhow!("BRP batch request failed: {e}"))?;
        let parsed: Vec<RpcResponse> = resp.into_json().context("parsing BRP batch response")?;
        Ok(parsed.into_iter().map(RpcResponse::into_result).collect())
    }

    /// Call a `+watch` method. `on_event` is invoked for each streamed response and
    /// returns `true` to keep listening. `timeout` bounds the whole watch.
    pub fn watch(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Option<Duration>,
        mut on_event: impl FnMut(Result<Value, RpcError>) -> bool,
    ) -> Result<()> {
        let body = self.request_body(method, params);
        let resp = self
            .agent
            .post(&self.url)
            .timeout(timeout.unwrap_or(Duration::from_secs(60 * 60 * 24)))
            .send_json(body)
            .map_err(|e| anyhow!("BRP watch request failed: {e}"))?;
        let reader = BufReader::new(resp.into_reader());
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    bail!("timed out waiting for {method}");
                }
                Err(e) => return Err(e.into()),
            };
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let parsed: RpcResponse =
                serde_json::from_str(data).context("parsing streamed BRP response")?;
            if !on_event(parsed.into_result()) {
                break;
            }
        }
        Ok(())
    }

    /// Call a `+watch` method and return its first response.
    pub fn watch_first(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let mut first = None;
        self.watch(method, params, Some(timeout), |r| {
            first = Some(r);
            false
        })?;
        match first {
            Some(Ok(v)) => Ok(v),
            Some(Err(e)) => Err(e.into()),
            None => bail!("stream for {method} ended without a response"),
        }
    }

    // ---- discovery ------------------------------------------------------

    pub fn discover(&self) -> Result<Value> {
        self.call("rpc.discover", None)
    }

    /// Names of all remote methods.
    pub fn methods(&self) -> Result<Vec<String>> {
        let doc = self.discover()?;
        Ok(doc["methods"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["name"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    // ---- component name resolution ---------------------------------------

    /// All registered component type paths (cached).
    pub fn registered_components(&self) -> Result<Vec<String>> {
        let mut cache = self.components.lock().unwrap();
        if let Some(c) = cache.as_ref() {
            return Ok(c.clone());
        }
        let list: Vec<String> = serde_json::from_value(self.call("world.list_components", None)?)?;
        *cache = Some(list.clone());
        Ok(list)
    }

    /// All registered resource type paths (cached).
    pub fn registered_resources(&self) -> Result<Vec<String>> {
        let mut cache = self.resources.lock().unwrap();
        if let Some(c) = cache.as_ref() {
            return Ok(c.clone());
        }
        let list: Vec<String> = serde_json::from_value(self.call("world.list_resources", None)?)?;
        *cache = Some(list.clone());
        Ok(list)
    }

    /// Expand a short type name (`Transform`, `transform::Transform`) to its
    /// fully-qualified path. Full paths pass through unchanged. Errors if the
    /// short name is ambiguous or unknown.
    pub fn resolve_component(&self, name: &str) -> Result<String> {
        resolve_in(name, &self.registered_components()?, "component")
    }

    pub fn resolve_components(&self, names: &[String]) -> Result<Vec<String>> {
        names.iter().map(|n| self.resolve_component(n)).collect()
    }

    pub fn resolve_resource(&self, name: &str) -> Result<String> {
        resolve_in(name, &self.registered_resources()?, "resource")
    }

    // ---- world.* ---------------------------------------------------------

    pub fn list_components(&self, entity: Option<u64>) -> Result<Vec<String>> {
        let params = entity.map(|e| json!({ "entity": e }));
        Ok(serde_json::from_value(
            self.call("world.list_components", params)?,
        )?)
    }

    /// Components of one entity. With `strict == false`, missing/unreflectable
    /// components are reported under `errors`.
    pub fn get_components(
        &self,
        entity: u64,
        components: &[String],
        strict: bool,
    ) -> Result<Value> {
        self.call(
            "world.get_components",
            Some(json!({ "entity": entity, "components": components, "strict": strict })),
        )
    }

    /// All reflectable components of an entity as a map.
    pub fn get_all_components(&self, entity: u64) -> Result<Map<String, Value>> {
        let names = self.list_components(Some(entity))?;
        let v = self.get_components(entity, &names, false)?;
        Ok(component_results(&v))
    }

    pub fn query(&self, data: Value, filter: Value, strict: bool) -> Result<Vec<Value>> {
        let v = self.call(
            "world.query",
            Some(json!({ "data": data, "filter": filter, "strict": strict })),
        )?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    pub fn spawn(&self, components: Map<String, Value>) -> Result<u64> {
        let v = self.call(
            "world.spawn_entity",
            Some(json!({ "components": components })),
        )?;
        v["entity"]
            .as_u64()
            .ok_or_else(|| anyhow!("unexpected spawn result: {v}"))
    }

    pub fn insert(&self, entity: u64, components: Map<String, Value>) -> Result<()> {
        self.call(
            "world.insert_components",
            Some(json!({ "entity": entity, "components": components })),
        )?;
        Ok(())
    }

    pub fn remove(&self, entity: u64, components: &[String]) -> Result<()> {
        self.call(
            "world.remove_components",
            Some(json!({ "entity": entity, "components": components })),
        )?;
        Ok(())
    }

    pub fn despawn(&self, entity: u64) -> Result<()> {
        self.call("world.despawn_entity", Some(json!({ "entity": entity })))?;
        Ok(())
    }

    pub fn mutate(&self, entity: u64, component: &str, path: &str, value: Value) -> Result<()> {
        self.call(
            "world.mutate_components",
            Some(json!({ "entity": entity, "component": component, "path": path, "value": value })),
        )?;
        Ok(())
    }

    pub fn reparent(&self, entities: &[u64], parent: Option<u64>) -> Result<()> {
        let mut p = json!({ "entities": entities });
        if let Some(parent) = parent {
            p["parent"] = json!(parent);
        }
        self.call("world.reparent_entities", Some(p))?;
        Ok(())
    }

    pub fn get_resource(&self, resource: &str) -> Result<Value> {
        let v = self.call("world.get_resources", Some(json!({ "resource": resource })))?;
        Ok(v.get("value").cloned().unwrap_or(v))
    }

    pub fn insert_resource(&self, resource: &str, value: Value) -> Result<()> {
        self.call(
            "world.insert_resources",
            Some(json!({ "resource": resource, "value": value })),
        )?;
        Ok(())
    }

    pub fn remove_resource(&self, resource: &str) -> Result<()> {
        self.call(
            "world.remove_resources",
            Some(json!({ "resource": resource })),
        )?;
        Ok(())
    }

    pub fn mutate_resource(&self, resource: &str, path: &str, value: Value) -> Result<()> {
        self.call(
            "world.mutate_resources",
            Some(json!({ "resource": resource, "path": path, "value": value })),
        )?;
        Ok(())
    }

    pub fn schema(&self, params: Value) -> Result<Value> {
        self.call("registry.schema", Some(params))
    }

    // ---- events, messages, schedules --------------------------------------

    /// Type paths of events that can be triggered and observed (`#[reflect(Event)]`).
    pub fn registered_events(&self) -> Result<Vec<String>> {
        Ok(serde_json::from_value(self.call(
            "harness.types",
            Some(json!({ "kind": "event" })),
        )?)?)
    }

    /// Type paths of messages that can be written (`#[reflect(Message)]`).
    pub fn registered_messages(&self) -> Result<Vec<String>> {
        Ok(serde_json::from_value(self.call(
            "harness.types",
            Some(json!({ "kind": "message" })),
        )?)?)
    }

    pub fn resolve_event(&self, name: &str) -> Result<String> {
        resolve_in(name, &self.registered_events()?, "event")
    }

    pub fn resolve_message(&self, name: &str) -> Result<String> {
        resolve_in(name, &self.registered_messages()?, "message")
    }

    /// Trigger an observer event. `event` is a full type path; `value` its
    /// reflected shape (omit for unit-like events).
    pub fn trigger_event(&self, event: &str, value: Option<Value>) -> Result<()> {
        let mut p = json!({ "event": event });
        if let Some(v) = value {
            p["value"] = v;
        }
        self.call("world.trigger_event", Some(p))?;
        Ok(())
    }

    /// Write a buffered message; systems read it during the next frame.
    pub fn write_message(&self, message: &str, value: Option<Value>) -> Result<()> {
        let mut p = json!({ "message": message });
        if let Some(v) = value {
            p["value"] = v;
        }
        self.call("world.write_message", Some(p))?;
        Ok(())
    }

    /// Events recorded since the previous call (the first call starts recording).
    /// Returns `{ recording_started, events, dropped }`.
    pub fn recorded_events(&self, event: &str, entity: Option<u64>) -> Result<Value> {
        self.call("harness.events", Some(observe_params(event, entity)))
    }

    /// `{ schedule_labels, unavailable_schedule_labels, empty_schedule_labels }`.
    pub fn schedule_list(&self) -> Result<Value> {
        self.call("schedule.list", None)
    }

    /// Systems, sets, ordering and conflicts of one schedule (`schedule_data`).
    pub fn schedule_graph(&self, label: &str) -> Result<Value> {
        let v = self.call("schedule.graph", Some(json!({ "schedule_label": label })))?;
        Ok(v.get("schedule_data").cloned().unwrap_or(v))
    }

    /// Entity hierarchy with names and (optionally) component lists.
    pub fn entity_tree(&self, with_components: bool) -> Result<Vec<EntityNode>> {
        let rows = self.query(
            json!({ "option": [type_paths::NAME, type_paths::CHILD_OF] }),
            json!({}),
            false,
        )?;
        let mut nodes: BTreeMap<u64, EntityNode> = BTreeMap::new();
        for row in &rows {
            let Some(id) = row["entity"].as_u64() else {
                continue;
            };
            let comps = &row["components"];
            let name = comps[type_paths::NAME].as_str().map(String::from);
            let parent = comps[type_paths::CHILD_OF].as_u64();
            nodes.insert(
                id,
                EntityNode {
                    id,
                    name,
                    parent,
                    children: Vec::new(),
                    components: Vec::new(),
                },
            );
        }
        let ids: Vec<u64> = nodes.keys().copied().collect();
        for id in &ids {
            if let Some(parent) = nodes[id].parent
                && let Some(p) = nodes.get_mut(&parent)
            {
                p.children.push(*id);
            }
        }
        if with_components {
            let calls = ids
                .iter()
                .map(|id| {
                    (
                        "world.list_components".to_string(),
                        Some(json!({ "entity": id })),
                    )
                })
                .collect();
            for (id, result) in ids.iter().zip(self.call_batch(calls)?) {
                if let Ok(v) = result {
                    let list: Vec<String> = serde_json::from_value(v).unwrap_or_default();
                    nodes.get_mut(id).unwrap().components = list;
                }
            }
        }
        Ok(nodes.into_values().collect())
    }

    // ---- harness.* -------------------------------------------------------

    pub fn status(&self) -> Result<Value> {
        self.call("harness.status", None)
    }

    /// Take a screenshot; blocks until the image is ready. Returns the
    /// `harness.screenshot.result` payload (`path` or `png_base64`).
    pub fn screenshot(&self, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.call("harness.screenshot", Some(params))?;
        let id = id["id"]
            .as_u64()
            .ok_or_else(|| anyhow!("bad screenshot id"))?;
        self.watch_first(
            "harness.screenshot.result+watch",
            Some(json!({ "id": id })),
            timeout,
        )
    }

    /// Start recording a video on the game's machine. `params`: `path`
    /// (mp4, mov, mkv, webm or gif) and optionally `fps`, `max_seconds`,
    /// `camera`, `window`.
    pub fn record_start(&self, params: Value) -> Result<Value> {
        self.call("harness.record.start", Some(params))
    }

    /// Stop recording and block until the file is finished. Returns `path`,
    /// `width`, `height`, `fps`, `frames`, `seconds` and `bytes`.
    pub fn record_stop(&self, timeout: Duration) -> Result<Value> {
        self.call("harness.record.stop", None)?;
        self.watch_first("harness.record.result+watch", None, timeout)
    }

    /// Screenshot as raw PNG bytes (never touches the game's filesystem).
    pub fn screenshot_png(&self, params: Value, timeout: Duration) -> Result<Vec<u8>> {
        use base64::Engine;
        let mut params = params;
        if let Some(obj) = params.as_object_mut() {
            obj.remove("path");
        }
        let v = self.screenshot(params, timeout)?;
        let b64 = v["png_base64"]
            .as_str()
            .ok_or_else(|| anyhow!("no png in result"))?;
        Ok(base64::engine::general_purpose::STANDARD.decode(b64)?)
    }

    pub fn key(&self, params: Value) -> Result<Value> {
        self.call("harness.input.key", Some(params))
    }
    pub fn mouse(&self, params: Value) -> Result<Value> {
        self.call("harness.input.mouse", Some(params))
    }
    pub fn cursor(&self, params: Value) -> Result<Value> {
        self.call("harness.input.cursor", Some(params))
    }
    /// Move the cursor along a path over several frames, optionally holding a
    /// button: `to`, and optionally `from`, `via`, `frames`, `button`, `ease`.
    pub fn drag(&self, params: Value) -> Result<Value> {
        self.call("harness.input.drag", Some(params))
    }
    pub fn scroll(&self, params: Value) -> Result<Value> {
        self.call("harness.input.scroll", Some(params))
    }
    pub fn logs(&self, params: Value) -> Result<Value> {
        self.call("harness.logs", Some(params))
    }
    pub fn pause(&self) -> Result<Value> {
        self.call("harness.pause", None)
    }
    pub fn resume(&self) -> Result<Value> {
        self.call("harness.resume", None)
    }
    pub fn set_time(&self, params: Value) -> Result<Value> {
        self.call("harness.time", Some(params))
    }

    /// Current frame counter.
    pub fn frame(&self) -> Result<u64> {
        Ok(self.status()?["frame"].as_u64().unwrap_or(0))
    }

    /// Block until the game has rendered `frames` more frames.
    pub fn wait_frames(&self, frames: u32, timeout: Duration) -> Result<Value> {
        let until = (self.frame()? as u32).wrapping_add(frames);
        self.wait_until_frame(until, timeout)
    }

    pub fn wait_until_frame(&self, until_frame: u32, timeout: Duration) -> Result<Value> {
        self.watch_first(
            "harness.wait_frame+watch",
            Some(json!({ "until_frame": until_frame })),
            timeout,
        )
    }

    /// Step `frames` frames (pausing afterwards) and, if `wait`, block until done.
    pub fn step(
        &self,
        frames: u32,
        dt: Option<f64>,
        wait: bool,
        timeout: Duration,
    ) -> Result<Value> {
        let mut p = json!({ "frames": frames });
        if let Some(dt) = dt {
            p["dt"] = json!(dt);
        }
        let status = self.call("harness.step", Some(p))?;
        if !wait {
            return Ok(status);
        }
        let until = status["until_frame"].as_u64().unwrap_or(0) as u32;
        self.wait_until_frame(until, timeout)
    }
}

/// One entity in the hierarchy returned by [`BrpClient::entity_tree`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityNode {
    pub id: u64,
    pub name: Option<String>,
    pub parent: Option<u64>,
    pub children: Vec<u64>,
    pub components: Vec<String>,
}

/// Params shared by `world.observe+watch` and `harness.events`.
pub fn observe_params(event: &str, entity: Option<u64>) -> Value {
    let mut p = json!({ "event": event });
    if let Some(e) = entity {
        p["entity"] = json!(e);
    }
    p
}

fn resolve_in(name: &str, registry: &[String], kind: &str) -> Result<String> {
    if registry.iter().any(|r| r == name) {
        return Ok(name.to_string());
    }
    let short = short_name(name);
    // Match "Foo" against "a::b::Foo", and "b::Foo" against "a::b::Foo".
    let mut matches: Vec<&String> = registry
        .iter()
        .filter(|r| {
            r.ends_with(name) && r[..r.len() - name.len()].ends_with("::")
                || short_name(r) == short && !name.contains("::")
        })
        .collect();
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => {
            // Suggest close names.
            let lower = short.to_ascii_lowercase();
            let mut suggestions: Vec<&String> = registry
                .iter()
                .filter(|r| short_name(r).to_ascii_lowercase().contains(&lower))
                .take(8)
                .collect();
            suggestions.sort();
            if suggestions.is_empty() {
                bail!("unknown {kind} `{name}`")
            } else {
                bail!(
                    "unknown {kind} `{name}`; did you mean one of: {}",
                    suggestions
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
        many => bail!(
            "{kind} `{name}` is ambiguous: {}",
            many.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `a::b::Foo<c::Bar>` → `Foo<Bar>`.
pub fn short_name(path: &str) -> String {
    crate::client_free_short_name(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names() {
        assert_eq!(
            short_name("bevy_transform::components::transform::Transform"),
            "Transform"
        );
        assert_eq!(short_name("bevy_ecs::hierarchy::Children"), "Children");
        assert_eq!(
            short_name("bevy_asset::handle::Handle<bevy_image::image::Image>"),
            "Handle<Image>"
        );
    }

    #[test]
    fn resolve() {
        let reg = vec![
            "a::b::Transform".to_string(),
            "a::c::Health".to_string(),
            "x::y::Health".to_string(),
        ];
        assert_eq!(
            resolve_in("Transform", &reg, "component").unwrap(),
            "a::b::Transform"
        );
        assert_eq!(
            resolve_in("b::Transform", &reg, "component").unwrap(),
            "a::b::Transform"
        );
        assert_eq!(
            resolve_in("c::Health", &reg, "component").unwrap(),
            "a::c::Health"
        );
        assert!(
            resolve_in("Health", &reg, "component")
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        assert!(resolve_in("Nope", &reg, "component").is_err());
    }
}

/// Flatten a lenient BRP response without discarding diagnostic codes or data.
/// Presence is only definitive for COMPONENT_NOT_PRESENT; list/read can race.
pub fn component_results(response: &Value) -> Map<String, Value> {
    let mut result = response["components"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if let Some(errors) = response["errors"].as_object() {
        for (name, error) in errors {
            let missing = error["code"].as_i64()
                == Some(i64::from(bevy::remote::error_codes::COMPONENT_NOT_PRESENT));
            result.insert(
                name.clone(),
                json!({
                    "$error": error["message"].as_str().unwrap_or("component inspection failed"),
                    "$code": error["code"],
                    "$data": error["data"],
                    "$status": if missing { "missing" } else { "unreadable_or_unknown" },
                }),
            );
        }
    }
    result
}

#[cfg(test)]
mod component_diagnostic_tests {
    use super::*;

    #[test]
    fn flattening_preserves_partial_success_and_error_details() {
        let result = component_results(&json!({
            "components": {"Health": 42},
            "errors": {
                "Missing": {"code": -23403, "message": "not present"},
                "Opaque": {"code": -23402, "message": "cannot serialize", "data": {"field": "handle"}}
            }
        }));
        assert_eq!(result["Health"], 42);
        assert_eq!(result["Missing"]["$status"], "missing");
        assert_eq!(result["Opaque"]["$code"], -23402);
        assert_eq!(result["Opaque"]["$error"], "cannot serialize");
        assert_eq!(result["Opaque"]["$data"]["field"], "handle");
        assert_eq!(result["Opaque"]["$status"], "unreadable_or_unknown");
    }
}

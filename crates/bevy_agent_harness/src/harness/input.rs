//! Input injection. Actions are queued and delivered as ordinary Bevy input
//! messages (`KeyboardInput`, `MouseButtonInput`, `CursorMoved`, ...) at the
//! start of the next frame, so game code cannot tell them apart from real input.

use bevy::input::keyboard::{Key, KeyboardInput, NativeKey};
use bevy::input::mouse::{MouseButtonInput, MouseMotion, MouseScrollUnit, MouseWheel};
use bevy::input::{ButtonState, InputSystems};
use bevy::prelude::*;
use bevy::remote::BrpResult;
use bevy::window::{CursorMoved, PrimaryWindow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::parse_params;

/// One injectable input action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputAction {
    /// Press or release a key.
    Key {
        key: KeyCode,
        state: ButtonState,
        /// Logical key; derived from `key` when omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        logical: Option<Key>,
    },
    /// Press or release a mouse button.
    MouseButton {
        button: MouseButton,
        state: ButtonState,
    },
    /// Move the cursor to a position in logical window pixels (origin top-left).
    Cursor { position: [f32; 2] },
    /// Relative mouse motion (for FPS-style camera controls).
    Motion { delta: [f32; 2] },
    /// Scroll wheel.
    Scroll {
        #[serde(default)]
        x: f32,
        #[serde(default)]
        y: f32,
        #[serde(default)]
        unit: ScrollUnit,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScrollUnit {
    #[default]
    Line,
    Pixel,
}

/// Queue of actions to deliver. `delay` is in frames; `0` means "next frame".
#[derive(Resource, Default, Debug)]
pub struct InputQueue {
    pending: Vec<(u32, InputAction)>,
}

impl InputQueue {
    /// Deliver `action` after `delay` frames (0 = next frame).
    pub fn push(&mut self, delay: u32, action: InputAction) {
        self.pending.push((delay, action));
    }

    /// Press now (next frame) and release after `hold_frames` frames.
    pub fn tap_key(&mut self, key: KeyCode, hold_frames: u32) {
        self.push(
            0,
            InputAction::Key {
                key,
                state: ButtonState::Pressed,
                logical: None,
            },
        );
        self.push(
            hold_frames.max(1),
            InputAction::Key {
                key,
                state: ButtonState::Released,
                logical: None,
            },
        );
    }

    pub fn tap_mouse(&mut self, button: MouseButton, hold_frames: u32) {
        self.push(
            0,
            InputAction::MouseButton {
                button,
                state: ButtonState::Pressed,
            },
        );
        self.push(
            hold_frames.max(1),
            InputAction::MouseButton {
                button,
                state: ButtonState::Released,
            },
        );
    }

    /// Largest delay currently queued, so callers can chain sequential actions.
    pub fn horizon(&self) -> u32 {
        self.pending.iter().map(|(d, _)| *d + 1).max().unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

pub(crate) fn plugin(app: &mut App) {
    app.init_resource::<InputQueue>()
        .add_systems(PreUpdate, deliver_inputs.before(InputSystems));
}

fn deliver_inputs(
    mut queue: ResMut<InputQueue>,
    mut windows: Query<(Entity, &mut Window, Option<&PrimaryWindow>)>,
    mut keys: MessageWriter<KeyboardInput>,
    mut buttons: MessageWriter<MouseButtonInput>,
    mut cursor: MessageWriter<CursorMoved>,
    mut motion: MessageWriter<MouseMotion>,
    mut wheel: MessageWriter<MouseWheel>,
) {
    if queue.is_empty() {
        return;
    }
    // Prefer the primary window, otherwise any window, otherwise a placeholder.
    let mut target = None;
    for (e, _, primary) in windows.iter() {
        if primary.is_some() {
            target = Some(e);
            break;
        }
        target.get_or_insert(e);
    }
    let window = target.unwrap_or(Entity::PLACEHOLDER);

    let mut due = Vec::new();
    queue.pending.retain_mut(|(delay, action)| {
        if *delay == 0 {
            due.push(action.clone());
            false
        } else {
            *delay -= 1;
            true
        }
    });

    for action in due {
        match action {
            InputAction::Key {
                key,
                state,
                logical,
            } => {
                let logical_key = logical.unwrap_or_else(|| logical_key_for(key));
                let text = match (&logical_key, state) {
                    (Key::Character(c), ButtonState::Pressed) => Some(c.clone()),
                    (Key::Space, ButtonState::Pressed) => Some(" ".into()),
                    _ => None,
                };
                keys.write(KeyboardInput {
                    key_code: key,
                    logical_key,
                    state,
                    text,
                    repeat: false,
                    window,
                });
            }
            InputAction::MouseButton { button, state } => {
                buttons.write(MouseButtonInput {
                    button,
                    state,
                    window,
                });
            }
            InputAction::Cursor { position } => {
                let position = Vec2::from(position);
                let mut delta = None;
                if let Ok((_, mut win, _)) = windows.get_mut(window) {
                    delta = win.cursor_position().map(|prev| position - prev);
                    let scale = win.scale_factor() as f64;
                    win.set_physical_cursor_position(Some(position.as_dvec2() * scale));
                }
                cursor.write(CursorMoved {
                    window,
                    position,
                    delta,
                });
            }
            InputAction::Motion { delta } => {
                motion.write(MouseMotion {
                    delta: Vec2::from(delta),
                });
            }
            InputAction::Scroll { x, y, unit } => {
                wheel.write(MouseWheel {
                    unit: match unit {
                        ScrollUnit::Line => MouseScrollUnit::Line,
                        ScrollUnit::Pixel => MouseScrollUnit::Pixel,
                    },
                    x,
                    y,
                    window,
                    phase: bevy::input::touch::TouchPhase::Moved,
                });
            }
        }
    }
}

/// Best-effort mapping from a physical key to the logical key a US layout produces.
pub fn logical_key_for(key: KeyCode) -> Key {
    use KeyCode::*;
    let ch = |c: &str| Key::Character(c.into());
    match key {
        KeyA => ch("a"),
        KeyB => ch("b"),
        KeyC => ch("c"),
        KeyD => ch("d"),
        KeyE => ch("e"),
        KeyF => ch("f"),
        KeyG => ch("g"),
        KeyH => ch("h"),
        KeyI => ch("i"),
        KeyJ => ch("j"),
        KeyK => ch("k"),
        KeyL => ch("l"),
        KeyM => ch("m"),
        KeyN => ch("n"),
        KeyO => ch("o"),
        KeyP => ch("p"),
        KeyQ => ch("q"),
        KeyR => ch("r"),
        KeyS => ch("s"),
        KeyT => ch("t"),
        KeyU => ch("u"),
        KeyV => ch("v"),
        KeyW => ch("w"),
        KeyX => ch("x"),
        KeyY => ch("y"),
        KeyZ => ch("z"),
        Digit0 => ch("0"),
        Digit1 => ch("1"),
        Digit2 => ch("2"),
        Digit3 => ch("3"),
        Digit4 => ch("4"),
        Digit5 => ch("5"),
        Digit6 => ch("6"),
        Digit7 => ch("7"),
        Digit8 => ch("8"),
        Digit9 => ch("9"),
        Minus => ch("-"),
        Equal => ch("="),
        BracketLeft => ch("["),
        BracketRight => ch("]"),
        Backslash => ch("\\"),
        Semicolon => ch(";"),
        Quote => ch("'"),
        Comma => ch(","),
        Period => ch("."),
        Slash => ch("/"),
        Backquote => ch("`"),
        Space => Key::Space,
        Enter | NumpadEnter => Key::Enter,
        Tab => Key::Tab,
        Backspace => Key::Backspace,
        Delete => Key::Delete,
        Escape => Key::Escape,
        ArrowUp => Key::ArrowUp,
        ArrowDown => Key::ArrowDown,
        ArrowLeft => Key::ArrowLeft,
        ArrowRight => Key::ArrowRight,
        ShiftLeft | ShiftRight => Key::Shift,
        ControlLeft | ControlRight => Key::Control,
        AltLeft | AltRight => Key::Alt,
        SuperLeft | SuperRight => Key::Super,
        Home => Key::Home,
        End => Key::End,
        PageUp => Key::PageUp,
        PageDown => Key::PageDown,
        Insert => Key::Insert,
        CapsLock => Key::CapsLock,
        F1 => Key::F1,
        F2 => Key::F2,
        F3 => Key::F3,
        F4 => Key::F4,
        F5 => Key::F5,
        F6 => Key::F6,
        F7 => Key::F7,
        F8 => Key::F8,
        F9 => Key::F9,
        F10 => Key::F10,
        F11 => Key::F11,
        F12 => Key::F12,
        _ => Key::Unidentified(NativeKey::Unidentified),
    }
}

// ---- BRP handlers ---------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PressKind {
    #[default]
    Tap,
    Press,
    Release,
}

#[derive(Deserialize, Default)]
struct KeyParams {
    /// Either a single key or a list of keys, tapped in sequence.
    #[serde(default)]
    key: Option<KeyCode>,
    #[serde(default)]
    keys: Vec<KeyCode>,
    #[serde(default)]
    state: PressKind,
    /// Frames to hold for `tap`. Default 1.
    #[serde(default)]
    hold_frames: Option<u32>,
    /// Frames to wait before the first action. Default 0.
    #[serde(default)]
    delay: u32,
}

fn queued(queue: &InputQueue) -> Value {
    json!({ "queued": queue.len(), "horizon_frames": queue.horizon() })
}

/// `harness.input.key`
pub fn inject_key(In(params): In<Option<Value>>, mut queue: ResMut<InputQueue>) -> BrpResult {
    let p: KeyParams = parse_params(params)?;
    let mut keys = p.keys;
    if let Some(k) = p.key {
        keys.insert(0, k);
    }
    if keys.is_empty() {
        return Err(super::invalid_params("expected `key` or `keys`"));
    }
    let hold = p.hold_frames.unwrap_or(1).max(1);
    let mut delay = p.delay;
    for key in keys {
        match p.state {
            PressKind::Tap => {
                queue.push(
                    delay,
                    InputAction::Key {
                        key,
                        state: ButtonState::Pressed,
                        logical: None,
                    },
                );
                queue.push(
                    delay + hold,
                    InputAction::Key {
                        key,
                        state: ButtonState::Released,
                        logical: None,
                    },
                );
                // Sequential taps: leave a frame gap so `just_pressed` fires per key.
                delay += hold + 1;
            }
            PressKind::Press => {
                queue.push(
                    delay,
                    InputAction::Key {
                        key,
                        state: ButtonState::Pressed,
                        logical: None,
                    },
                );
            }
            PressKind::Release => {
                queue.push(
                    delay,
                    InputAction::Key {
                        key,
                        state: ButtonState::Released,
                        logical: None,
                    },
                );
            }
        }
    }
    Ok(queued(&queue))
}

#[derive(Deserialize)]
struct MouseParams {
    #[serde(default = "default_button")]
    button: MouseButton,
    #[serde(default)]
    state: PressKind,
    #[serde(default)]
    position: Option<[f32; 2]>,
    #[serde(default)]
    hold_frames: Option<u32>,
    #[serde(default)]
    delay: u32,
}
fn default_button() -> MouseButton {
    MouseButton::Left
}
impl Default for MouseParams {
    fn default() -> Self {
        Self {
            button: MouseButton::Left,
            state: PressKind::Tap,
            position: None,
            hold_frames: None,
            delay: 0,
        }
    }
}

/// `harness.input.mouse`
pub fn inject_mouse(In(params): In<Option<Value>>, mut queue: ResMut<InputQueue>) -> BrpResult {
    let p: MouseParams = parse_params(params)?;
    let hold = p.hold_frames.unwrap_or(1).max(1);
    if let Some(position) = p.position {
        queue.push(p.delay, InputAction::Cursor { position });
    }
    match p.state {
        PressKind::Tap => {
            queue.push(
                p.delay,
                InputAction::MouseButton {
                    button: p.button,
                    state: ButtonState::Pressed,
                },
            );
            queue.push(
                p.delay + hold,
                InputAction::MouseButton {
                    button: p.button,
                    state: ButtonState::Released,
                },
            );
        }
        PressKind::Press => {
            queue.push(
                p.delay,
                InputAction::MouseButton {
                    button: p.button,
                    state: ButtonState::Pressed,
                },
            );
        }
        PressKind::Release => {
            queue.push(
                p.delay,
                InputAction::MouseButton {
                    button: p.button,
                    state: ButtonState::Released,
                },
            );
        }
    }
    Ok(queued(&queue))
}

#[derive(Deserialize, Default)]
struct CursorParams {
    position: Option<[f32; 2]>,
    #[serde(default)]
    delta: Option<[f32; 2]>,
    #[serde(default)]
    delay: u32,
}

/// `harness.input.cursor`
pub fn inject_cursor(In(params): In<Option<Value>>, mut queue: ResMut<InputQueue>) -> BrpResult {
    let p: CursorParams = parse_params(params)?;
    if p.position.is_none() && p.delta.is_none() {
        return Err(super::invalid_params("expected `position` and/or `delta`"));
    }
    if let Some(position) = p.position {
        queue.push(p.delay, InputAction::Cursor { position });
    }
    if let Some(delta) = p.delta {
        queue.push(p.delay, InputAction::Motion { delta });
    }
    Ok(queued(&queue))
}

#[derive(Deserialize, Default)]
struct ScrollParams {
    #[serde(default)]
    x: f32,
    #[serde(default)]
    y: f32,
    #[serde(default)]
    unit: ScrollUnit,
    #[serde(default)]
    delay: u32,
}

/// `harness.input.scroll`
pub fn inject_scroll(In(params): In<Option<Value>>, mut queue: ResMut<InputQueue>) -> BrpResult {
    let p: ScrollParams = parse_params(params)?;
    queue.push(
        p.delay,
        InputAction::Scroll {
            x: p.x,
            y: p.y,
            unit: p.unit,
        },
    );
    Ok(queued(&queue))
}

#[derive(Deserialize, Default)]
struct BatchParams {
    /// `[{ "delay": 0, "action": {...} }, ...]` or bare actions delivered in successive frames.
    actions: Vec<TimedAction>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TimedAction {
    Timed { delay: u32, action: InputAction },
    Bare(InputAction),
}

/// `harness.input` – queue several raw actions at once.
pub fn inject_batch(In(params): In<Option<Value>>, mut queue: ResMut<InputQueue>) -> BrpResult {
    let p: BatchParams = parse_params(params)?;
    let mut next = 0;
    for item in p.actions {
        match item {
            TimedAction::Timed { delay, action } => queue.push(delay, action),
            TimedAction::Bare(action) => {
                queue.push(next, action);
                next += 1;
            }
        }
    }
    Ok(queued(&queue))
}

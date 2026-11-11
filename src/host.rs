// The plugin's exports to zellij — `load`, `update`, `pipe`, `render`, `plugin_version`,
// `zellij_plugin_contract` — written out instead of `register_plugin!(State)`.
//
// This is the `register_plugin!` macro of `zellij-tile` (git rev 644d78c), line for line, with ONE
// change: `update` drops the keybinds whose key is not ASCII from a `ModeUpdate` before the
// library decodes it (`ascii_keybinds_only`).
//
// Why: a keybind's key reaches the plugin as protobuf `MainKey::Char(i32)`, the whole code point,
// and the library decodes it with `char_index as u8 as char` (`zellij-utils/src/plugin_api/key.rs`)
// — inside this plugin. A layout twin such as `bind "v" "э"` (the Bulgarian key under `v`) would
// arrive as `M` (U+044D & 0xFF), and the hints bar would print `M Split down`. The twins exist so
// the binds work on a Cyrillic layout; the hints name the Latin key, so they are dropped here
// rather than repaired.
//
// **When `zellij-tile` is upgraded, compare this file with the new macro** (`src/lib.rs` of the
// crate) and carry over whatever changed.

use crate::State;
use std::cell::RefCell;
use std::collections::BTreeMap;
use zellij_tile::prelude::*;
use zellij_tile::shim::plugin_api::action::ProtobufPluginConfiguration;
use zellij_tile::shim::plugin_api::event::{ProtobufEvent, ProtobufEventPayload};
use zellij_tile::shim::plugin_api::key::ProtobufMainKey;
use zellij_tile::shim::prost::Message;

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// The macro's `main`, called from the crate's own (a binary's `main` must live in `main.rs`).
pub fn main() {
    std::panic::set_hook(Box::new(|info| {
        report_panic(info);
    }));
}

#[no_mangle]
fn load() {
    STATE.with(|state| {
        let protobuf_bytes: Vec<u8> = zellij_tile::shim::object_from_stdin().unwrap();
        let protobuf_configuration =
            ProtobufPluginConfiguration::decode(protobuf_bytes.as_slice()).unwrap();
        let configuration: BTreeMap<String, String> =
            BTreeMap::try_from(&protobuf_configuration).unwrap();
        state.borrow_mut().load(configuration);
    });
}

#[no_mangle]
pub fn update() -> bool {
    STATE.with(|state| {
        let protobuf_bytes: Vec<u8> = zellij_tile::shim::object_from_stdin().unwrap();
        // The one line that differs from the macro.
        let protobuf_bytes = ascii_keybinds_only(protobuf_bytes);
        let Some((event, mouse_modifiers)) =
            zellij_tile::shim::plugin_api::event::plugin_event_from_protobuf_bytes(&protobuf_bytes)
        else {
            return false;
        };
        zellij_tile::shim::set_mouse_modifiers(mouse_modifiers);
        state.borrow_mut().update(event)
    })
}

#[no_mangle]
pub fn pipe() -> bool {
    STATE.with(|state| {
        let protobuf_bytes: Vec<u8> = zellij_tile::shim::object_from_stdin().unwrap();
        let Some(pipe_message) =
            zellij_tile::shim::plugin_api::pipe_message::pipe_message_from_protobuf_bytes(
                &protobuf_bytes,
            )
        else {
            return false;
        };
        state.borrow_mut().pipe(pipe_message)
    })
}

#[no_mangle]
pub fn render(rows: i32, cols: i32) {
    STATE.with(|state| {
        state.borrow_mut().render(rows as usize, cols as usize);
    });
}

#[no_mangle]
pub fn plugin_version() {
    println!("{}", zellij_tile::prelude::VERSION);
}

#[no_mangle]
pub fn zellij_plugin_contract() {}

/// A `ModeUpdate` with every keybind whose key is a non-ASCII character taken out; any other event
/// (or bytes that do not decode) unchanged.
fn ascii_keybinds_only(bytes: Vec<u8>) -> Vec<u8> {
    let Ok(mut event) = ProtobufEvent::decode(bytes.as_slice()) else { return bytes };
    let Some(ProtobufEventPayload::ModeUpdatePayload(mode)) = event.payload.as_mut() else {
        return bytes;
    };
    let mut dropped = false;
    for per_mode in &mut mode.keybinds {
        per_mode.key_bind.retain(|bind| {
            let ascii = match bind.key.as_ref().and_then(|key| key.main_key.as_ref()) {
                Some(ProtobufMainKey::Char(code)) => (0..128).contains(code),
                _ => true,
            };
            dropped |= !ascii;
            ascii
        });
    }
    if dropped { event.encode_to_vec() } else { bytes }
}

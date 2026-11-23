//! Terminal input utilities
//!
//! Prime Agent patch: upstream maps key codes through the `terminfo` crate
//! here, for an interactive shell's key bindings. That crate's licence
//! (WTFPL) is not one this workspace ships, and an embedded shell reads no
//! keys: the mapping is the one the other platforms use.

use crate::interfaces;

/// Translates a key code (byte sequence) into a `Key` enum value. Returns `None`
/// if the key code is not recognized.
///
/// Recognizes single-byte non-control characters; terminal-specific key
/// sequences are not supported.
pub fn try_get_key_from_key_code(key_code: &[u8]) -> Option<interfaces::Key> {
    if key_code.len() == 1 && !key_code[0].is_ascii_control() {
        Some(interfaces::Key::Character(key_code[0] as char))
    } else {
        None
    }
}

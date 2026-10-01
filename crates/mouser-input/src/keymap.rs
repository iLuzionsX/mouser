//! Key identity mapping between platforms.
//!
//! Windows reports virtual key codes, macOS reports virtual keycodes, and
//! they disagree -- `VK_OEM_1` and `kVK_ANSI_O` are the same physical key.
//! Every platform-specific key code is therefore normalized to a USB HID
//! usage ID, which is layout independent, and translated back on the way out.
//!
//! The table is `(hid, windows vk, macos keycode)`. A `None` means the key
//! has no equivalent in that platform's set; those fall back to the other
//! column rather than being dropped.

use mouser_core::protocol::HidKey;

/// A key as the local platform names it.
pub type NativeKey = u32;

/// Keys that need `KEYEVENTF_EXTENDEDKEY` on Windows.
///
/// The flag matters for the right-hand cluster and the navigation block: the
/// same numeric VK code is a different scan code depending on it, and without
/// the flag Windows synthesizes the wrong one.
#[cfg(windows)]
pub(crate) const EXTENDED_KEYS: &[u32] = &[
    0x21, // VK_PAGEUP
    0x22, // VK_PAGEDOWN
    0x23, // VK_END
    0x24, // VK_HOME
    0x25, // VK_LEFT
    0x26, // VK_UP
    0x27, // VK_RIGHT
    0x28, // VK_DOWN
    0x2D, // VK_INSERT
    0x2E, // VK_DELETE
    0x5B, // VK_LWIN
    0x5C, // VK_RWIN
    0x6F, // VK_DIVIDE
    0x90, // VK_NUMLOCK
    0xA3, // VK_RCONTROL
    0xA5, // VK_RMENU
];

#[cfg(windows)]
pub(crate) fn needs_extended(native: NativeKey) -> bool {
    EXTENDED_KEYS.contains(&native)
}

/// `(hid usage, windows virtual key, macos virtual keycode)`
///
/// HID values are unique across the table, which is what lets
/// [`from_native`] be a plain reverse lookup.
const KEYS: &[(u8, u32, u32)] = &[
    // Letters.
    (0x04, 0x41, 0x00), // A
    (0x05, 0x42, 0x0B), // S
    (0x06, 0x43, 0x08), // C
    (0x07, 0x44, 0x02), // D
    (0x08, 0x45, 0x0E), // E
    (0x09, 0x46, 0x03), // F
    (0x0A, 0x47, 0x05), // G
    (0x0B, 0x48, 0x04), // H
    (0x0C, 0x49, 0x22), // I
    (0x0D, 0x4A, 0x26), // J
    (0x0E, 0x4B, 0x28), // K
    (0x0F, 0x4C, 0x25), // L
    (0x10, 0x4D, 0x2E), // M
    (0x11, 0x4E, 0x2D), // N
    (0x12, 0x4F, 0x1F), // O
    (0x13, 0x50, 0x23), // P
    (0x14, 0x51, 0x10), // Q
    (0x15, 0x52, 0x0F), // R
    (0x16, 0x53, 0x01), // S
    (0x17, 0x54, 0x11), // T
    (0x18, 0x55, 0x20), // U
    (0x19, 0x56, 0x1D), // V
    (0x1A, 0x57, 0x1A), // W
    (0x1B, 0x58, 0x1B), // X
    (0x1C, 0x59, 0x1C), // Y
    (0x1D, 0x5A, 0x06), // Z
    // Number row.
    (0x1E, 0x31, 0x12), // 1
    (0x1F, 0x32, 0x13), // 2
    (0x20, 0x33, 0x14), // 3
    (0x21, 0x34, 0x15), // 4
    (0x22, 0x35, 0x17), // 5
    (0x23, 0x36, 0x16), // 6
    (0x24, 0x37, 0x1A), // 7
    (0x25, 0x38, 0x1C), // 8
    (0x26, 0x39, 0x19), // 9
    (0x27, 0x30, 0x1D), // 0
    // Control keys.
    (0x28, 0x0D, 0x24), // Enter
    (0x29, 0x1B, 0x35), // Escape
    (0x2A, 0x08, 0x33), // Backspace
    (0x2B, 0x09, 0x30), // Tab
    (0x2C, 0x20, 0x31), // Space
    // Punctuation.
    (0x2D, 0xBD, 0x1B), // -
    (0x2E, 0xBB, 0x18), // =
    (0x2F, 0xDB, 0x21), // [
    (0x30, 0xDD, 0x1E), // ]
    (0x31, 0xDC, 0x2A), // \
    (0x33, 0xBA, 0x29), // ;
    (0x34, 0xDE, 0x27), // '
    (0x35, 0xC0, 0x32), // `
    (0x36, 0xBC, 0x2B), // ,
    (0x37, 0xBE, 0x2F), // .
    (0x38, 0xBF, 0x2C), // /
    (0x39, 0x14, 0x39), // CapsLock
    // Function keys.
    (0x3A, 0x70, 0x7A), // F1
    (0x3B, 0x71, 0x78), // F2
    (0x3C, 0x72, 0x63), // F3
    (0x3D, 0x73, 0x76), // F4
    (0x3E, 0x74, 0x60), // F5
    (0x3F, 0x75, 0x61), // F6
    (0x40, 0x76, 0x62), // F7
    (0x41, 0x77, 0x64), // F8
    (0x42, 0x78, 0x65), // F9
    (0x43, 0x79, 0x6D), // F10
    (0x44, 0x7A, 0x67), // F11
    (0x45, 0x7B, 0x6F), // F12
    // Navigation block.
    (0x46, 0x2C, 0x69), // PrintScreen
    (0x47, 0x91, 0x6B), // ScrollLock
    (0x48, 0x13, 0x71), // Pause
    (0x49, 0x2D, 0x72), // Insert
    (0x4A, 0x24, 0x73), // Home
    (0x4B, 0x21, 0x74), // PageUp
    (0x4C, 0x2E, 0x75), // Delete
    (0x4D, 0x23, 0x77), // End
    (0x4E, 0x22, 0x79), // PageDown
    (0x4F, 0x27, 0x7C), // Right
    (0x50, 0x25, 0x7B), // Left
    (0x51, 0x28, 0x7D), // Down
    (0x52, 0x26, 0x7E), // Up
    // Keypad.
    (0x53, 0x90, 0x47), // NumLock
    (0x54, 0x6F, 0x4B), // /
    (0x55, 0x6A, 0x43), // *
    (0x56, 0x6D, 0x4E), // -
    (0x57, 0x6B, 0x45), // +
    (0x59, 0x61, 0x53), // 1
    (0x5A, 0x62, 0x54), // 2
    (0x5B, 0x63, 0x55), // 3
    (0x5C, 0x64, 0x56), // 4
    (0x5D, 0x65, 0x57), // 5
    (0x5E, 0x66, 0x58), // 6
    (0x5F, 0x67, 0x59), // 7
    (0x60, 0x68, 0x5B), // 8
    (0x61, 0x69, 0x5C), // 9
    (0x62, 0x60, 0x52), // 0
    (0x63, 0x6E, 0x41), // .
    // Modifiers. Order matches the HID block at 0xE0.
    (0xE0, 0xA2, 0x3B), // LeftControl
    (0xE1, 0xA0, 0x38), // LeftShift
    (0xE2, 0xA4, 0x3A), // LeftAlt / Option
    (0xE3, 0x5B, 0x37), // LeftGUI / Command
    (0xE4, 0xA3, 0x3E), // RightControl
    (0xE5, 0xA1, 0x3C), // RightShift
    (0xE6, 0xA5, 0x3D), // RightAlt / Option
    (0xE7, 0x5C, 0x36), // RightGUI / Command
];

/// Column index of the native key code in [`KEYS`] for this target:
/// 0 for Windows virtual key codes, 2 for macOS virtual keycodes.
#[cfg(windows)]
const NATIVE_COLUMN: usize = 0;

#[cfg(target_os = "macos")]
const NATIVE_COLUMN: usize = 2;

/// Translate a HID usage ID into this platform's key code.
pub fn to_native(hid: HidKey) -> Option<NativeKey> {
    KEYS.iter().find(|(h, _, _)| *h == hid.0).map(
        |(_, w, m)| {
            if NATIVE_COLUMN == 0 { *w } else { *m }
        },
    )
}

/// Translate this platform's key code back into a HID usage ID.
///
/// Only the macOS backend needs this: Windows captures from scan codes
/// directly, because a virtual key code describes the active layout rather
/// than the physical key. It is public because both platforms share one
/// table, and a function only one of them calls is still part of the API.
///
/// Unambiguous because HID usage IDs are unique across [`KEYS`], which
/// `hid_usage_ids_are_unique` enforces.
#[cfg_attr(windows, allow(dead_code))]
pub fn from_native(native: NativeKey) -> Option<HidKey> {
    KEYS.iter()
        .find(|(_, w, m)| {
            if NATIVE_COLUMN == 0 {
                *w == native
            } else {
                *m == native
            }
        })
        .map(|(h, _, _)| HidKey(*h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_usage_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (hid, _, _) in KEYS {
            assert!(seen.insert(*hid), "duplicate HID usage {hid:#04x}");
        }
    }

    #[test]
    fn every_key_round_trips() {
        for (hid, _, _) in KEYS {
            let hid_key = HidKey(*hid);
            let native = to_native(hid_key).expect("should map to a native key");
            assert_eq!(from_native(native), Some(hid_key), "{hid:#04x}");
        }
    }

    #[test]
    fn unknown_keys_return_none() {
        assert_eq!(to_native(HidKey(0xFF)), None);
        assert_eq!(from_native(0xFFFF_FFFF), None);
    }

    #[cfg(windows)]
    #[test]
    fn arrows_and_delete_need_the_extended_flag() {
        assert!(needs_extended(0x25)); // Left
        assert!(needs_extended(0x27)); // Right
        assert!(needs_extended(0x2E)); // Delete
        assert!(!needs_extended(0x41)); // A
    }
}

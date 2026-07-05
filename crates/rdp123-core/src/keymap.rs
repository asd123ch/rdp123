//! Translation from macOS virtual key codes (Carbon `kVK_*`) to PC/XT
//! scan codes (RDP "set 1"). We deliberately map by *physical position*, not
//! by character: the Windows host applies its own keyboard layout, so the
//! client only reports which physical key changed state. This is what keeps
//! non-US layouts (e.g. Swiss German) working without any per-layout logic.

/// A PC scan code plus whether it needs the extended (`0xE0`) prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCode {
    pub code: u16,
    pub extended: bool,
}

const fn sc(code: u16) -> ScanCode {
    ScanCode {
        code,
        extended: false,
    }
}
const fn ext(code: u16) -> ScanCode {
    ScanCode {
        code,
        extended: true,
    }
}

/// Map a macOS virtual key code to a PC scan code.
///
/// Returns `None` for keys we intentionally handle locally or cannot map
/// (e.g. the Fn key, which has no RDP equivalent).
///
/// With `swap_cmd_alt`, ⌘ sends Alt and ⌥ sends the Windows key, so the key
/// next to the space bar behaves like a PC's Alt.
pub fn mac_keycode_to_scancode(keycode: u16, swap_cmd_alt: bool) -> Option<ScanCode> {
    if swap_cmd_alt {
        match keycode {
            0x3A => return Some(ext(0x5B)), // Left Option -> Left Windows
            0x3D => return Some(ext(0x5C)), // Right Option -> Right Windows
            0x37 => return Some(sc(0x38)),  // Left Command -> Left Alt
            0x36 => return Some(ext(0x38)), // Right Command -> Right Alt
            _ => {}
        }
    }
    let s = match keycode {
        // ── Letters (physical positions on ANSI/ISO) ──
        0x00 => sc(0x1E), // A
        0x0B => sc(0x30), // B
        0x08 => sc(0x2E), // C
        0x02 => sc(0x20), // D
        0x0E => sc(0x12), // E
        0x03 => sc(0x21), // F
        0x05 => sc(0x22), // G
        0x04 => sc(0x23), // H
        0x22 => sc(0x17), // I
        0x26 => sc(0x24), // J
        0x28 => sc(0x25), // K
        0x25 => sc(0x26), // L
        0x2E => sc(0x32), // M
        0x2D => sc(0x31), // N
        0x1F => sc(0x18), // O
        0x23 => sc(0x19), // P
        0x0C => sc(0x10), // Q
        0x0F => sc(0x13), // R
        0x01 => sc(0x1F), // S
        0x11 => sc(0x14), // T
        0x20 => sc(0x16), // U
        0x09 => sc(0x2F), // V
        0x0D => sc(0x11), // W
        0x07 => sc(0x2D), // X
        0x10 => sc(0x15), // Y
        0x06 => sc(0x2C), // Z

        // ── Number row ──
        0x12 => sc(0x02), // 1
        0x13 => sc(0x03), // 2
        0x14 => sc(0x04), // 3
        0x15 => sc(0x05), // 4
        0x17 => sc(0x06), // 5
        0x16 => sc(0x07), // 6
        0x1A => sc(0x08), // 7
        0x1C => sc(0x09), // 8
        0x19 => sc(0x0A), // 9
        0x1D => sc(0x0B), // 0

        // ── Punctuation (physical position) ──
        0x1B => sc(0x0C), // -
        0x18 => sc(0x0D), // =
        0x21 => sc(0x1A), // [
        0x1E => sc(0x1B), // ]
        0x2A => sc(0x2B), // backslash
        0x29 => sc(0x27), // ;
        0x27 => sc(0x28), // '
        0x32 => sc(0x29), // ` (grave / below Esc)
        0x2B => sc(0x33), // ,
        0x2F => sc(0x34), // .
        0x2C => sc(0x35), // /
        0x0A => sc(0x56), // ISO section key (extra key left of Z on ISO layouts)

        // ── Whitespace / editing ──
        0x24 => sc(0x1C), // Return
        0x30 => sc(0x0F), // Tab
        0x31 => sc(0x39), // Space
        0x33 => sc(0x0E), // Backspace (Delete)
        0x35 => sc(0x01), // Escape

        // ── Modifiers ──
        0x38 => sc(0x2A),  // Left Shift
        0x3C => sc(0x36),  // Right Shift
        0x3B => sc(0x1D),  // Left Control
        0x3E => ext(0x1D), // Right Control
        0x3A => sc(0x38),  // Left Option -> Left Alt
        0x3D => ext(0x38), // Right Option -> Right Alt (AltGr)
        0x37 => ext(0x5B), // Left Command -> Left Windows
        0x36 => ext(0x5C), // Right Command -> Right Windows
        0x39 => sc(0x3A),  // Caps Lock

        // ── Function keys ──
        0x7A => sc(0x3B), // F1
        0x78 => sc(0x3C), // F2
        0x63 => sc(0x3D), // F3
        0x76 => sc(0x3E), // F4
        0x60 => sc(0x3F), // F5
        0x61 => sc(0x40), // F6
        0x62 => sc(0x41), // F7
        0x64 => sc(0x42), // F8
        0x65 => sc(0x43), // F9
        0x6D => sc(0x44), // F10
        0x67 => sc(0x57), // F11
        0x6F => sc(0x58), // F12

        // ── Navigation cluster (all extended) ──
        0x73 => ext(0x47), // Home
        0x77 => ext(0x4F), // End
        0x74 => ext(0x49), // Page Up
        0x79 => ext(0x51), // Page Down
        0x75 => ext(0x53), // Forward Delete
        0x72 => ext(0x52), // Insert / Help
        0x7E => ext(0x48), // Up
        0x7D => ext(0x50), // Down
        0x7B => ext(0x4B), // Left
        0x7C => ext(0x4D), // Right

        // ── Keypad ──
        0x52 => sc(0x52),  // KP 0
        0x53 => sc(0x4F),  // KP 1
        0x54 => sc(0x50),  // KP 2
        0x55 => sc(0x51),  // KP 3
        0x56 => sc(0x4B),  // KP 4
        0x57 => sc(0x4C),  // KP 5
        0x58 => sc(0x4D),  // KP 6
        0x59 => sc(0x47),  // KP 7
        0x5B => sc(0x48),  // KP 8
        0x5C => sc(0x49),  // KP 9
        0x41 => sc(0x53),  // KP .
        0x43 => sc(0x37),  // KP *
        0x45 => sc(0x4E),  // KP +
        0x4E => sc(0x4A),  // KP -
        0x4B => ext(0x35), // KP /
        0x4C => ext(0x1C), // KP Enter
        0x47 => sc(0x45),  // Clear -> NumLock

        _ => return None,
    };
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_map_to_expected_positions() {
        assert_eq!(mac_keycode_to_scancode(0x00, false), Some(sc(0x1E))); // A
        assert_eq!(mac_keycode_to_scancode(0x06, false), Some(sc(0x2C))); // Z
    }

    #[test]
    fn navigation_keys_are_extended() {
        for kc in [0x73, 0x77, 0x74, 0x79, 0x75, 0x7E, 0x7D, 0x7B, 0x7C] {
            assert!(mac_keycode_to_scancode(kc, false).unwrap().extended);
        }
    }

    #[test]
    fn command_maps_to_windows_key_extended() {
        assert_eq!(mac_keycode_to_scancode(0x37, false), Some(ext(0x5B)));
        assert_eq!(mac_keycode_to_scancode(0x36, false), Some(ext(0x5C)));
    }

    #[test]
    fn right_modifiers_differ_from_left() {
        assert_eq!(mac_keycode_to_scancode(0x3B, false), Some(sc(0x1D))); // L Ctrl
        assert_eq!(mac_keycode_to_scancode(0x3E, false), Some(ext(0x1D))); // R Ctrl
        assert_eq!(mac_keycode_to_scancode(0x3A, false), Some(sc(0x38))); // L Alt
        assert_eq!(mac_keycode_to_scancode(0x3D, false), Some(ext(0x38))); // R Alt
    }

    #[test]
    fn swap_exchanges_command_and_option_only() {
        // Swapped: ⌘ becomes Alt, ⌥ becomes the Windows key.
        assert_eq!(mac_keycode_to_scancode(0x37, true), Some(sc(0x38))); // L Cmd -> L Alt
        assert_eq!(mac_keycode_to_scancode(0x36, true), Some(ext(0x38))); // R Cmd -> R Alt
        assert_eq!(mac_keycode_to_scancode(0x3A, true), Some(ext(0x5B))); // L Opt -> L Win
        assert_eq!(mac_keycode_to_scancode(0x3D, true), Some(ext(0x5C))); // R Opt -> R Win
                                                                          // Everything else is untouched.
        assert_eq!(mac_keycode_to_scancode(0x00, true), Some(sc(0x1E))); // A
        assert_eq!(mac_keycode_to_scancode(0x3B, true), Some(sc(0x1D))); // L Ctrl
    }

    #[test]
    fn unmapped_returns_none() {
        assert_eq!(mac_keycode_to_scancode(0x3F, false), None); // Fn
        assert_eq!(mac_keycode_to_scancode(0xFFFF, false), None);
    }

    #[test]
    fn iso_section_key_maps_to_0x56() {
        assert_eq!(mac_keycode_to_scancode(0x0A, false), Some(sc(0x56)));
    }
}

//! Physical key -> PC/AT set-1 scancode (+ E0 "extended" flag), which the Windows agent injects with
//! `SendInput(KEYEVENTF_SCANCODE)`. Layout independent: the remote keyboard layout / IME decides
//! the character, exactly like a local keyboard.
use iced::keyboard::key::{Code, NativeCode, Physical};

/// Returns (scancode, extended).
pub fn scancode(p: &Physical) -> Option<(u16, bool)> {
    match p {
        Physical::Code(c) => code(*c),
        // Windows reports the raw scancode for keys winit cannot name (E0-prefixed codes as 0xE0xx).
        Physical::Unidentified(NativeCode::Windows(sc)) => Some(((sc & 0xFF), sc & 0xE000 == 0xE000)),
        Physical::Unidentified(_) => None,
    }
}

fn code(c: Code) -> Option<(u16, bool)> {
    use Code::*;
    let n = |s: u16| Some((s, false));
    let e = |s: u16| Some((s, true));
    match c {
        Escape => n(0x01),
        Digit1 => n(0x02),
        Digit2 => n(0x03),
        Digit3 => n(0x04),
        Digit4 => n(0x05),
        Digit5 => n(0x06),
        Digit6 => n(0x07),
        Digit7 => n(0x08),
        Digit8 => n(0x09),
        Digit9 => n(0x0A),
        Digit0 => n(0x0B),
        Minus => n(0x0C),
        Equal => n(0x0D),
        Backspace => n(0x0E),
        Tab => n(0x0F),
        KeyQ => n(0x10),
        KeyW => n(0x11),
        KeyE => n(0x12),
        KeyR => n(0x13),
        KeyT => n(0x14),
        KeyY => n(0x15),
        KeyU => n(0x16),
        KeyI => n(0x17),
        KeyO => n(0x18),
        KeyP => n(0x19),
        BracketLeft => n(0x1A),
        BracketRight => n(0x1B),
        Enter => n(0x1C),
        ControlLeft => n(0x1D),
        KeyA => n(0x1E),
        KeyS => n(0x1F),
        KeyD => n(0x20),
        KeyF => n(0x21),
        KeyG => n(0x22),
        KeyH => n(0x23),
        KeyJ => n(0x24),
        KeyK => n(0x25),
        KeyL => n(0x26),
        Semicolon => n(0x27),
        Quote => n(0x28),
        Backquote => n(0x29),
        ShiftLeft => n(0x2A),
        Backslash => n(0x2B),
        KeyZ => n(0x2C),
        KeyX => n(0x2D),
        KeyC => n(0x2E),
        KeyV => n(0x2F),
        KeyB => n(0x30),
        KeyN => n(0x31),
        KeyM => n(0x32),
        Comma => n(0x33),
        Period => n(0x34),
        Slash => n(0x35),
        ShiftRight => n(0x36),
        NumpadMultiply | NumpadStar => n(0x37),
        AltLeft => n(0x38),
        Space => n(0x39),
        CapsLock => n(0x3A),
        F1 => n(0x3B),
        F2 => n(0x3C),
        F3 => n(0x3D),
        F4 => n(0x3E),
        F5 => n(0x3F),
        F6 => n(0x40),
        F7 => n(0x41),
        F8 => n(0x42),
        F9 => n(0x43),
        F10 => n(0x44),
        NumLock => n(0x45), // hardware: 45 without prefix (MapVirtualKey's 0xE045 is a Windows quirk)
        ScrollLock => n(0x46),
        Numpad7 => n(0x47),
        Numpad8 => n(0x48),
        Numpad9 => n(0x49),
        NumpadSubtract => n(0x4A),
        Numpad4 => n(0x4B),
        Numpad5 => n(0x4C),
        Numpad6 => n(0x4D),
        NumpadAdd => n(0x4E),
        Numpad1 => n(0x4F),
        Numpad2 => n(0x50),
        Numpad3 => n(0x51),
        Numpad0 => n(0x52),
        NumpadDecimal => n(0x53),
        IntlBackslash => n(0x56),
        F11 => n(0x57),
        F12 => n(0x58),
        NumpadEqual => n(0x59),
        F13 => n(0x64),
        F14 => n(0x65),
        F15 => n(0x66),
        F16 => n(0x67),
        F17 => n(0x68),
        F18 => n(0x69),
        F19 => n(0x6A),
        F20 => n(0x6B),
        F21 => n(0x6C),
        F22 => n(0x6D),
        F23 => n(0x6E),
        F24 => n(0x76),
        KanaMode => n(0x70),
        IntlRo => n(0x73),
        Convert => n(0x79),
        NonConvert => n(0x7B),
        IntlYen => n(0x7D),
        NumpadComma => n(0x7E),
        // Korean keyboards: 한/영 (Lang1) and 한자 (Lang2) - USB HID usage -> set-1 0xF2 / 0xF1.
        Lang1 => n(0xF2),
        Lang2 => n(0xF1),
        NumpadEnter => e(0x1C),
        ControlRight => e(0x1D),
        NumpadDivide => e(0x35),
        PrintScreen => e(0x37),
        AltRight => e(0x38),
        Home => e(0x47),
        ArrowUp => e(0x48),
        PageUp => e(0x49),
        ArrowLeft => e(0x4B),
        ArrowRight => e(0x4D),
        End => e(0x4F),
        ArrowDown => e(0x50),
        PageDown => e(0x51),
        Insert => e(0x52),
        Delete => e(0x53),
        SuperLeft | Meta => e(0x5B),
        SuperRight => e(0x5C),
        ContextMenu => e(0x5D),
        Pause => n(protocol::SCANCODE_PAUSE), // E1 1D 45
        AudioVolumeMute => e(0x20),
        AudioVolumeDown => e(0x2E),
        AudioVolumeUp => e(0x30),
        MediaPlayPause => e(0x22),
        MediaStop => e(0x24),
        MediaTrackPrevious => e(0x10),
        MediaTrackNext => e(0x19),
        BrowserBack => e(0x6A),
        BrowserForward => e(0x69),
        BrowserRefresh => e(0x67),
        BrowserHome => e(0x32),
        _ => None,
    }
}

/// macOS virtual key code (`kVK_*`, as in CGEvent / NSEvent `keyCode`) -> scancode, through the
/// same physical-key table (same codes winit assigns). Fn and unknown keys -> None.
#[cfg(any(target_os = "macos", test))]
pub fn mac_keycode(kc: u16) -> Option<(u16, bool)> {
    use Code::*;
    code(match kc {
        0x00 => KeyA,
        0x01 => KeyS,
        0x02 => KeyD,
        0x03 => KeyF,
        0x04 => KeyH,
        0x05 => KeyG,
        0x06 => KeyZ,
        0x07 => KeyX,
        0x08 => KeyC,
        0x09 => KeyV,
        0x0A => IntlBackslash, // ISO section key
        0x0B => KeyB,
        0x0C => KeyQ,
        0x0D => KeyW,
        0x0E => KeyE,
        0x0F => KeyR,
        0x10 => KeyY,
        0x11 => KeyT,
        0x12 => Digit1,
        0x13 => Digit2,
        0x14 => Digit3,
        0x15 => Digit4,
        0x16 => Digit6,
        0x17 => Digit5,
        0x18 => Equal,
        0x19 => Digit9,
        0x1A => Digit7,
        0x1B => Minus,
        0x1C => Digit8,
        0x1D => Digit0,
        0x1E => BracketRight,
        0x1F => KeyO,
        0x20 => KeyU,
        0x21 => BracketLeft,
        0x22 => KeyI,
        0x23 => KeyP,
        0x24 => Enter,
        0x25 => KeyL,
        0x26 => KeyJ,
        0x27 => Quote,
        0x28 => KeyK,
        0x29 => Semicolon,
        0x2A => Backslash,
        0x2B => Comma,
        0x2C => Slash,
        0x2D => KeyN,
        0x2E => KeyM,
        0x2F => Period,
        0x30 => Tab,
        0x31 => Space,
        0x32 => Backquote,
        0x33 => Backspace,
        0x35 => Escape,
        // Command = Windows key, Option = Alt (physical position, like a PC keyboard)
        0x36 => SuperRight,
        0x37 => SuperLeft,
        0x38 => ShiftLeft,
        0x39 => CapsLock,
        0x3A => AltLeft,
        0x3B => ControlLeft,
        0x3C => ShiftRight,
        0x3D => AltRight,
        0x3E => ControlRight,
        0x40 => F17,
        0x41 => NumpadDecimal,
        0x43 => NumpadMultiply,
        0x45 => NumpadAdd,
        0x47 => NumLock, // keypad Clear
        0x48 => AudioVolumeUp,
        0x49 => AudioVolumeDown,
        0x4A => AudioVolumeMute,
        0x4B => NumpadDivide,
        0x4C => NumpadEnter,
        0x4E => NumpadSubtract,
        0x4F => F18,
        0x50 => F19,
        0x51 => NumpadEqual,
        0x52 => Numpad0,
        0x53 => Numpad1,
        0x54 => Numpad2,
        0x55 => Numpad3,
        0x56 => Numpad4,
        0x57 => Numpad5,
        0x58 => Numpad6,
        0x59 => Numpad7,
        0x5A => F20,
        0x5B => Numpad8,
        0x5C => Numpad9,
        0x5D => IntlYen,
        0x5E => IntlRo,
        0x5F => NumpadComma,
        0x60 => F5,
        0x61 => F6,
        0x62 => F7,
        0x63 => F3,
        0x64 => F8,
        0x65 => F9,
        0x66 => Lang2, // JIS Eisu
        0x67 => F11,
        0x68 => Lang1, // JIS Kana
        0x69 => F13,
        0x6A => F16,
        0x6B => F14,
        0x6D => F10,
        0x6E => ContextMenu,
        0x6F => F12,
        0x71 => F15,
        0x72 => Insert, // Help
        0x73 => Home,
        0x74 => PageUp,
        0x75 => Delete,
        0x76 => F4,
        0x77 => End,
        0x78 => F2,
        0x79 => PageDown,
        0x7A => F1,
        0x7B => ArrowLeft,
        0x7C => ArrowRight,
        0x7D => ArrowDown,
        0x7E => ArrowUp,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_keys() {
        assert_eq!(mac_keycode(0x00), Some((0x1E, false)), "A");
        assert_eq!(mac_keycode(0x37), Some((0x5B, true)), "left Command = left Windows key");
        assert_eq!(mac_keycode(0x3A), Some((0x38, false)), "left Option = Alt");
        assert_eq!(mac_keycode(0x3D), Some((0x38, true)), "right Option = right Alt");
        assert_eq!(mac_keycode(0x30), Some((0x0F, false)), "Tab");
        assert_eq!(mac_keycode(0x77), Some((0x4F, true)), "End (also Fn+Right on laptops)");
        assert_eq!(mac_keycode(0x3F), None, "Fn stays local");
    }

    #[test]
    fn common_keys() {
        assert_eq!(scancode(&Physical::Code(Code::KeyA)), Some((0x1E, false)));
        assert_eq!(scancode(&Physical::Code(Code::Enter)), Some((0x1C, false)));
        assert_eq!(scancode(&Physical::Code(Code::NumpadEnter)), Some((0x1C, true)));
        assert_eq!(scancode(&Physical::Code(Code::ArrowLeft)), Some((0x4B, true)));
        assert_eq!(scancode(&Physical::Code(Code::AltRight)), Some((0x38, true)));
        assert_eq!(scancode(&Physical::Unidentified(NativeCode::Windows(0xE05B))), Some((0x5B, true)));
    }
}

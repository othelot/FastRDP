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

#[cfg(test)]
mod tests {
    use super::*;

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

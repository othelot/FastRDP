//! FastRDP v1 control-channel wire format.
//!
//! ```text
//! Frame  : u32 LE payload_len | u16 LE type | u16 LE flags (0) | payload
//! String : u16 LE number of UTF-16 code units | UTF-16LE code units (no terminator)
//! ```
//!
//! **Security:** [`Message::Auth`] carries the Windows password. Between client and server it only
//! travels inside QUIC (TLS 1.3, server certificate pinned by the client); between server and
//! provider over a local named pipe that only SYSTEM, LOCAL SERVICE, Administrators and TermService
//! may open. Passwords are kept in [`Secret`] (wiped on drop) and never logged.

use std::fmt;
use std::io::{self, Read, Write};

pub const MAGIC: u32 = 0x5044_5246; // "FRDP"
/// 2: `DisplayRequest::bitrate_kbps` (client-chosen target bitrate)
pub const VERSION: u16 = 2;
/// Large enough for a 256x256 RGBA cursor shape.
pub const MAX_PAYLOAD: u32 = 320 * 1024;
pub const HEADER_SIZE: usize = 8;

/// Matches `WTS_USERNAME_LENGTH` / `WTS_DOMAIN_LENGTH` / `WTS_PASSWORD_LENGTH` (255).
pub const MAX_CREDENTIAL_CHARS: usize = 255;
pub const MAX_TEXT_CHARS: usize = 1024;

pub mod msg {
    pub const C2S_HELLO: u16 = 0x0001;
    pub const C2S_AUTH: u16 = 0x0002;
    pub const C2S_BYE: u16 = 0x0003;
    pub const S2C_HELLO_ACK: u16 = 0x0100;
    pub const S2C_EVENT: u16 = 0x0101;
    /// Video stream description (sent on the control stream when the stream starts or changes).
    pub const S2C_STREAM_INFO: u16 = 0x0102;
    /// Client receive statistics / recovery requests (control stream, ~1 Hz or immediately on loss).
    pub const C2S_FEEDBACK: u16 = 0x0004;
    /// Reliable input event (keys, buttons, wheel, release-all). Client -> server input stream,
    /// and server -> agent pipe.
    pub const INPUT: u16 = 0x0005;
    /// Agent -> server: announces the session the agent runs in.
    pub const AGENT_HELLO: u16 = 0x0006;
    /// client -> server: desired remote display mode (after AUTH, and on every window resize / DPI change)
    pub const C2S_DISPLAY: u16 = 0x0007;
    /// provider -> server -> client: a session component opened a virtual channel
    pub const CHANNEL_OPEN: u16 = 0x0010;
    /// both directions: one fragment of a channel message (see `ChannelData`)
    pub const CHANNEL_DATA: u16 = 0x0011;
    /// provider -> server -> client: channel closed
    pub const CHANNEL_CLOSE: u16 = 0x0012;
    /// server -> client: pointer image (sent when it changes)
    pub const S2C_CURSOR_SHAPE: u16 = 0x0103;
    /// server -> client: pointer visibility / hotspot position (coalesced, at most ~every 8 ms)
    pub const S2C_CURSOR_POS: u16 = 0x0104;
}

/// Events the provider reports to the client (S2C_EVENT).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The provider recorded the request without creating a session (not sent by current providers).
    LogOnlyMode = 1,
    /// value = HRESULT of `IWRdsProtocolListenerCallback::OnConnected`.
    OnConnectedResult = 2,
    /// value = TermService connection id (`IWRdsProtocolConnectionCallback::GetConnectionId`).
    ConnectionId = 3,
    /// value = HRESULT of `IWRdsProtocolConnectionCallback::OnReady`.
    OnReadyResult = 4,
    /// value = Windows session id (`NotifySessionId`).
    SessionId = 5,
    ConnectNotify = 6,
    CommandProcessCreated = 7,
    /// value = session id, text = DOMAIN\user (`LogonNotify`).
    Logon = 8,
    HardwareIdRequested = 9,
    DriverLoad = 10,
    DriverUnload = 11,
    PreDisconnect = 12,
    DisconnectNotify = 13,
    SetErrorInfo = 14,
    Closed = 15,
    Error = 16,
    Rejected = 17,
    EnableWddmIdd = 18,
    LogonErrorRedirected = 19,
    /// value = 1 if the active encoder is a fallback; text = "from -> to: reason".
    EncoderFallback = 20,
    /// Server -> client: how input reaches the session. value = [`input_path`] constant.
    InputStatus = 21,
    /// Provider -> server: session keyboard/mouse devices (the RDP way: terminpt.sys) for session
    /// `text` are ready (value 1) or could not be created (value 0, text = reason).
    InputDevices = 22,
}

/// `Event::InputStatus` values.
pub mod input_path {
    /// input is dropped (no device input, no agent)
    pub const NONE: u32 = 0;
    /// fallback: SendInput through the session agent (no secure desktop, no mouse device)
    pub const AGENT: u32 = 1;
    /// session keyboard/mouse devices like RDP (lock screen, UAC and Ctrl+Alt+Del work)
    pub const DEVICES: u32 = 2;
}

/// Lock-key bits of `InputEvent::Sync` (= ntddkbd.h KEYBOARD_*_LOCK_ON).
pub mod leds {
    pub const SCROLL_LOCK: u8 = 1;
    pub const NUM_LOCK: u8 = 2;
    pub const CAPS_LOCK: u8 = 4;
    pub const KANA_LOCK: u8 = 8;
    pub const ALL: u8 = 0x0F;
}

/// Pause/Break: the only key with an E1 prefix (E1 1D 45). Sent as this scancode in
/// `InputEvent::Key` (extended = false).
pub const SCANCODE_PAUSE: u16 = 0xE11D;

/// Text for the disconnect reason codes TermService reports through `SetErrorInfo` (the RDP
/// ERRINFO_* values, MS-RDPBCGR 2.2.5.1.1). None for 0 / unknown codes.
pub fn disconnect_reason(code: u32) -> Option<&'static str> {
    Some(match code {
        0x1 => "disconnected by an administrator tool on the server",
        0x2 => "logged off by an administrator tool on the server",
        0x3 => "the session reached the idle time limit",
        0x4 => "the logon time limit was reached",
        0x5 => "the session was taken over by another connection (for example a sign-in at the computer itself)",
        0x6 => "the server ran out of memory",
        0x7 => "the server denied the connection",
        0x9 => "the user has no permission to sign in remotely",
        0xA => "the server requires fresh credentials",
        0xB => "disconnected by the user on the server",
        0xC => "the user signed out on the server",
        _ => return None,
    })
}

impl Event {
    pub fn from_u32(v: u32) -> Option<Event> {
        use Event::*;
        Some(match v {
            1 => LogOnlyMode,
            2 => OnConnectedResult,
            3 => ConnectionId,
            4 => OnReadyResult,
            5 => SessionId,
            6 => ConnectNotify,
            7 => CommandProcessCreated,
            8 => Logon,
            9 => HardwareIdRequested,
            10 => DriverLoad,
            11 => DriverUnload,
            12 => PreDisconnect,
            13 => DisconnectNotify,
            14 => SetErrorInfo,
            15 => Closed,
            16 => Error,
            17 => Rejected,
            18 => EnableWddmIdd,
            19 => LogonErrorRedirected,
            20 => EncoderFallback,
            21 => InputStatus,
            22 => InputDevices,
            _ => return None,
        })
    }
}

/// UTF-16 secret that is zeroed on drop and never printed.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(Vec<u16>);

impl Secret {
    #[allow(clippy::should_implement_trait)] // infallible, and a Secret must not come from `parse()` by accident
    pub fn from_str(s: &str) -> Self {
        Secret(s.encode_utf16().collect())
    }
    pub fn from_utf16(v: Vec<u16>) -> Self {
        Secret(v)
    }
    pub fn as_utf16(&self) -> &[u16] {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Overwrites the buffer with zeros (volatile, not optimised away) and clears it.
    pub fn wipe(&mut self) {
        wipe_u16(&mut self.0);
        self.0.clear();
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

pub fn wipe_u16(buf: &mut [u16]) {
    for x in buf.iter_mut() {
        // SAFETY: x is a valid, aligned &mut u16.
        unsafe { std::ptr::write_volatile(x, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

pub fn wipe_u8(buf: &mut [u8]) {
    for x in buf.iter_mut() {
        // SAFETY: x is a valid &mut u8.
        unsafe { std::ptr::write_volatile(x, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub width: u16,
    pub height: u16,
    pub refresh_hz: u16,
    pub client_name: String,
    /// Session experience (`perf::*`, the RDP `WTS_PERF_*` flags), applied by Windows to this
    /// session only - the user's profile and the computer's settings are not changed.
    pub performance_flags: u32,
}

/// Session experience flags (wtsdefs.h `WTS_PERF_*`, same values as RDP's TS_PERF_*).
pub mod perf {
    pub const DISABLE_WALLPAPER: u32 = 0x01;
    pub const DISABLE_FULLWINDOWDRAG: u32 = 0x02;
    pub const DISABLE_MENUANIMATIONS: u32 = 0x04;
    pub const DISABLE_THEMING: u32 = 0x08;
    pub const DISABLE_CURSOR_SHADOW: u32 = 0x20;
    pub const DISABLE_CURSORSETTINGS: u32 = 0x40;
    pub const ENABLE_FONT_SMOOTHING: u32 = 0x80;
    pub const ENABLE_DESKTOP_COMPOSITION: u32 = 0x100;
    pub const VALID: u32 = 0x1EF;

    /// Every effect on (mstsc "LAN").
    pub const BEST_QUALITY: u32 = ENABLE_FONT_SMOOTHING | ENABLE_DESKTOP_COMPOSITION;
    /// No wallpaper / animations; keeps text and window quality.
    pub const BALANCED: u32 = DISABLE_WALLPAPER | DISABLE_MENUANIMATIONS | DISABLE_CURSOR_SHADOW | ENABLE_FONT_SMOOTHING | ENABLE_DESKTOP_COMPOSITION;
    /// Everything off (mstsc "Modem").
    pub const BEST_PERFORMANCE: u32 =
        DISABLE_WALLPAPER | DISABLE_FULLWINDOWDRAG | DISABLE_MENUANIMATIONS | DISABLE_THEMING | DISABLE_CURSOR_SHADOW | DISABLE_CURSORSETTINGS;

    /// (flag, true = the flag DISABLES the effect, label of the effect)
    pub const ITEMS: [(u32, bool, &str); 8] = [
        (DISABLE_WALLPAPER, true, "Desktop background"),
        (DISABLE_FULLWINDOWDRAG, true, "Show window contents while dragging"),
        (DISABLE_MENUANIMATIONS, true, "Menu and window animation"),
        (DISABLE_THEMING, true, "Visual styles"),
        (DISABLE_CURSOR_SHADOW, true, "Pointer shadow"),
        (DISABLE_CURSORSETTINGS, true, "Cursor blinking"),
        (ENABLE_FONT_SMOOTHING, false, "Font smoothing (ClearType)"),
        (ENABLE_DESKTOP_COMPOSITION, false, "Desktop composition (transparency, shadows)"),
    ];

    /// Is effect `i` of `ITEMS` shown with these flags?
    pub fn effect_on(flags: u32, i: usize) -> bool {
        let (f, disables, _) = ITEMS[i];
        (flags & f != 0) != disables
    }

    pub fn set_effect(flags: u32, i: usize, on: bool) -> u32 {
        let (f, disables, _) = ITEMS[i];
        if on != disables { flags | f } else { flags & !f }
    }

    pub fn preset_name(flags: u32) -> Option<&'static str> {
        match flags {
            BEST_QUALITY => Some("best quality"),
            BALANCED => Some("balanced"),
            BEST_PERFORMANCE => Some("best performance"),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Auth {
    pub domain: String,
    pub user: String,
    pub password: Secret,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelloAck {
    pub version: u16,
    pub mode: u32,
    pub server_pid: u32,
    pub server_session_id: u32,
    pub listener: String,
    pub os_info: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventMsg {
    pub event: u32,
    pub value: u32,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    Auth(Auth),
    Bye,
    HelloAck(HelloAck),
    Event(EventMsg),
    StreamInfo(StreamInfo),
    Feedback(Feedback),
    Input(InputEvent),
    AgentHello(AgentHello),
    CursorShape(CursorShape),
    CursorPos(CursorPos),
    Display(DisplayRequest),
    ChannelOpen(ChannelOpen),
    ChannelData(ChannelData),
    ChannelClose(u32),
}

/// A virtual channel opened by a Windows session component (rdpclip: "CLIPRDR", audio:
/// "AUDIO_PLAYBACK_DVC", device redirection: "RDPDR", ...). Ids are per connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelOpen {
    pub id: u32,
    pub name: String,
    pub is_static: bool,
}

/// One fragment of a channel message. Messages larger than [`ChannelData::MAX_FRAGMENT`] are
/// split; `last` marks the final fragment, so message boundaries (one channel write = one message)
/// are preserved end to end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelData {
    pub id: u32,
    pub last: bool,
    pub data: Vec<u8>,
}

impl ChannelData {
    pub const MAX_FRAGMENT: usize = 256 * 1024;

    /// Splits one channel message into fragments.
    pub fn fragments(id: u32, message: &[u8]) -> Vec<ChannelData> {
        if message.is_empty() {
            return vec![ChannelData { id, last: true, data: Vec::new() }];
        }
        let n = message.len().div_ceil(Self::MAX_FRAGMENT);
        message.chunks(Self::MAX_FRAGMENT).enumerate().map(|(i, c)| ChannelData { id, last: i + 1 == n, data: c.to_vec() }).collect()
    }
}

/// Reassembles fragments into channel messages (per channel id).
#[derive(Default)]
pub struct ChannelAssembler {
    partial: std::collections::HashMap<u32, Vec<u8>>,
}

impl ChannelAssembler {
    /// Maximum size of one reassembled message (protects against a misbehaving peer).
    pub const MAX_MESSAGE: usize = 512 * 1024 * 1024;

    /// Returns (id, message) when `d` completes a message.
    pub fn push(&mut self, d: ChannelData) -> Option<(u32, Vec<u8>)> {
        let buf = self.partial.entry(d.id).or_default();
        if buf.len() + d.data.len() > Self::MAX_MESSAGE {
            self.partial.remove(&d.id);
            return None;
        }
        buf.extend_from_slice(&d.data);
        if d.last { self.partial.remove(&d.id).map(|m| (d.id, m)) } else { None }
    }

    pub fn close(&mut self, id: u32) {
        self.partial.remove(&id);
    }
}

/// Remote display mode wanted by the client: desktop size in pixels, refresh, DPI scale (100..500),
/// and the video bitrate target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayRequest {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub scale_percent: u32,
    /// target video bitrate in kbit/s; 0 = automatic (the server's setting, scaled to the size and
    /// refresh rate)
    pub bitrate_kbps: u32,
}

impl DisplayRequest {
    /// Windows display scale steps.
    pub const SCALES: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

    pub const MIN_BITRATE_KBPS: u32 = 1_000;
    pub const MAX_BITRATE_KBPS: u32 = 200_000;

    /// What the remote IDD accepts: width multiple of 8 in 640..=max_w, even height in 480..=max_h,
    /// refresh 24..=240 (else 60), scale snapped to the nearest Windows step; bitrate 0 (automatic)
    /// or MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS.
    pub fn clamped(self, max_w: u32, max_h: u32) -> DisplayRequest {
        let scale = *Self::SCALES.iter().min_by_key(|s| (**s as i64 - self.scale_percent as i64).abs()).unwrap();
        DisplayRequest {
            width: self.width.clamp(640, max_w) & !7,
            height: self.height.clamp(480, max_h) & !1,
            refresh_hz: if (24..=240).contains(&self.refresh_hz) { self.refresh_hz } else { 60 },
            scale_percent: scale,
            bitrate_kbps: if self.bitrate_kbps == 0 { 0 } else { self.bitrate_kbps.clamp(Self::MIN_BITRATE_KBPS, Self::MAX_BITRATE_KBPS) },
        }
    }
}

/// Pointer image. `rgba` is width*height straight-alpha RGBA; a pixel with A=0 and RGB != 0 means
/// "XOR the screen with RGB" (Windows masked-color cursors, e.g. the text I-beam).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorShape {
    pub id: u32,
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

/// Pointer state: hotspot position in remote desktop pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorPos {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub shape_id: u32,
}

pub mod cursor {
    /// IDDCX_CURSOR_SHAPE_TYPE_MASKED_COLOR
    pub const MASKED_COLOR: u32 = 1;
    /// IDDCX_CURSOR_SHAPE_TYPE_ALPHA
    pub const ALPHA: u32 = 2;

    /// Converts a Windows hardware-cursor image (BGRA rows of width*4) to the wire RGBA format.
    pub fn rgba_from_bgra(shape_type: u32, bgra: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bgra.len());
        for p in bgra.as_chunks::<4>().0 {
            let (b, g, r, a) = (p[0], p[1], p[2], p[3]);
            let px = match shape_type {
                MASKED_COLOR if a == 0 => [r, g, b, 255],
                MASKED_COLOR => [r, g, b, 0], // XOR (0,0,0 = transparent)
                _ if a == 0 => [0, 0, 0, 0],
                _ => [r, g, b, a],
            };
            out.extend_from_slice(&px);
        }
        out
    }
}

/// Mouse buttons.
pub mod button {
    pub const LEFT: u8 = 0;
    pub const RIGHT: u8 = 1;
    pub const MIDDLE: u8 = 2;
    pub const X1: u8 = 3;
    pub const X2: u8 = 4;
}

/// Input events. Coordinates are pixels of the streamed desktop (0..width-1, 0..height-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// PC/AT set-1 make code + E0 prefix flag (hardware encoding: NumLock = 0x45, Pause =
    /// [`SCANCODE_PAUSE`]).
    Key {
        scancode: u16,
        extended: bool,
        down: bool,
    },
    /// x/y: remote desktop pixels (client -> server); normalized 0..=65535 on the provider pipe.
    Button {
        button: u8,
        down: bool,
        x: u16,
        y: u16,
    },
    /// WHEEL_DELTA units (120 per notch); `horizontal` = tilt wheel.
    Wheel {
        delta: i16,
        horizontal: bool,
        x: u16,
        y: u16,
    },
    MouseMove {
        x: u16,
        y: u16,
    },
    /// Client lost focus: release every key/button it reported as down.
    ReleaseAll,
    /// Lock-key state of the client keyboard (`leds::*`), sent on connect and when the window gets
    /// focus (RDP: TS_SYNC_EVENT). The session's toggles are set to match.
    Sync {
        leds: u8,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentHello {
    pub session_id: u32,
    pub pid: u32,
    pub version: u16,
}

/// Mouse-move datagrams (client -> server). Unreliable; the receiver keeps only the newest `seq`.
/// `[0xF1][version 1][u32 seq][u16 x][u16 y]` = 10 bytes.
pub mod mouse_datagram {
    pub const TAG: u8 = 0xF1;
    pub fn encode(seq: u32, x: u16, y: u16) -> [u8; 10] {
        let mut b = [0u8; 10];
        b[0] = TAG;
        b[1] = 1;
        b[2..6].copy_from_slice(&seq.to_le_bytes());
        b[6..8].copy_from_slice(&x.to_le_bytes());
        b[8..10].copy_from_slice(&y.to_le_bytes());
        b
    }
    pub fn decode(b: &[u8]) -> Option<(u32, u16, u16)> {
        if b.len() != 10 || b[0] != TAG || b[1] != 1 {
            return None;
        }
        Some((u32::from_le_bytes(b[2..6].try_into().ok()?), u16::from_le_bytes(b[6..8].try_into().ok()?), u16::from_le_bytes(b[8..10].try_into().ok()?)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// fastrdp-encoder kind: 0 x264, 1 openh264, 2 nvenc, 3 mf
    pub encoder: u8,
    pub fallback: bool,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Feedback {
    pub frames_complete: u32,
    pub frames_lost: u32,
    pub fec_recovered: u32,
    pub datagrams_received: u32,
    pub last_frame_id: u32,
    /// 1 = client needs a keyframe (unrecoverable loss / decoder error)
    pub request_idr: u8,
}

#[derive(Debug)]
pub enum WireError {
    Io(io::Error),
    TooLarge(u32),
    BadMagic(u32),
    BadVersion(u16),
    UnknownType(u16),
    Truncated,
    FieldTooLong,
    BadUtf16,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Io(e) => write!(f, "io: {e}"),
            WireError::TooLarge(n) => write!(f, "payload too large: {n}"),
            WireError::BadMagic(m) => write!(f, "bad magic 0x{m:08x}"),
            WireError::BadVersion(v) => write!(f, "unsupported version {v}"),
            WireError::UnknownType(t) => write!(f, "unknown message type 0x{t:04x}"),
            WireError::Truncated => write!(f, "truncated payload"),
            WireError::FieldTooLong => write!(f, "string field too long"),
            WireError::BadUtf16 => write!(f, "invalid UTF-16"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> Self {
        WireError::Io(e)
    }
}

struct W(Vec<u8>);
impl W {
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn units(&mut self, u: &[u16]) {
        let n = u.len().min(u16::MAX as usize);
        self.u16(n as u16);
        for c in &u[..n] {
            self.u16(*c);
        }
    }
    fn str(&mut self, s: &str) {
        let u: Vec<u16> = s.encode_utf16().collect();
        self.units(&u);
    }
}

struct R<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.p + n > self.b.len() {
            return Err(WireError::Truncated);
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn units(&mut self, max: usize) -> Result<Vec<u16>, WireError> {
        let n = self.u16()? as usize;
        if n > max {
            return Err(WireError::FieldTooLong);
        }
        let raw = self.take(n * 2)?;
        Ok(raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
    }
    fn str(&mut self, max: usize) -> Result<String, WireError> {
        String::from_utf16(&self.units(max)?).map_err(|_| WireError::BadUtf16)
    }
}

impl Message {
    pub fn type_code(&self) -> u16 {
        match self {
            Message::Hello(_) => msg::C2S_HELLO,
            Message::Auth(_) => msg::C2S_AUTH,
            Message::Bye => msg::C2S_BYE,
            Message::HelloAck(_) => msg::S2C_HELLO_ACK,
            Message::Event(_) => msg::S2C_EVENT,
            Message::StreamInfo(_) => msg::S2C_STREAM_INFO,
            Message::Feedback(_) => msg::C2S_FEEDBACK,
            Message::Input(_) => msg::INPUT,
            Message::AgentHello(_) => msg::AGENT_HELLO,
            Message::CursorShape(_) => msg::S2C_CURSOR_SHAPE,
            Message::CursorPos(_) => msg::S2C_CURSOR_POS,
            Message::Display(_) => msg::C2S_DISPLAY,
            Message::ChannelOpen(_) => msg::CHANNEL_OPEN,
            Message::ChannelData(_) => msg::CHANNEL_DATA,
            Message::ChannelClose(_) => msg::CHANNEL_CLOSE,
        }
    }

    /// Encodes payload only (without the 8-byte header). Caller must wipe the result for Auth.
    pub fn encode_payload(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        match self {
            Message::Hello(h) => {
                w.u32(MAGIC);
                w.u16(VERSION);
                w.u16(h.width);
                w.u16(h.height);
                w.u16(h.refresh_hz);
                w.str(&h.client_name);
                w.u32(h.performance_flags);
            }
            Message::Auth(a) => {
                w.str(&a.domain);
                w.str(&a.user);
                w.units(a.password.as_utf16());
            }
            Message::Bye => {}
            Message::HelloAck(a) => {
                w.u16(a.version);
                w.u32(a.mode);
                w.u32(a.server_pid);
                w.u32(a.server_session_id);
                w.str(&a.listener);
                w.str(&a.os_info);
            }
            Message::Event(e) => {
                w.u32(e.event);
                w.u32(e.value);
                w.str(&e.text);
            }
            Message::StreamInfo(s) => {
                w.u32(s.width);
                w.u32(s.height);
                w.u32(s.fps);
                w.0.push(s.encoder);
                w.0.push(s.fallback as u8);
                w.str(&s.description);
            }
            Message::Feedback(f) => {
                w.u32(f.frames_complete);
                w.u32(f.frames_lost);
                w.u32(f.fec_recovered);
                w.u32(f.datagrams_received);
                w.u32(f.last_frame_id);
                w.0.push(f.request_idr);
            }
            Message::Input(e) => match *e {
                InputEvent::Key { scancode, extended, down } => {
                    w.0.push(1);
                    w.u16(scancode);
                    w.0.push((extended as u8) | ((down as u8) << 1));
                }
                InputEvent::Button { button, down, x, y } => {
                    w.0.push(2);
                    w.0.push(button);
                    w.0.push(down as u8);
                    w.u16(x);
                    w.u16(y);
                }
                InputEvent::Wheel { delta, horizontal, x, y } => {
                    w.0.push(3);
                    w.u16(delta as u16);
                    w.0.push(horizontal as u8);
                    w.u16(x);
                    w.u16(y);
                }
                InputEvent::MouseMove { x, y } => {
                    w.0.push(4);
                    w.u16(x);
                    w.u16(y);
                }
                InputEvent::ReleaseAll => w.0.push(5),
                InputEvent::Sync { leds } => {
                    w.0.push(6);
                    w.0.push(leds);
                }
            },
            Message::AgentHello(a) => {
                w.u32(a.session_id);
                w.u32(a.pid);
                w.u16(a.version);
            }
            Message::CursorShape(c) => {
                w.u32(c.id);
                w.u16(c.width);
                w.u16(c.height);
                w.u16(c.hot_x);
                w.u16(c.hot_y);
                w.0.extend_from_slice(&c.rgba);
            }
            Message::ChannelOpen(c) => {
                w.u32(c.id);
                w.0.push(c.is_static as u8);
                w.str(&c.name);
            }
            Message::ChannelData(c) => {
                w.u32(c.id);
                w.0.push(c.last as u8);
                w.u32(c.data.len() as u32);
                w.0.extend_from_slice(&c.data);
            }
            Message::ChannelClose(id) => w.u32(*id),
            Message::Display(d) => {
                w.u32(d.width);
                w.u32(d.height);
                w.u32(d.refresh_hz);
                w.u32(d.scale_percent);
                w.u32(d.bitrate_kbps);
            }
            Message::CursorPos(c) => {
                w.0.push(c.visible as u8);
                w.u32(c.x as u32);
                w.u32(c.y as u32);
                w.u32(c.shape_id);
            }
        }
        w.0
    }

    pub fn decode(ty: u16, payload: &[u8]) -> Result<Message, WireError> {
        let mut r = R { b: payload, p: 0 };
        Ok(match ty {
            msg::C2S_HELLO => {
                let magic = r.u32()?;
                if magic != MAGIC {
                    return Err(WireError::BadMagic(magic));
                }
                let v = r.u16()?;
                if v != VERSION {
                    return Err(WireError::BadVersion(v));
                }
                Message::Hello(Hello {
                    width: r.u16()?,
                    height: r.u16()?,
                    refresh_hz: r.u16()?,
                    client_name: r.str(MAX_TEXT_CHARS)?,
                    performance_flags: r.u32()? & perf::VALID,
                })
            }
            msg::C2S_AUTH => Message::Auth(Auth {
                domain: r.str(MAX_CREDENTIAL_CHARS)?,
                user: r.str(MAX_CREDENTIAL_CHARS)?,
                password: Secret::from_utf16(r.units(MAX_CREDENTIAL_CHARS)?),
            }),
            msg::C2S_BYE => Message::Bye,
            msg::S2C_HELLO_ACK => Message::HelloAck(HelloAck {
                version: r.u16()?,
                mode: r.u32()?,
                server_pid: r.u32()?,
                server_session_id: r.u32()?,
                listener: r.str(MAX_TEXT_CHARS)?,
                os_info: r.str(MAX_TEXT_CHARS)?,
            }),
            msg::S2C_EVENT => Message::Event(EventMsg { event: r.u32()?, value: r.u32()?, text: r.str(MAX_TEXT_CHARS)? }),
            msg::S2C_STREAM_INFO => Message::StreamInfo(StreamInfo {
                width: r.u32()?,
                height: r.u32()?,
                fps: r.u32()?,
                encoder: r.take(1)?[0],
                fallback: r.take(1)?[0] != 0,
                description: r.str(MAX_TEXT_CHARS)?,
            }),
            msg::INPUT => Message::Input(match r.take(1)?[0] {
                1 => {
                    let scancode = r.u16()?;
                    let f = r.take(1)?[0];
                    InputEvent::Key { scancode, extended: f & 1 != 0, down: f & 2 != 0 }
                }
                2 => {
                    let button = r.take(1)?[0];
                    let down = r.take(1)?[0] != 0;
                    InputEvent::Button { button, down, x: r.u16()?, y: r.u16()? }
                }
                3 => {
                    let delta = r.u16()? as i16;
                    let horizontal = r.take(1)?[0] != 0;
                    InputEvent::Wheel { delta, horizontal, x: r.u16()?, y: r.u16()? }
                }
                4 => InputEvent::MouseMove { x: r.u16()?, y: r.u16()? },
                5 => InputEvent::ReleaseAll,
                6 => InputEvent::Sync { leds: r.take(1)?[0] & leds::ALL },
                k => return Err(WireError::UnknownType(0x0500 | k as u16)),
            }),
            msg::AGENT_HELLO => Message::AgentHello(AgentHello { session_id: r.u32()?, pid: r.u32()?, version: r.u16()? }),
            msg::S2C_CURSOR_SHAPE => {
                let (id, width, height, hot_x, hot_y) = (r.u32()?, r.u16()?, r.u16()?, r.u16()?, r.u16()?);
                if width > 256 || height > 256 {
                    return Err(WireError::FieldTooLong);
                }
                let rgba = r.take(width as usize * height as usize * 4)?.to_vec();
                Message::CursorShape(CursorShape { id, width, height, hot_x, hot_y, rgba })
            }
            msg::CHANNEL_OPEN => Message::ChannelOpen(ChannelOpen { id: r.u32()?, is_static: r.take(1)?[0] != 0, name: r.str(256)? }),
            msg::CHANNEL_DATA => {
                let (id, last, len) = (r.u32()?, r.take(1)?[0] != 0, r.u32()? as usize);
                if len > ChannelData::MAX_FRAGMENT {
                    return Err(WireError::FieldTooLong);
                }
                Message::ChannelData(ChannelData { id, last, data: r.take(len)?.to_vec() })
            }
            msg::CHANNEL_CLOSE => Message::ChannelClose(r.u32()?),
            msg::C2S_DISPLAY => {
                Message::Display(DisplayRequest { width: r.u32()?, height: r.u32()?, refresh_hz: r.u32()?, scale_percent: r.u32()?, bitrate_kbps: r.u32()? })
            }
            msg::S2C_CURSOR_POS => Message::CursorPos(CursorPos { visible: r.take(1)?[0] != 0, x: r.u32()? as i32, y: r.u32()? as i32, shape_id: r.u32()? }),
            msg::C2S_FEEDBACK => Message::Feedback(Feedback {
                frames_complete: r.u32()?,
                frames_lost: r.u32()?,
                fec_recovered: r.u32()?,
                datagrams_received: r.u32()?,
                last_frame_id: r.u32()?,
                request_idr: r.take(1)?[0],
            }),
            t => return Err(WireError::UnknownType(t)),
        })
    }
}

/// Async framing for QUIC streams (feature `tokio`). Same wire format as the sync functions.
#[cfg(feature = "tokio")]
pub mod asyncio {
    use super::*;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    pub async fn write_message<W: AsyncWrite + Unpin>(w: &mut W, m: &Message) -> Result<(), WireError> {
        let mut payload = m.encode_payload();
        let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&m.type_code().to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&payload);
        let r = w.write_all(&buf).await;
        wipe_u8(&mut payload);
        wipe_u8(&mut buf);
        r?;
        Ok(())
    }

    pub async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> Result<Message, WireError> {
        let mut hdr = [0u8; HEADER_SIZE];
        r.read_exact(&mut hdr).await?;
        let len = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let ty = u16::from_le_bytes(hdr[4..6].try_into().unwrap());
        if len > MAX_PAYLOAD {
            return Err(WireError::TooLarge(len));
        }
        let mut payload = vec![0u8; len as usize];
        r.read_exact(&mut payload).await?;
        let res = Message::decode(ty, &payload);
        wipe_u8(&mut payload);
        res
    }
}

/// Writes one frame. The temporary payload buffer is wiped (it may contain a password).
pub fn write_message<Wr: Write>(w: &mut Wr, m: &Message) -> Result<(), WireError> {
    let mut payload = m.encode_payload();
    let res = (|| {
        if payload.len() > MAX_PAYLOAD as usize {
            return Err(WireError::TooLarge(payload.len() as u32));
        }
        let mut hdr = [0u8; HEADER_SIZE];
        hdr[0..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        hdr[4..6].copy_from_slice(&m.type_code().to_le_bytes());
        w.write_all(&hdr)?;
        w.write_all(&payload)?;
        w.flush()?;
        Ok(())
    })();
    wipe_u8(&mut payload);
    res
}

/// Reads one frame and decodes it. The raw payload buffer is wiped before returning.
pub fn read_message<Rd: Read>(r: &mut Rd) -> Result<Message, WireError> {
    let mut hdr = [0u8; HEADER_SIZE];
    r.read_exact(&mut hdr)?;
    let len = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
    let ty = u16::from_le_bytes(hdr[4..6].try_into().unwrap());
    if len > MAX_PAYLOAD {
        return Err(WireError::TooLarge(len));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    let res = Message::decode(ty, &payload);
    wipe_u8(&mut payload);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: Message) {
        let mut buf = Vec::new();
        write_message(&mut buf, &m).unwrap();
        let back = read_message(&mut buf.as_slice()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn roundtrip_all() {
        roundtrip(Message::Hello(Hello {
            width: 1920, height: 1080, refresh_hz: 60, client_name: "클라이언트".into(), performance_flags: perf::BALANCED
        }));
        for i in 0..perf::ITEMS.len() {
            assert!(perf::effect_on(perf::BEST_QUALITY, i), "best quality shows everything ({i})");
            assert!(!perf::effect_on(perf::BEST_PERFORMANCE, i), "best performance shows nothing ({i})");
            assert!(perf::effect_on(perf::set_effect(perf::BEST_PERFORMANCE, i, true), i));
            assert!(!perf::effect_on(perf::set_effect(perf::BEST_QUALITY, i, false), i));
        }
        assert_eq!(perf::BEST_PERFORMANCE & !perf::VALID, 0);
        roundtrip(Message::Auth(Auth { domain: "".into(), user: "alice".into(), password: Secret::from_str("p@ss") }));
        roundtrip(Message::Bye);
        roundtrip(Message::HelloAck(HelloAck {
            version: 1,
            mode: 0,
            server_pid: 4,
            server_session_id: 0,
            listener: "FastRDP".into(),
            os_info: "10.0.26300".into(),
        }));
        roundtrip(Message::Event(EventMsg { event: Event::SessionId as u32, value: 3, text: String::new() }));
        roundtrip(Message::StreamInfo(StreamInfo { width: 1920, height: 1080, fps: 60, encoder: 0, fallback: true, description: "x264".into() }));
        roundtrip(Message::Feedback(Feedback {
            frames_complete: 59,
            frames_lost: 1,
            fec_recovered: 3,
            datagrams_received: 900,
            last_frame_id: 77,
            request_idr: 1,
        }));
        for e in [
            InputEvent::Key { scancode: 0x1D, extended: true, down: true },
            InputEvent::Key { scancode: 0x1E, extended: false, down: false },
            InputEvent::Button { button: button::RIGHT, down: true, x: 1919, y: 0 },
            InputEvent::Wheel { delta: -240, horizontal: false, x: 5, y: 6 },
            InputEvent::MouseMove { x: 100, y: 200 },
            InputEvent::ReleaseAll,
            InputEvent::Sync { leds: leds::CAPS_LOCK | leds::NUM_LOCK },
        ] {
            roundtrip(Message::Input(e));
        }
        roundtrip(Message::AgentHello(AgentHello { session_id: 9, pid: 1234, version: 1 }));
        roundtrip(Message::CursorShape(CursorShape { id: 3, width: 2, height: 1, hot_x: 1, hot_y: 0, rgba: vec![1, 2, 3, 4, 5, 6, 7, 8] }));
        roundtrip(Message::CursorPos(CursorPos { visible: true, x: -4, y: 1079, shape_id: 3 }));
        assert!(disconnect_reason(5).unwrap().contains("taken over"));
        roundtrip(Message::Display(DisplayRequest { width: 2560, height: 1440, refresh_hz: 60, scale_percent: 150, bitrate_kbps: 25_000 }));
        roundtrip(Message::ChannelOpen(ChannelOpen { id: 3, name: "CLIPRDR".into(), is_static: true }));
        roundtrip(Message::ChannelClose(3));
        let big: Vec<u8> = (0..(ChannelData::MAX_FRAGMENT * 2 + 17)).map(|i| i as u8).collect();
        let frags = ChannelData::fragments(5, &big);
        assert_eq!(frags.len(), 3);
        let mut asm = ChannelAssembler::default();
        let mut out = None;
        for f in frags {
            roundtrip(Message::ChannelData(f.clone()));
            out = asm.push(f).or(out);
        }
        assert_eq!(out, Some((5, big)));
        assert_eq!(asm.push(ChannelData { id: 9, last: true, data: vec![] }), Some((9, vec![])));
        let c = DisplayRequest { width: 1923, height: 1081, refresh_hz: 1000, scale_percent: 160, bitrate_kbps: 0 }.clamped(3840, 2160);
        assert_eq!(c.bitrate_kbps, 0, "automatic stays automatic");
        assert_eq!((c.width, c.height, c.refresh_hz, c.scale_percent), (1920, 1080, 60, 150));
        let c = DisplayRequest { width: 100, height: 9000, refresh_hz: 144, scale_percent: 90, bitrate_kbps: 50 }.clamped(3840, 2160);
        assert_eq!(c.bitrate_kbps, DisplayRequest::MIN_BITRATE_KBPS);
        assert_eq!((c.width, c.height, c.refresh_hz, c.scale_percent), (640, 2160, 144, 100));
        assert!(disconnect_reason(0).is_none());
        let big = vec![0u8; 256 * 256 * 4];
        roundtrip(Message::CursorShape(CursorShape { id: 4, width: 256, height: 256, hot_x: 0, hot_y: 0, rgba: big }));
        // masked color: opaque, transparent, XOR
        let px = cursor::rgba_from_bgra(cursor::MASKED_COLOR, &[1, 2, 3, 0, 0, 0, 0, 255, 255, 255, 255, 255]);
        assert_eq!(px, vec![3, 2, 1, 255, 0, 0, 0, 0, 255, 255, 255, 0]);
        assert_eq!(cursor::rgba_from_bgra(cursor::ALPHA, &[9, 9, 9, 0, 1, 2, 3, 128]), vec![0, 0, 0, 0, 3, 2, 1, 128]);
        assert_eq!(mouse_datagram::decode(&mouse_datagram::encode(7, 1919, 1079)), Some((7, 1919, 1079)));
        assert_eq!(mouse_datagram::decode(&[0xF1, 2, 0, 0, 0, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn rejects_bad_magic_and_oversize() {
        let mut p = Message::Hello(Hello { width: 1, height: 1, refresh_hz: 1, client_name: String::new(), performance_flags: 0 }).encode_payload();
        p[0] ^= 0xFF;
        assert!(matches!(Message::decode(msg::C2S_HELLO, &p), Err(WireError::BadMagic(_))));
        let mut hdr = Vec::new();
        hdr.extend_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        hdr.extend_from_slice(&msg::C2S_BYE.to_le_bytes());
        hdr.extend_from_slice(&0u16.to_le_bytes());
        assert!(matches!(read_message(&mut hdr.as_slice()), Err(WireError::TooLarge(_))));
    }

    #[test]
    fn rejects_overlong_credentials() {
        let long = "x".repeat(MAX_CREDENTIAL_CHARS + 1);
        let p = Message::Auth(Auth { domain: String::new(), user: long, password: Secret::from_str("a") }).encode_payload();
        assert!(matches!(Message::decode(msg::C2S_AUTH, &p), Err(WireError::FieldTooLong)));
    }

    #[test]
    fn truncated_is_error() {
        let p = Message::Event(EventMsg { event: 1, value: 2, text: "abc".into() }).encode_payload();
        assert!(matches!(Message::decode(msg::S2C_EVENT, &p[..p.len() - 1]), Err(WireError::Truncated)));
    }

    #[test]
    fn secret_debug_redacted_and_wipe() {
        let mut s = Secret::from_str("hunter2");
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        s.wipe();
        assert!(s.is_empty());
    }
}

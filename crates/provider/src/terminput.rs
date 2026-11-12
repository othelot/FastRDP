//! Session keyboard and mouse the way Microsoft's RDP stack does it (rdpcorets.dll):
//!
//! 1. software devices `TS_INPT\TS_KBD` / `TS_INPT\TS_MOU` carrying `DEVPKEY_Device_SessionId`;
//!    the inbox termkbd.inf / termmou.inf bind terminpt.sys + kbdclass/mouclass to them
//!    ("Remote Desktop Keyboard/Mouse Device");
//! 2. `\\.\TermInptCDO\<session><PDO name>` - terminpt's control device; only
//!    `NT SERVICE\TermService` may open it (we run inside TermService);
//! 3. `IOCTL_TERMINPT_GET_CLASS_HANDLE` returns a handle to the class device for TermService,
//!    handed over in `IWRdsProtocolConnection::GetInputHandles` (the session then HAS a keyboard
//!    and a mouse: the cursor is shown, the lock screen / UAC / Ctrl+Alt+Del accept input);
//! 4. input = `KEYBOARD_INPUT_DATA[]` / `MOUSE_INPUT_DATA[]` through terminpt IOCTLs.
//!
//! These are undocumented Windows internals: every step is checked, and on failure the server
//! falls back to the session agent (SendInput).
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use protocol::{InputEvent, SCANCODE_PAUSE, button};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{CM_Get_DevNode_PropertyW, CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CR_SUCCESS};
use windows_sys::Win32::Devices::Enumeration::Pnp::{
    HSWDEVICE, SW_DEVICE_CREATE_INFO, SWDeviceCapabilitiesDriverRequired, SWDeviceCapabilitiesSilentInstall, SwDeviceClose, SwDeviceCreate,
};
use windows_sys::Win32::Devices::Properties::{
    DEVPKEY_Device_PDOName, DEVPKEY_Device_SessionId, DEVPROP_STORE_SYSTEM, DEVPROP_TYPE_UINT32, DEVPROPCOMPKEY, DEVPROPERTY,
};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

use crate::{logi, logw};

/// terminpt.sys IOCTLs (FILE_DEVICE_TERMSRV 0x38, METHOD_BUFFERED, FILE_WRITE_ACCESS).
const IOCTL_TERMINPT_KEYBOARD_INPUT: u32 = 0x0038_8804; // in: KEYBOARD_INPUT_DATA[] (12 B each)
const IOCTL_TERMINPT_MOUSE_INPUT: u32 = 0x0038_8808; // in: MOUSE_INPUT_DATA[] (24 B each)
const IOCTL_TERMINPT_GET_CLASS_HANDLE: u32 = 0x0038_880C; // out: HANDLE (caller must be in session 0)
const FILE_WRITE_DATA: u32 = 0x0002;

// KEYBOARD_INPUT_DATA.Flags / MOUSE_INPUT_DATA (ntddkbd.h / ntddmou.h)
const KEY_BREAK: u16 = 1;
const KEY_E0: u16 = 2;
const KEY_E1: u16 = 4;
/// Sets the session's lock-key state; ExtraInformation = KEYBOARD_*_LOCK_ON bits (RDP sync event,
/// rdpcorets `SendIndicatorstoKeyboard`).
const KEY_TERMSRV_SET_LED: u16 = 8;
const MOUSE_MOVE_ABSOLUTE: u16 = 1;
const MOUSE_VIRTUAL_DESKTOP: u16 = 2;
const MOUSE_WHEEL: u16 = 0x0400;
const MOUSE_HWHEEL: u16 = 0x0800;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct KeyboardInputData {
    pub unit_id: u16,
    pub make_code: u16,
    pub flags: u16,
    pub reserved: u16,
    pub extra_information: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct MouseInputData {
    pub unit_id: u16,
    pub flags: u16,
    pub button_flags: u16,
    pub button_data: u16,
    pub raw_buttons: u32,
    pub last_x: i32,
    pub last_y: i32,
    pub extra_information: u32,
}

const _: () = assert!(std::mem::size_of::<KeyboardInputData>() == 12);
const _: () = assert!(std::mem::size_of::<MouseInputData>() == 24);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn multi_sz(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0, 0]).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Keyboard,
    Mouse,
}

struct Created {
    event: HANDLE,
    hr: std::sync::atomic::AtomicI32,
    instance: Mutex<String>,
}

unsafe extern "system" fn on_created(_h: HSWDEVICE, hr: i32, ctx: *const c_void, instance: *const u16) {
    // SAFETY: ctx is the `Created` passed to SwDeviceCreate, alive until the event is waited on
    // (or leaked on timeout); `instance` is a NUL-terminated string valid for the call.
    unsafe {
        let c = &*(ctx as *const Created);
        if !instance.is_null() {
            let mut n = 0;
            while *instance.add(n) != 0 {
                n += 1;
            }
            *c.instance.lock().unwrap() = String::from_utf16_lossy(std::slice::from_raw_parts(instance, n));
        }
        c.hr.store(hr, std::sync::atomic::Ordering::SeqCst);
        SetEvent(c.event);
    }
}

/// One terminpt-backed device (removed again when dropped).
struct TiDevice {
    kind: Kind,
    hsw: HSWDEVICE,
    instance: String,
    cdo: HANDLE,
}

// SAFETY: plain kernel/cfgmgr handles, used under the SessionInput mutexes.
unsafe impl Send for TiDevice {}
unsafe impl Sync for TiDevice {}

impl Drop for TiDevice {
    fn drop(&mut self) {
        // SAFETY: owned handles. SwDeviceClose removes the software device (lifetime = handle).
        unsafe {
            if !self.cdo.is_null() {
                CloseHandle(self.cdo);
            }
            if !self.hsw.is_null() {
                SwDeviceClose(self.hsw);
            }
        }
        logi!("terminput: {:?} device {} removed", self.kind, self.instance);
    }
}

fn pdo_name(instance: &str) -> Option<String> {
    let id = wide(instance);
    let mut dn = 0u32;
    // SAFETY: valid in/out pointers.
    unsafe {
        if CM_Locate_DevNodeW(&mut dn, id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) != CR_SUCCESS {
            return None;
        }
        let mut ty = 0u32;
        let mut buf = [0u16; 256];
        let mut size = (buf.len() * 2) as u32;
        if CM_Get_DevNode_PropertyW(dn, &DEVPKEY_Device_PDOName, &mut ty, buf.as_mut_ptr() as *mut u8, &mut size, 0) != CR_SUCCESS {
            return None;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        (n > 0).then(|| String::from_utf16_lossy(&buf[..n]))
    }
}

impl TiDevice {
    fn create(session: u32, kind: Kind, tag: &str) -> Result<(TiDevice, HANDLE), String> {
        let (hwid, name) = match kind {
            Kind::Keyboard => ("TS_INPT\\TS_KBD", "KBD"),
            Kind::Mouse => ("TS_INPT\\TS_MOU", "MOU"),
        };
        let instance_id = wide(&format!("S{session}_{name}_{tag}"));
        let hwids = multi_sz(hwid);
        let compat = multi_sz("TI_COMPAT_DEVICE");
        let desc = wide(&format!("FastRDP {} (session {session})", if kind == Kind::Keyboard { "keyboard" } else { "mouse" }));
        let enumerator = wide("FastRdpInput");
        let parent = wide("HTREE\\ROOT\\0");
        let info = SW_DEVICE_CREATE_INFO {
            cbSize: std::mem::size_of::<SW_DEVICE_CREATE_INFO>() as u32,
            pszInstanceId: instance_id.as_ptr(),
            pszzHardwareIds: hwids.as_ptr(),
            pszzCompatibleIds: compat.as_ptr(),
            pContainerId: std::ptr::null(),
            CapabilityFlags: (SWDeviceCapabilitiesSilentInstall | SWDeviceCapabilitiesDriverRequired) as u32,
            pszDeviceDescription: desc.as_ptr(),
            pszDeviceLocation: std::ptr::null(),
            pSecurityDescriptor: std::ptr::null(),
        };
        let mut sid = session;
        let props = [DEVPROPERTY {
            CompKey: DEVPROPCOMPKEY { Key: DEVPKEY_Device_SessionId, Store: DEVPROP_STORE_SYSTEM, LocaleName: std::ptr::null() },
            Type: DEVPROP_TYPE_UINT32,
            BufferSize: 4,
            Buffer: &mut sid as *mut u32 as *mut c_void,
        }];
        // SAFETY: event + context outlive the callback (context leaked if the callback never comes).
        let ctx = Box::new(Created {
            event: unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) },
            hr: std::sync::atomic::AtomicI32::new(1),
            instance: Mutex::new(String::new()),
        });
        let mut hsw: HSWDEVICE = std::ptr::null_mut();
        let t0 = Instant::now();
        // SAFETY: all pointers valid for the call; the callback gets `ctx`.
        let hr = unsafe {
            SwDeviceCreate(
                enumerator.as_ptr(),
                parent.as_ptr(),
                &info,
                props.len() as u32,
                props.as_ptr(),
                Some(on_created),
                &*ctx as *const Created as *const c_void,
                &mut hsw,
            )
        };
        if hr < 0 {
            // SAFETY: owned event.
            unsafe { CloseHandle(ctx.event) };
            return Err(format!("SwDeviceCreate({hwid}) failed 0x{:08X}", hr as u32));
        }
        // SAFETY: valid event handle.
        if unsafe { WaitForSingleObject(ctx.event, 10_000) } != 0 {
            // SAFETY: owned handle; the callback may still run later -> leak the context.
            unsafe { SwDeviceClose(hsw) };
            std::mem::forget(ctx);
            return Err(format!("SwDeviceCreate({hwid}): no creation callback within 10 s"));
        }
        let created_hr = ctx.hr.load(std::sync::atomic::Ordering::SeqCst);
        let instance = ctx.instance.lock().unwrap().clone();
        // SAFETY: owned event, callback already ran.
        unsafe { CloseHandle(ctx.event) };
        drop(ctx);
        let mut dev = TiDevice { kind, hsw, instance, cdo: std::ptr::null_mut() };
        if created_hr < 0 {
            return Err(format!("{hwid}: creation callback 0x{:08X}", created_hr as u32));
        }
        // terminpt registers the device for its session when the stack starts: poll PDO + CDO.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last_err = String::from("PDO name not available");
        while Instant::now() < deadline {
            if let Some(pdo) = pdo_name(&dev.instance) {
                let path = wide(&format!("\\\\.\\TermInptCDO\\{session}{pdo}"));
                // SAFETY: valid path; handle owned by `dev`.
                let h = unsafe {
                    CreateFileW(
                        path.as_ptr(),
                        FILE_WRITE_DATA,
                        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        0,
                        std::ptr::null_mut(),
                    )
                };
                if h != INVALID_HANDLE_VALUE && !h.is_null() {
                    dev.cdo = h;
                    let mut class: HANDLE = std::ptr::null_mut();
                    let mut ret = 0u32;
                    // SAFETY: 8-byte output buffer for the handle.
                    let ok = unsafe {
                        DeviceIoControl(
                            h,
                            IOCTL_TERMINPT_GET_CLASS_HANDLE,
                            std::ptr::null(),
                            0,
                            &mut class as *mut HANDLE as *mut c_void,
                            8,
                            &mut ret,
                            std::ptr::null_mut(),
                        )
                    };
                    if ok == 0 || ret != 8 || class.is_null() {
                        // SAFETY: trivial.
                        return Err(format!("{hwid}: GET_CLASS_HANDLE failed (error {})", unsafe { GetLastError() }));
                    }
                    logi!("terminput: {kind:?} {} pdo={pdo} ready in {} ms", dev.instance, t0.elapsed().as_millis());
                    return Ok((dev, class));
                }
                // SAFETY: trivial.
                last_err = format!("open TermInptCDO\\{session}{pdo}: error {}", unsafe { GetLastError() });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!("{hwid} {}: {last_err}", dev.instance))
    }

    fn ioctl<T>(&self, code: u32, records: &[T]) -> bool {
        if records.is_empty() {
            return true;
        }
        let mut ret = 0u32;
        // SAFETY: METHOD_BUFFERED input of whole records.
        unsafe {
            DeviceIoControl(
                self.cdo,
                code,
                records.as_ptr() as *const c_void,
                std::mem::size_of_val(records) as u32,
                std::ptr::null_mut(),
                0,
                &mut ret,
                std::ptr::null_mut(),
            ) != 0
        }
    }
}

/// Keyboard + mouse of one session. Created in `GetInputHandles`, dropped on disconnect/close.
pub struct SessionInput {
    pub session: u32,
    kbd: TiDevice,
    mou: TiDevice,
    /// class handles handed to TermService (owned by TermService once returned)
    pub handles: (HANDLE, HANDLE),
    keys_down: Mutex<HashSet<(u16, bool)>>,
    buttons_down: Mutex<u16>,
    errors: std::sync::atomic::AtomicU32,
}

// SAFETY: see TiDevice; the class handles are only passed through.
unsafe impl Send for SessionInput {}
unsafe impl Sync for SessionInput {}

/// Keyboard records for one key transition (hardware encoding, see `InputEvent::Key`).
pub fn keyboard_records(scancode: u16, extended: bool, down: bool) -> Vec<KeyboardInputData> {
    let brk = if down { 0 } else { KEY_BREAK };
    let rec = |make_code, flags| KeyboardInputData { make_code, flags, ..Default::default() };
    match scancode {
        // Pause: E1 1D 45 (there is no separate break on real keyboards; send it anyway on release)
        SCANCODE_PAUSE => vec![rec(0x1D, KEY_E1 | brk), rec(0x45, brk)],
        // Korean Hangul / Hanja (set-1 F2 / F1 are "make only" codes): press + release at once
        0xF1 | 0xF2 if down => vec![rec(scancode & 0x7F, 0), rec(scancode & 0x7F, KEY_BREAK)],
        0xF1 | 0xF2 => vec![],
        _ => vec![rec(scancode & 0xFF, brk | if extended { KEY_E0 } else { 0 })],
    }
}

fn button_flags(b: u8, down: bool) -> u16 {
    let base: u16 = match b {
        button::LEFT => 0x0001,
        button::RIGHT => 0x0004,
        button::MIDDLE => 0x0010,
        button::X1 => 0x0040,
        button::X2 => 0x0100,
        _ => return 0,
    };
    if down { base } else { base << 1 }
}

/// Mouse record for an event whose coordinates are normalized 0..=65535 (provider pipe).
pub fn mouse_record(e: &InputEvent) -> Option<MouseInputData> {
    let abs = |x: u16, y: u16| MouseInputData { flags: MOUSE_MOVE_ABSOLUTE | MOUSE_VIRTUAL_DESKTOP, last_x: x as i32, last_y: y as i32, ..Default::default() };
    Some(match *e {
        InputEvent::MouseMove { x, y } => abs(x, y),
        InputEvent::Button { button, down, x, y } => MouseInputData { button_flags: button_flags(button, down), ..abs(x, y) },
        InputEvent::Wheel { delta, horizontal, x, y } => {
            MouseInputData { button_flags: if horizontal { MOUSE_HWHEEL } else { MOUSE_WHEEL }, button_data: delta as u16, ..abs(x, y) }
        }
        InputEvent::Key { .. } | InputEvent::ReleaseAll | InputEvent::Sync { .. } => return None,
    })
}

impl SessionInput {
    pub fn create(session: u32, tag: &str) -> Result<SessionInput, String> {
        let (kbd, kh) = TiDevice::create(session, Kind::Keyboard, tag)?;
        let (mou, mh) = TiDevice::create(session, Kind::Mouse, tag)?;
        Ok(SessionInput {
            session,
            kbd,
            mou,
            handles: (kh, mh),
            keys_down: Mutex::new(HashSet::new()),
            buttons_down: Mutex::new(0),
            errors: Default::default(),
        })
    }

    fn failed(&self, what: &str) {
        let n = self.errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 5 || n.is_multiple_of(1000) {
            // SAFETY: trivial.
            logw!("terminput s{}: {what} IOCTL failed (error {}, {} failures)", self.session, unsafe { GetLastError() }, n + 1);
        }
    }

    pub fn inject(&self, e: &InputEvent) {
        match *e {
            InputEvent::Key { scancode, extended, down } => {
                {
                    let mut k = self.keys_down.lock().unwrap();
                    if down {
                        k.insert((scancode, extended));
                    } else {
                        k.remove(&(scancode, extended));
                    }
                }
                if !self.kbd.ioctl(IOCTL_TERMINPT_KEYBOARD_INPUT, &keyboard_records(scancode, extended, down)) {
                    self.failed("keyboard");
                }
            }
            InputEvent::ReleaseAll => self.release_all(),
            InputEvent::Sync { leds } => {
                let r = KeyboardInputData { flags: KEY_TERMSRV_SET_LED, extra_information: leds as u32, ..Default::default() };
                if !self.kbd.ioctl(IOCTL_TERMINPT_KEYBOARD_INPUT, &[r]) {
                    self.failed("keyboard LED sync");
                }
            }
            _ => {
                if let InputEvent::Button { button, down, .. } = *e {
                    let f = button_flags(button, true);
                    let mut b = self.buttons_down.lock().unwrap();
                    *b = if down { *b | f } else { *b & !f };
                }
                if let Some(r) = mouse_record(e)
                    && !self.mou.ioctl(IOCTL_TERMINPT_MOUSE_INPUT, &[r])
                {
                    self.failed("mouse");
                }
            }
        }
    }

    pub fn release_all(&self) {
        let keys: Vec<_> = self.keys_down.lock().unwrap().drain().collect();
        let recs: Vec<_> = keys.iter().flat_map(|&(s, e)| keyboard_records(s, e, false)).collect();
        self.kbd.ioctl(IOCTL_TERMINPT_KEYBOARD_INPUT, &recs);
        let down = std::mem::take(&mut *self.buttons_down.lock().unwrap());
        if down != 0 {
            // every *_DOWN bit shifted left is the matching *_UP bit
            let up = MouseInputData { button_flags: down << 1, ..Default::default() };
            self.mou.ioctl(IOCTL_TERMINPT_MOUSE_INPUT, &[up]);
        }
    }
}

impl Drop for SessionInput {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_encoding() {
        assert_eq!(keyboard_records(0x1E, false, true), vec![KeyboardInputData { make_code: 0x1E, ..Default::default() }]);
        assert_eq!(keyboard_records(0x4B, true, false)[0].flags, KEY_E0 | KEY_BREAK);
        let p = keyboard_records(SCANCODE_PAUSE, false, true);
        assert_eq!((p[0].make_code, p[0].flags, p[1].make_code, p[1].flags), (0x1D, KEY_E1, 0x45, 0));
        assert_eq!(keyboard_records(0x45, false, true)[0].flags, 0, "NumLock has no prefix");
        assert_eq!(keyboard_records(0xF2, false, true).len(), 2);
        assert!(keyboard_records(0xF2, false, false).is_empty());
    }

    #[test]
    fn mouse_encoding() {
        let r = mouse_record(&InputEvent::Button { button: button::RIGHT, down: false, x: 65535, y: 0 }).unwrap();
        assert_eq!((r.flags, r.button_flags, r.last_x, r.last_y), (MOUSE_MOVE_ABSOLUTE | MOUSE_VIRTUAL_DESKTOP, 0x0008, 65535, 0));
        let w = mouse_record(&InputEvent::Wheel { delta: -120, horizontal: false, x: 1, y: 2 }).unwrap();
        assert_eq!((w.button_flags, w.button_data as i16), (MOUSE_WHEEL, -120));
        assert_eq!(button_flags(button::X2, true), 0x0100);
        assert_eq!(button_flags(button::X2, false), 0x0200);
        assert!(mouse_record(&InputEvent::ReleaseAll).is_none());
    }
}

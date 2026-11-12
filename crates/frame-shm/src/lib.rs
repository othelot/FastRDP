//! Consumer (and test producer) for the per-session frame shared memory written by FastRdpIdd.
//! Layout mirrors `drivers/FastRdpIdd/FastRdpShm.h` (keep both in sync; `layout_matches_c_header` test).

use std::ptr::NonNull;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering, fence};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Memory::{FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile};
use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject};

pub const MAGIC: u32 = 0x4D48_5346;
pub const VERSION: u32 = 3;
pub const SLOTS: usize = 3;
pub const MAX_WIDTH: u32 = 3840;
pub const MAX_HEIGHT: u32 = 2160;
pub const SLOT_BYTES: u64 = MAX_WIDTH as u64 * MAX_HEIGHT as u64 * 4;
pub const CURSOR_OFFSET: usize = 1024;
pub const CONTROL_OFFSET: usize = 2048;
pub const CURSOR_SHAPE_OFFSET: usize = 4096;
pub const CURSOR_MAX: u32 = 256;
pub const CURSOR_SHAPE_BYTES: usize = (CURSOR_MAX * CURSOR_MAX * 4) as usize;
pub const DATA_OFFSET: u64 = (CURSOR_SHAPE_OFFSET + CURSOR_SHAPE_BYTES) as u64;
/// IDDCX_CURSOR_SHAPE_TYPE_MASKED_COLOR: BGRA, A=0x00 opaque pixel, A=0xFF XOR with the screen.
pub const CURSOR_MASKED_COLOR: u32 = 1;
/// IDDCX_CURSOR_SHAPE_TYPE_ALPHA: BGRA with straight alpha.
pub const CURSOR_ALPHA: u32 = 2;
pub const TOTAL_BYTES: u64 = DATA_OFFSET + SLOTS as u64 * SLOT_BYTES;
pub const FORMAT_BGRA8: u32 = 87;

#[repr(C)]
pub struct Slot {
    pub seq: AtomicI64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
    pub pts_us: i64,
    pub frame_number: u64,
    pub dirty_rects: u32,
    pub reserved: [u32; 5],
}

#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub version: u32,
    pub header_size: u32,
    pub slot_count: u32,
    pub slot_bytes: u64,
    pub data_offset: u64,
    pub session_id: u32,
    pub producer_pid: u32,
    pub latest_frame: AtomicI64,
    pub latest_slot: AtomicI32,
    pub reserved0: u32,
    pub producer_heartbeat_us: AtomicI64,
    pub reserved: [u8; 256 - 64],
    pub slots: [Slot; SLOTS],
}

/// Hardware-cursor state (seqlock), at `CURSOR_OFFSET`.
#[repr(C)]
pub struct CursorBlock {
    pub seq: AtomicI64,
    pub visible: u32,
    pub x: i32,
    pub y: i32,
    pub shape_id: u32,
    pub shape_type: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub x_hot: u32,
    pub y_hot: u32,
    pub updates: u32,
    pub reserved: [u32; 3],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorState {
    pub visible: bool,
    /// top-left of the cursor image in desktop pixels
    pub x: i32,
    pub y: i32,
    pub shape_id: u32,
    pub updates: u32,
}

#[derive(Debug, Clone)]
pub struct CursorShape {
    pub id: u32,
    pub shape_type: u32,
    pub width: u32,
    pub height: u32,
    pub x_hot: u32,
    pub y_hot: u32,
    /// BGRA rows, `width * 4` bytes each
    pub bgra: Vec<u8>,
}

/// Display mode requested by the server (seqlock) and the driver's answer, at `CONTROL_OFFSET`.
#[repr(C)]
pub struct ControlBlock {
    pub seq: AtomicI64,
    pub request_id: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub scale_percent: u32,
    pub applied_id: std::sync::atomic::AtomicU32,
    pub applied_status: i32,
    pub applied_width: u32,
    pub applied_height: u32,
    pub applied_scale: u32,
    pub reserved: [u32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub scale_percent: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub request_id: u32,
    /// NTSTATUS of IddCxAdapterDisplayConfigUpdate
    pub status: i32,
    pub mode: DisplayMode,
}

pub fn control_event_name(session: u32, global: bool) -> String {
    format!("{}\\FastRDP_CtlEvt_S{session}", if global { "Global" } else { "Local" })
}

/// Same DACL as the driver: SYSTEM, LOCAL SERVICE (server + WUDFHost), Administrators.
const SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)";

/// Server side of the display control block. Creates the section if the driver has not yet (so a
/// request written now is the session's initial mode) or opens the driver's.
pub struct DisplayControl {
    section: HANDLE,
    event: HANDLE,
    view: NonNull<u8>,
    request_id: u32,
}

// SAFETY: as FrameShm.
unsafe impl Send for DisplayControl {}

impl DisplayControl {
    pub fn open_or_create(session: u32, global: bool) -> std::io::Result<Self> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};
        use windows_sys::Win32::System::Threading::CreateEventW;
        let n = wide(&section_name(session, global));
        let e = wide(&control_event_name(session, global));
        let sddl = wide(SDDL);
        // SAFETY: plain Win32 calls with checked results; the SD is freed after use.
        unsafe {
            let mut sd = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
            // Global\ (production) objects get the driver's DACL; Local\ ones (tests) default security.
            let sa: *const SECURITY_ATTRIBUTES = if global { &sa } else { std::ptr::null() };
            let section = CreateFileMappingW(-1isize as HANDLE, sa, PAGE_READWRITE, (TOTAL_BYTES >> 32) as u32, TOTAL_BYTES as u32, n.as_ptr());
            let section_err = std::io::Error::last_os_error();
            let event = CreateEventW(sa, 0, 0, e.as_ptr());
            let event_err = std::io::Error::last_os_error();
            LocalFree(sd);
            if section.is_null() {
                if !event.is_null() {
                    CloseHandle(event);
                }
                return Err(section_err);
            }
            if event.is_null() {
                CloseHandle(section);
                return Err(event_err);
            }
            let v = MapViewOfFile(section, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, (CONTROL_OFFSET + std::mem::size_of::<ControlBlock>()) as usize);
            if v.Value.is_null() {
                let err = std::io::Error::last_os_error();
                CloseHandle(section);
                CloseHandle(event);
                return Err(err);
            }
            let mut s = DisplayControl { section, event, view: NonNull::new_unchecked(v.Value as *mut u8), request_id: 0 };
            // continue the id sequence of an earlier server connection (the driver compares ids)
            s.request_id = std::ptr::read_volatile(&s.block().request_id);
            Ok(s)
        }
    }

    fn block(&self) -> &ControlBlock {
        // SAFETY: the view covers CONTROL_OFFSET + size_of::<ControlBlock>().
        unsafe { &*(self.view.as_ptr().add(CONTROL_OFFSET) as *const ControlBlock) }
    }

    /// Writes a new request and signals the driver. Returns its id.
    pub fn request(&mut self, m: DisplayMode) -> u32 {
        use windows_sys::Win32::System::Threading::SetEvent;
        self.request_id = self.request_id.wrapping_add(1).max(1);
        let c = self.view.as_ptr().wrapping_add(CONTROL_OFFSET) as *mut ControlBlock;
        // SAFETY: we are the only writer of the request fields (seqlock protocol of the header).
        unsafe {
            (*c).seq.fetch_add(1, Ordering::AcqRel);
            std::ptr::write_volatile(&mut (*c).width, m.width);
            std::ptr::write_volatile(&mut (*c).height, m.height);
            std::ptr::write_volatile(&mut (*c).refresh_hz, m.refresh_hz);
            std::ptr::write_volatile(&mut (*c).scale_percent, m.scale_percent);
            std::ptr::write_volatile(&mut (*c).request_id, self.request_id);
            (*c).seq.fetch_add(1, Ordering::AcqRel);
            SetEvent(self.event);
        }
        self.request_id
    }

    /// The driver's answer to the latest request it processed (None = nothing processed yet).
    pub fn applied(&self) -> Option<Applied> {
        let c = self.block();
        let id = c.applied_id.load(Ordering::Acquire);
        if id == 0 {
            return None;
        }
        // SAFETY: plain reads; the driver writes these before publishing applied_id.
        unsafe {
            Some(Applied {
                request_id: id,
                status: std::ptr::read_volatile(&c.applied_status),
                mode: DisplayMode {
                    width: std::ptr::read_volatile(&c.applied_width),
                    height: std::ptr::read_volatile(&c.applied_height),
                    refresh_hz: std::ptr::read_volatile(&c.refresh_hz),
                    scale_percent: std::ptr::read_volatile(&c.applied_scale),
                },
            })
        }
    }
}

impl Drop for DisplayControl {
    fn drop(&mut self) {
        // SAFETY: owned handles/view.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.as_ptr() as *mut _ });
            CloseHandle(self.section);
            CloseHandle(self.event);
        }
    }
}

pub fn section_name(session: u32, global: bool) -> String {
    format!("{}\\FastRDP_Frame_S{session}", if global { "Global" } else { "Local" })
}
pub fn event_name(session: u32, global: bool) -> String {
    format!("{}\\FastRDP_FrameEvt_S{session}", if global { "Global" } else { "Local" })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Microseconds from QueryPerformanceCounter (same clock as the driver's `QpcMicros`).
pub fn qpc_micros() -> i64 {
    use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut c, mut f) = (0i64, 0i64);
    // SAFETY: valid out pointers.
    unsafe {
        QueryPerformanceFrequency(&mut f);
        QueryPerformanceCounter(&mut c);
    }
    (c / f) * 1_000_000 + (c % f) * 1_000_000 / f
}

#[derive(Debug, Clone)]
pub struct FrameMeta {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pts_us: i64,
    pub frame_number: u64,
    pub dirty_rects: u32,
}

pub struct FrameShm {
    section: HANDLE,
    event: HANDLE,
    view: NonNull<u8>,
}

// SAFETY: the mapping is process-wide; all shared fields are accessed with atomics / seqlock.
unsafe impl Send for FrameShm {}

impl FrameShm {
    /// Opens the driver's section for `session` (consumer). `global=false`: `Local\` namespace (unit tests).
    pub fn open(session: u32, global: bool) -> std::io::Result<Self> {
        let n = wide(&section_name(session, global));
        let e = wide(&event_name(session, global));
        // SAFETY: plain Win32 calls; handles are checked and owned by the returned value.
        unsafe {
            let section = OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, 0, n.as_ptr());
            if section.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let event = OpenEventW(SYNCHRONIZATION_SYNCHRONIZE | EVENT_MODIFY_STATE, 0, e.as_ptr());
            if event.is_null() {
                let err = std::io::Error::last_os_error();
                CloseHandle(section);
                return Err(err);
            }
            let v = MapViewOfFile(section, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0);
            if v.Value.is_null() {
                let err = std::io::Error::last_os_error();
                CloseHandle(section);
                CloseHandle(event);
                return Err(err);
            }
            let s = FrameShm { section, event, view: NonNull::new_unchecked(v.Value as *mut u8) };
            let h = s.header();
            if h.magic != MAGIC || h.version != VERSION || h.header_size as usize != std::mem::size_of::<Header>() {
                return Err(std::io::Error::other(format!("bad shm header magic={:#x} version={} size={}", h.magic, h.version, h.header_size)));
            }
            Ok(s)
        }
    }

    pub fn header(&self) -> &Header {
        // SAFETY: the view is at least TOTAL_BYTES and starts with a Header.
        unsafe { &*(self.view.as_ptr() as *const Header) }
    }

    fn cursor_block(&self) -> &CursorBlock {
        // SAFETY: the view is at least TOTAL_BYTES; CursorBlock lies at CURSOR_OFFSET.
        unsafe { &*(self.view.as_ptr().add(CURSOR_OFFSET) as *const CursorBlock) }
    }

    /// Reads the hardware-cursor state; if the shape differs from `known_shape` it is copied too.
    /// None while the producer is writing (retry on the next wake-up) or before the first update.
    pub fn read_cursor(&self, known_shape: u32) -> Option<(CursorState, Option<CursorShape>)> {
        let c = self.cursor_block();
        for _ in 0..4 {
            let s1 = c.seq.load(Ordering::Acquire);
            if s1 == 0 {
                return None;
            }
            if s1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            // SAFETY: plain reads under the seqlock (validated below); shape bounded by CURSOR_MAX.
            let (state, shape) = unsafe {
                let state = CursorState {
                    visible: std::ptr::read_volatile(&c.visible) != 0,
                    x: std::ptr::read_volatile(&c.x),
                    y: std::ptr::read_volatile(&c.y),
                    shape_id: std::ptr::read_volatile(&c.shape_id),
                    updates: std::ptr::read_volatile(&c.updates),
                };
                let (w, h) = (std::ptr::read_volatile(&c.width), std::ptr::read_volatile(&c.height));
                let shape = if state.shape_id != known_shape && state.shape_id != 0 && w > 0 && w <= CURSOR_MAX && h > 0 && h <= CURSOR_MAX {
                    let n = (w * h * 4) as usize;
                    let mut bgra = vec![0u8; n];
                    std::ptr::copy_nonoverlapping(self.view.as_ptr().add(CURSOR_SHAPE_OFFSET), bgra.as_mut_ptr(), n);
                    Some(CursorShape {
                        id: state.shape_id,
                        shape_type: std::ptr::read_volatile(&c.shape_type),
                        width: w,
                        height: h,
                        x_hot: std::ptr::read_volatile(&c.x_hot),
                        y_hot: std::ptr::read_volatile(&c.y_hot),
                        bgra,
                    })
                } else {
                    None
                };
                (state, shape)
            };
            fence(Ordering::Acquire);
            if c.seq.load(Ordering::Acquire) == s1 {
                return Some((state, shape));
            }
        }
        None
    }

    pub fn latest_frame(&self) -> i64 {
        self.header().latest_frame.load(Ordering::Acquire)
    }

    /// Waits for the producer's "new frame" event (true = signalled).
    pub fn wait(&self, timeout_ms: u32) -> bool {
        // SAFETY: valid event handle.
        unsafe { WaitForSingleObject(self.event, timeout_ms) == WAIT_OBJECT_0 }
    }

    /// Copies the latest complete frame into `dst` (resized as needed). Returns None if no frame
    /// has been published yet or the producer kept overwriting the slot (retry later).
    pub fn read_latest(&self, dst: &mut Vec<u8>) -> Option<FrameMeta> {
        let h = self.header();
        for _ in 0..4 {
            if h.latest_frame.load(Ordering::Acquire) < 0 {
                return None;
            }
            let idx = h.latest_slot.load(Ordering::Acquire) as usize % SLOTS;
            let slot = &h.slots[idx];
            let s1 = slot.seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                continue;
            }
            // SAFETY: plain reads of slot metadata under the seqlock (validated by s2 below).
            let meta = unsafe {
                FrameMeta {
                    width: std::ptr::read_volatile(&slot.width),
                    height: std::ptr::read_volatile(&slot.height),
                    stride: std::ptr::read_volatile(&slot.stride),
                    pts_us: std::ptr::read_volatile(&slot.pts_us),
                    frame_number: std::ptr::read_volatile(&slot.frame_number),
                    dirty_rects: std::ptr::read_volatile(&slot.dirty_rects),
                }
            };
            if meta.width == 0 || meta.width > MAX_WIDTH || meta.height > MAX_HEIGHT || meta.stride < meta.width * 4 {
                continue;
            }
            let bytes = meta.stride as usize * meta.height as usize;
            dst.resize(bytes, 0);
            // SAFETY: slot data lies inside the mapping (bounds checked against MAX dimensions).
            unsafe {
                let src = self.view.as_ptr().add((h.data_offset + idx as u64 * h.slot_bytes) as usize);
                std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), bytes);
            }
            fence(Ordering::Acquire);
            if slot.seq.load(Ordering::Acquire) == s1 {
                return Some(meta);
            }
        }
        None
    }
}

impl Drop for FrameShm {
    fn drop(&mut self) {
        // SAFETY: owned handles/view.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.as_ptr() as *mut _ });
            CloseHandle(self.section);
            CloseHandle(self.event);
        }
    }
}

/// Producer with the same protocol as the driver (unit tests).
#[cfg(test)]
pub struct TestProducer {
    section: HANDLE,
    event: HANDLE,
    view: NonNull<u8>,
    frame: u64,
}

// SAFETY: as FrameShm.
#[cfg(test)]
unsafe impl Send for TestProducer {}

#[cfg(test)]
impl TestProducer {
    pub fn create(session: u32, global: bool) -> std::io::Result<Self> {
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};
        use windows_sys::Win32::System::Threading::CreateEventW;
        let n = wide(&section_name(session, global));
        let e = wide(&event_name(session, global));
        // SAFETY: plain Win32 calls with checked results.
        unsafe {
            let section = CreateFileMappingW(-1isize as HANDLE, std::ptr::null(), PAGE_READWRITE, (TOTAL_BYTES >> 32) as u32, TOTAL_BYTES as u32, n.as_ptr());
            if section.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let event = CreateEventW(std::ptr::null(), 0, 0, e.as_ptr());
            let v = MapViewOfFile(section, windows_sys::Win32::System::Memory::FILE_MAP_ALL_ACCESS, 0, 0, 0);
            if event.is_null() || v.Value.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let p = TestProducer { section, event, view: NonNull::new_unchecked(v.Value as *mut u8), frame: 0 };
            let h = &mut *(p.view.as_ptr() as *mut Header);
            h.version = VERSION;
            h.header_size = std::mem::size_of::<Header>() as u32;
            h.slot_count = SLOTS as u32;
            h.slot_bytes = SLOT_BYTES;
            h.data_offset = DATA_OFFSET;
            h.session_id = session;
            h.latest_frame.store(-1, Ordering::Release);
            fence(Ordering::Release);
            std::ptr::write_volatile(&mut h.magic, MAGIC);
            Ok(p)
        }
    }

    pub fn publish(&mut self, width: u32, height: u32, bgra: &[u8], pts_us: i64) {
        use windows_sys::Win32::System::Threading::SetEvent;
        // SAFETY: we own the producer side; layout checked; data bounded by MAX dims.
        unsafe {
            let h = &*(self.view.as_ptr() as *const Header);
            let idx = (h.latest_slot.load(Ordering::Acquire) as usize + 1) % SLOTS;
            let slot = &h.slots[idx] as *const Slot as *mut Slot;
            (*slot).seq.fetch_add(1, Ordering::AcqRel);
            (*slot).width = width;
            (*slot).height = height;
            (*slot).stride = width * 4;
            (*slot).format = FORMAT_BGRA8;
            (*slot).pts_us = pts_us;
            self.frame += 1;
            (*slot).frame_number = self.frame;
            let dst = self.view.as_ptr().add((DATA_OFFSET + idx as u64 * SLOT_BYTES) as usize);
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), dst, (width * height * 4) as usize);
            (*slot).seq.fetch_add(1, Ordering::AcqRel);
            h.latest_frame.store(self.frame as i64, Ordering::Release);
            h.latest_slot.store(idx as i32, Ordering::Release);
            SetEvent(self.event);
        }
    }
}

#[cfg(test)]
impl TestProducer {
    /// Same protocol as `FramePublisher::PublishCursor` in the driver.
    pub fn publish_cursor(&mut self, visible: bool, x: i32, y: i32, shape: Option<&CursorShape>) {
        use windows_sys::Win32::System::Threading::SetEvent;
        // SAFETY: producer side of our own mapping; shape bounded by CURSOR_MAX.
        unsafe {
            let c = self.view.as_ptr().add(CURSOR_OFFSET) as *mut CursorBlock;
            (*c).seq.fetch_add(1, Ordering::AcqRel);
            (*c).visible = visible as u32;
            (*c).x = x;
            (*c).y = y;
            if let Some(s) = shape {
                std::ptr::copy_nonoverlapping(s.bgra.as_ptr(), self.view.as_ptr().add(CURSOR_SHAPE_OFFSET), (s.width * s.height * 4) as usize);
                (*c).shape_id = s.id;
                (*c).shape_type = s.shape_type;
                (*c).width = s.width;
                (*c).height = s.height;
                (*c).pitch = s.width * 4;
                (*c).x_hot = s.x_hot;
                (*c).y_hot = s.y_hot;
            }
            (*c).updates += 1;
            (*c).seq.fetch_add(1, Ordering::AcqRel);
            SetEvent(self.event);
        }
    }
}

#[cfg(test)]
impl Drop for TestProducer {
    fn drop(&mut self) {
        // SAFETY: owned handles/view.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.as_ptr() as *mut _ });
            CloseHandle(self.section);
            CloseHandle(self.event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_c_header() {
        assert_eq!(std::mem::size_of::<Slot>(), 64);
        assert_eq!(std::mem::size_of::<Header>(), 256 + 3 * 64);
        assert_eq!(std::mem::offset_of!(Header, latest_frame), 40);
        assert_eq!(std::mem::offset_of!(Header, latest_slot), 48);
        assert_eq!(std::mem::offset_of!(Header, producer_heartbeat_us), 56);
        assert_eq!(std::mem::offset_of!(Header, slots), 256);
        assert_eq!(std::mem::size_of::<CursorBlock>(), 64);
        assert_eq!(std::mem::size_of::<ControlBlock>(), 64);
        const { assert!(CURSOR_OFFSET + 64 <= CONTROL_OFFSET && CONTROL_OFFSET + 64 <= CURSOR_SHAPE_OFFSET) };
        const { assert!(std::mem::size_of::<Header>() <= CURSOR_OFFSET) };
        assert_eq!(DATA_OFFSET, 266_240);
        assert_eq!(DATA_OFFSET % 4096, 0);
    }

    #[test]
    fn display_control_before_and_after_the_producer() {
        let session = 970_000 + std::process::id();
        // server first: creates the section, writes the initial mode
        let mut ctl = DisplayControl::open_or_create(session, false).unwrap();
        assert!(ctl.applied().is_none());
        let m = DisplayMode { width: 2560, height: 1440, refresh_hz: 60, scale_percent: 150 };
        assert_eq!(ctl.request(m), 1);
        // the producer (driver) opens the same section and sees the request
        let p = TestProducer::create(session, false).unwrap();
        // SAFETY: test-only peek at the producer's mapping.
        let blk = unsafe { &*(p.view.as_ptr().add(CONTROL_OFFSET) as *const ControlBlock) };
        assert_eq!((blk.request_id, blk.width, blk.height, blk.scale_percent), (1, 2560, 1440, 150));
        // driver answer
        let blk_mut = blk as *const ControlBlock as *mut ControlBlock;
        unsafe {
            (*blk_mut).applied_status = 0;
            (*blk_mut).applied_width = 2560;
            (*blk_mut).applied_height = 1440;
            (*blk_mut).applied_scale = 150;
        }
        blk.applied_id.store(1, Ordering::Release);
        let a = ctl.applied().unwrap();
        assert_eq!((a.request_id, a.status, a.mode.width, a.mode.scale_percent), (1, 0, 2560, 150));
        // a second server handle continues the id sequence
        drop(ctl);
        let mut ctl2 = DisplayControl::open_or_create(session, false).unwrap();
        assert_eq!(ctl2.request(m), 2);
    }

    #[test]
    fn cursor_roundtrip() {
        let session = 950_000 + std::process::id();
        let mut p = TestProducer::create(session, false).unwrap();
        let c = FrameShm::open(session, false).unwrap();
        assert!(c.read_cursor(0).is_none());
        let shape = CursorShape { id: 7, shape_type: CURSOR_ALPHA, width: 2, height: 2, x_hot: 1, y_hot: 0, bgra: (0..16).collect() };
        p.publish_cursor(true, -3, 40, Some(&shape));
        let (st, sh) = c.read_cursor(0).unwrap();
        assert_eq!((st.visible, st.x, st.y, st.shape_id), (true, -3, 40, 7));
        let sh = sh.unwrap();
        assert_eq!((sh.width, sh.height, sh.x_hot, sh.shape_type), (2, 2, 1, CURSOR_ALPHA));
        assert_eq!(sh.bgra, shape.bgra);
        p.publish_cursor(false, 10, 11, None);
        let (st, sh) = c.read_cursor(7).unwrap();
        assert_eq!((st.visible, st.x, st.shape_id), (false, 10, 7));
        assert!(sh.is_none());
    }

    #[test]
    fn produce_consume_roundtrip() {
        let session = 900_000 + std::process::id();
        let mut p = TestProducer::create(session, false).unwrap();
        let c = FrameShm::open(session, false).unwrap();
        let mut buf = Vec::new();
        assert!(c.read_latest(&mut buf).is_none());
        let px: Vec<u8> = (0..64 * 32 * 4).map(|i| i as u8).collect();
        p.publish(64, 32, &px, 123);
        assert!(c.wait(1000));
        let m = c.read_latest(&mut buf).unwrap();
        assert_eq!((m.width, m.height, m.pts_us, m.frame_number), (64, 32, 123, 1));
        assert_eq!(buf, px);
        for i in 0..5 {
            p.publish(64, 32, &px, 200 + i);
        }
        assert_eq!(c.read_latest(&mut buf).unwrap().frame_number, 6);
    }
}

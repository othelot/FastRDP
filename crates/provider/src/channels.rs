//! Virtual channels (`FastRdpVirtualChannels=1`): Windows session components (rdpclip "CLIPRDR",
//! audio "AUDIO_PLAYBACK_DVC", device redirection "RDPDR", ...) ask the protocol for a channel in
//! `IWRdsProtocolConnection::CreateVirtualChannel` and get a handle they read/write like a file.
//!
//! We create a message-mode pipe pair (one session write = one message), hand out the client end
//! and relay messages to the fastrdp-server: `ChannelOpen` / `ChannelData` (fragments) /
//! `ChannelClose`. Data from the client is written back as one message per channel message;
//! every message gets a CHANNEL_PDU_HEADER in front (what WTSVirtualChannelRead returns).
//! Content is never logged.
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use protocol::{ChannelData, ChannelOpen, Message};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, ERROR_MORE_DATA, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT};
use windows_sys::Win32::System::Threading::CreateEventW;

use crate::{logi, logw};

static NEXT_PIPE: AtomicU32 = AtomicU32::new(1);

/// CHANNEL_PDU_HEADER flags
const CHANNEL_FLAG_FIRST: u32 = 0x01;
const CHANNEL_FLAG_LAST: u32 = 0x02;
/// RDP static channel chunk size (CHANNEL_CHUNK_LENGTH)
const CHANNEL_CHUNK_LENGTH: usize = 1600;

/// Sends a message to the server (the connection's transport; false = gone).
pub type Sender = Arc<dyn Fn(&Message) -> bool + Send + Sync>;

pub struct Channel {
    pub id: u32,
    pub name: String,
    pub is_static: bool,
    server: HANDLE,
    write_lock: Mutex<()>,
    closed: AtomicBool,
    writes: AtomicU32,
}

// SAFETY: kernel handle; overlapped I/O from several threads with per-call OVERLAPPED/events.
unsafe impl Send for Channel {}
unsafe impl Sync for Channel {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

struct Ev(HANDLE);
impl Drop for Ev {
    fn drop(&mut self) {
        // SAFETY: owned event.
        unsafe { CloseHandle(self.0) };
    }
}

/// One overlapped ReadFile/WriteFile, waited for. Returns (bytes, more_data).
fn overlapped(h: HANDLE, buf: *mut u8, len: u32, write: bool) -> std::io::Result<(u32, bool)> {
    // SAFETY: the OVERLAPPED and buffer live until GetOverlappedResult(wait=TRUE) returned.
    unsafe {
        let ev = Ev(CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()));
        let mut ov: OVERLAPPED = std::mem::zeroed();
        ov.hEvent = ev.0;
        let mut n = 0u32;
        let ok = if write { WriteFile(h, buf, len, &mut n, &mut ov) } else { ReadFile(h, buf, len, &mut n, &mut ov) };
        if ok == 0 {
            match GetLastError() {
                ERROR_IO_PENDING => {}
                ERROR_MORE_DATA => return Ok((len, true)),
                e => return Err(std::io::Error::from_raw_os_error(e as i32)),
            }
        }
        if GetOverlappedResult(h, &ov, &mut n, 1) == 0 {
            return match GetLastError() {
                ERROR_MORE_DATA => Ok((n, true)),
                e => Err(std::io::Error::from_raw_os_error(e as i32)),
            };
        }
        Ok((n, false))
    }
}

impl Channel {
    /// Writes one channel message from the client to the session component.
    pub fn write(&self, data: &[u8]) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        // Readers (WTSVirtualChannelRead) get the data like from RDP: chunks of at most
        // CHANNEL_CHUNK_LENGTH bytes, each behind a CHANNEL_PDU_HEADER (total length + FIRST/LAST).
        let _g = self.write_lock.lock().unwrap();
        if self.writes.fetch_add(1, Ordering::Relaxed) < 3 {
            logi!("channel '{}' #{}: client message {} bytes", self.name, self.id, data.len());
        }
        let n = data.len().div_ceil(CHANNEL_CHUNK_LENGTH).max(1);
        let mut msg = Vec::with_capacity(CHANNEL_CHUNK_LENGTH + 8);
        for i in 0..n {
            let chunk = &data[(i * CHANNEL_CHUNK_LENGTH).min(data.len())..((i + 1) * CHANNEL_CHUNK_LENGTH).min(data.len())];
            let flags = if i == 0 { CHANNEL_FLAG_FIRST } else { 0 } | if i + 1 == n { CHANNEL_FLAG_LAST } else { 0 };
            msg.clear();
            msg.extend_from_slice(&(data.len() as u32).to_le_bytes());
            msg.extend_from_slice(&flags.to_le_bytes());
            msg.extend_from_slice(chunk);
            match overlapped(self.server, msg.as_ptr() as *mut u8, msg.len() as u32, true) {
                Ok((w, _)) if w as usize == msg.len() => {}
                Ok((w, _)) => {
                    logw!("channel '{}' #{}: short write {w}/{}", self.name, self.id, msg.len());
                    return false;
                }
                Err(e) => {
                    logw!("channel '{}' #{}: write failed: {e}", self.name, self.id);
                    return false;
                }
            }
        }
        true
    }

    /// Ends the channel (cancels the pending read; the reader thread reports ChannelClose).
    pub fn shutdown(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            // SAFETY: valid handle; cancels I/O of all threads on it.
            unsafe { CancelIoEx(self.server, std::ptr::null()) };
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        // SAFETY: owned handle.
        unsafe { CloseHandle(self.server) };
    }
}

/// Creates the channel and its reader thread; returns (channel, handle for TermService).
pub fn create(conn: u32, id: u32, name: &str, is_static: bool, send: Sender) -> Result<(Arc<Channel>, HANDLE), String> {
    let pipe = wide(&format!(r"\\.\pipe\FastRDP.VC.{}.{conn}.{}", std::process::id(), NEXT_PIPE.fetch_add(1, Ordering::Relaxed)));
    // SAFETY: plain pipe creation; both ends owned until handed out / dropped. Default security:
    // the (unpublished) name is opened right here by the same process, max 1 instance.
    let (server, client) = unsafe {
        let server = CreateNamedPipeW(
            pipe.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            256 * 1024,
            256 * 1024,
            0,
            std::ptr::null(),
        );
        if server == INVALID_HANDLE_VALUE {
            return Err(format!("CreateNamedPipe: {}", std::io::Error::last_os_error()));
        }
        let client = CreateFileW(pipe.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_OVERLAPPED, std::ptr::null_mut());
        if client == INVALID_HANDLE_VALUE {
            let e = std::io::Error::last_os_error();
            CloseHandle(server);
            return Err(format!("open client end: {e}"));
        }
        (server, client)
    };
    let ch = Arc::new(Channel {
        id,
        name: name.to_string(),
        is_static,
        server,
        write_lock: Mutex::new(()),
        closed: AtomicBool::new(false),
        writes: AtomicU32::new(0),
    });
    send(&Message::ChannelOpen(ChannelOpen { id, name: name.to_string(), is_static }));
    let reader = ch.clone();
    let tag = format!("Connection#{conn} channel '{name}' #{id} ({})", if is_static { "static" } else { "dynamic" });
    let spawned = std::thread::Builder::new().name("fastrdp-vc".into()).spawn(move || {
        let mut buf = vec![0u8; 256 * 1024];
        let (mut messages, mut total) = (0u64, 0u64);
        let mut msg: Vec<u8> = Vec::new();
        while let Ok((n, more)) = overlapped(reader.server, buf.as_mut_ptr(), buf.len() as u32, false) {
            msg.extend_from_slice(&buf[..n as usize]);
            if more || msg.is_empty() {
                continue;
            }
            messages += 1;
            total += msg.len() as u64;
            for f in ChannelData::fragments(reader.id, &msg) {
                if !send(&Message::ChannelData(f)) {
                    break;
                }
            }
            msg.clear();
        }
        reader.closed.store(true, Ordering::Release);
        send(&Message::ChannelClose(reader.id));
        logi!("{tag}: closed after {messages} message(s), {total} bytes from the session");
    });
    if let Err(e) = spawned {
        // SAFETY: owned client end not handed out.
        unsafe { CloseHandle(client) };
        return Err(format!("reader thread: {e}"));
    }
    logi!("Connection#{conn} channel '{name}' #{id}: created");
    Ok((ch, client))
}

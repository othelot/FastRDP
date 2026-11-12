//! Byte-stream transport for the provider's control protocol: `\\.\pipe\FastRDP.Provider.<Listener>`,
//! local only (PIPE_REJECT_REMOTE_CLIENTS), DACL = SYSTEM + LOCAL SERVICE (fastrdp-server) +
//! Administrators. TermService exposes no network port.
//! Pipe handles are overlapped so one thread can block in a read while another writes events.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

pub trait Transport: Read + Write + Send {
    fn clone_box(&self) -> io::Result<Box<dyn Transport>>;
    fn shutdown(&self);
    fn set_read_timeout(&mut self, d: Option<Duration>);
    fn set_write_timeout(&mut self, d: Option<Duration>);
}

struct OwnedHandle(HANDLE);
// SAFETY: kernel handles are usable from any thread.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; overlapped ops use per-call OVERLAPPED/event.
unsafe impl Sync for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: owned handle.
        unsafe { CloseHandle(self.0) };
    }
}

pub struct PipeStream {
    h: Arc<OwnedHandle>,
    closed: Arc<AtomicBool>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    pub client_pid: u32,
}

fn new_event() -> io::Result<OwnedHandle> {
    // SAFETY: plain manual-reset event.
    let e = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if e.is_null() { Err(io::Error::last_os_error()) } else { Ok(OwnedHandle(e)) }
}

impl PipeStream {
    fn io(&self, buf_ptr: *mut u8, len: usize, write: bool, timeout: Option<Duration>) -> io::Result<usize> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let ev = new_event()?;
        // SAFETY: OVERLAPPED zero-init is valid; buffer outlives the operation because we always wait
        // for completion (or cancel + wait) before returning.
        unsafe {
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = ev.0;
            let mut n = 0u32;
            let len32 = len.min(u32::MAX as usize) as u32;
            let ok = if write { WriteFile(self.h.0, buf_ptr, len32, &mut n, &mut ov) } else { ReadFile(self.h.0, buf_ptr, len32, &mut n, &mut ov) };
            if ok == 0 {
                let err = GetLastError();
                if err != ERROR_IO_PENDING {
                    return if err == ERROR_BROKEN_PIPE { Ok(0) } else { Err(io::Error::from_raw_os_error(err as i32)) };
                }
                let ms = timeout.map(|d| d.as_millis().min(u32::MAX as u128 - 1) as u32).unwrap_or(u32::MAX);
                let w = WaitForSingleObject(ev.0, ms);
                if w == WAIT_TIMEOUT {
                    CancelIoEx(self.h.0, &ov);
                    GetOverlappedResult(self.h.0, &ov, &mut n, 1);
                    return Err(io::ErrorKind::TimedOut.into());
                }
                if w != WAIT_OBJECT_0 || GetOverlappedResult(self.h.0, &ov, &mut n, 0) == 0 {
                    let err = GetLastError();
                    return if err == ERROR_BROKEN_PIPE { Ok(0) } else { Err(io::Error::from_raw_os_error(err as i32)) };
                }
            }
            Ok(n as usize)
        }
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io(buf.as_mut_ptr(), buf.len(), false, self.read_timeout)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io(buf.as_ptr() as *mut u8, buf.len(), true, self.write_timeout)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Transport for PipeStream {
    fn clone_box(&self) -> io::Result<Box<dyn Transport>> {
        Ok(Box::new(PipeStream {
            h: self.h.clone(),
            closed: self.closed.clone(),
            read_timeout: self.read_timeout,
            write_timeout: self.write_timeout,
            client_pid: self.client_pid,
        }))
    }
    fn shutdown(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            // SAFETY: valid handle; cancels pending I/O of all threads, then drops the client.
            unsafe {
                CancelIoEx(self.h.0, std::ptr::null());
                DisconnectNamedPipe(self.h.0);
            }
        }
    }
    fn set_read_timeout(&mut self, d: Option<Duration>) {
        self.read_timeout = d;
    }
    fn set_write_timeout(&mut self, d: Option<Duration>) {
        self.write_timeout = d;
    }
}

/// `NT SERVICE\TermService` - the provider's own process identity.
pub const TERMSERVICE_SID: &str = "S-1-5-80-446051430-1559341753-4161941529-1950928533-810483104";

/// Default pipe DACL: SYSTEM, LOCAL SERVICE (fastrdp-server), Administrators may connect.
/// TermService itself (the creator, which hosts this DLL) needs access too: creating an additional
/// instance is checked for FILE_CREATE_PIPE_INSTANCE *and* the duplex read/write access, otherwise
/// every further instance fails with ACCESS_DENIED while one connection is open (one client at a time).
pub fn pipe_sddl() -> String {
    format!("D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;GA;;;{TERMSERVICE_SID})")
}

pub struct PipeListener {
    name: Vec<u16>,
    sddl: Vec<u16>,
    first: bool,
}

impl PipeListener {
    pub fn new(name: &str, sddl: &str) -> Self {
        PipeListener { name: name.encode_utf16().chain(Some(0)).collect(), sddl: sddl.encode_utf16().chain(Some(0)).collect(), first: true }
    }

    fn create_instance(&mut self) -> io::Result<OwnedHandle> {
        // SAFETY: SDDL -> SD conversion, pipe creation; SD freed after use.
        unsafe {
            let mut sd = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(self.sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
                return Err(io::Error::last_os_error());
            }
            let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
            let open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | if self.first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
            let h = CreateNamedPipeW(
                self.name.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                &sa,
            );
            LocalFree(sd);
            if h == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            self.first = false;
            Ok(OwnedHandle(h))
        }
    }

    /// Waits for one client. Returns Ok(None) if `stop` became true.
    pub fn accept(&mut self, stop: &AtomicBool) -> io::Result<Option<PipeStream>> {
        let h = self.create_instance()?;
        let ev = new_event()?;
        // SAFETY: overlapped ConnectNamedPipe; we wait or cancel before `ov` goes out of scope.
        unsafe {
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = ev.0;
            if ConnectNamedPipe(h.0, &mut ov) == 0 {
                match GetLastError() {
                    ERROR_PIPE_CONNECTED => {}
                    ERROR_IO_PENDING => loop {
                        if stop.load(Ordering::SeqCst) {
                            CancelIoEx(h.0, &ov);
                            let mut n = 0;
                            GetOverlappedResult(h.0, &ov, &mut n, 1);
                            return Ok(None);
                        }
                        let w = WaitForSingleObject(ev.0, 200);
                        if w == WAIT_OBJECT_0 {
                            let mut n = 0;
                            if GetOverlappedResult(h.0, &ov, &mut n, 0) == 0 {
                                return Err(io::Error::last_os_error());
                            }
                            break;
                        }
                    },
                    e => return Err(io::Error::from_raw_os_error(e as i32)),
                }
            }
            let mut pid = 0u32;
            GetNamedPipeClientProcessId(h.0, &mut pid);
            Ok(Some(PipeStream { h: Arc::new(h), closed: Arc::new(AtomicBool::new(false)), read_timeout: None, write_timeout: None, client_pid: pid }))
        }
    }
}

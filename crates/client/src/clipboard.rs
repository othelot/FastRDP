//! Local clipboard access on its own thread: reports local changes, applies remote content and
//! never reports content it set itself (no echo). System clipboard via arboard (Windows / macOS /
//! Linux); an in-memory clipboard for unit tests.
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crate::cliprdr::Content;

/// In-memory clipboard for unit tests: (change counter, content).
#[cfg(test)]
pub type MemoryClipboard = std::sync::Arc<std::sync::Mutex<(u64, Option<Content>)>>;

#[derive(Clone)]
pub enum Backend {
    System,
    #[cfg(test)]
    Memory(MemoryClipboard),
}

const POLL: Duration = Duration::from_millis(400);

trait Access {
    /// Cheap change counter if the platform has one (None = always read).
    fn sequence(&mut self) -> Option<u64>;
    fn read(&mut self) -> Option<Content>;
    fn write(&mut self, c: &Content);
}

#[cfg(test)]
struct Memory(MemoryClipboard);
#[cfg(test)]
impl Access for Memory {
    fn sequence(&mut self) -> Option<u64> {
        Some(self.0.lock().unwrap().0)
    }
    fn read(&mut self) -> Option<Content> {
        self.0.lock().unwrap().1.clone()
    }
    fn write(&mut self, c: &Content) {
        let mut g = self.0.lock().unwrap();
        g.0 += 1;
        g.1 = Some(c.clone());
    }
}

struct System(Option<arboard::Clipboard>);
impl System {
    fn cb(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.0.is_none() {
            self.0 = arboard::Clipboard::new().ok();
        }
        self.0.as_mut()
    }
}
impl Access for System {
    fn sequence(&mut self) -> Option<u64> {
        #[cfg(windows)]
        {
            // SAFETY: trivial query.
            Some(unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() } as u64)
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
    fn read(&mut self) -> Option<Content> {
        let cb = self.cb()?;
        if let Ok(files) = cb.get().file_list()
            && !files.is_empty()
        {
            return Some(Content::Files(files));
        }
        if let Ok(t) = cb.get_text()
            && !t.is_empty()
        {
            return Some(Content::Text(t));
        }
        let img = cb.get_image().ok()?;
        Some(Content::Image { width: img.width as u32, height: img.height as u32, rgba: img.bytes.into_owned() })
    }
    fn write(&mut self, c: &Content) {
        let Some(cb) = self.cb() else { return };
        let r = match c {
            Content::Text(t) => cb.set_text(t.clone()),
            Content::Image { width, height, rgba } => {
                cb.set_image(arboard::ImageData { width: *width as usize, height: *height as usize, bytes: std::borrow::Cow::Borrowed(rgba) })
            }
            Content::Files(f) => cb.set().file_list(f),
        };
        if let Err(e) = r {
            eprintln!("clipboard: {e}");
            self.0 = None; // re-open next time
        }
    }
}

/// Starts the clipboard thread. Returns the sender for remote content; local changes arrive on
/// `changed`. The thread ends when the returned sender is dropped.
pub fn spawn(backend: Backend, changed: tokio::sync::mpsc::UnboundedSender<Content>) -> mpsc::Sender<Content> {
    let (tx, rx) = mpsc::channel::<Content>();
    let _ = std::thread::Builder::new().name("fastrdp-clipboard".into()).spawn(move || {
        let mut access: Box<dyn Access> = match backend {
            Backend::System => Box::new(System(None)),
            #[cfg(test)]
            Backend::Memory(m) => Box::new(Memory(m)),
        };
        let mut last_seq: Option<u64> = None;
        let mut last_seen: Option<u64> = None;
        let mut ours: Option<u64> = None;
        loop {
            match rx.recv_timeout(POLL) {
                Ok(c) => {
                    access.write(&c);
                    ours = Some(c.fingerprint());
                    last_seen = ours;
                    last_seq = access.sequence();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let seq = access.sequence();
            if seq.is_some() && seq == last_seq {
                continue;
            }
            last_seq = seq;
            let Some(c) = access.read() else { continue };
            let fp = c.fingerprint();
            if Some(fp) == last_seen {
                continue;
            }
            last_seen = Some(fp);
            if Some(fp) != ours && changed.send(c).is_err() {
                break;
            }
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_local_changes_but_not_its_own_writes() {
        let mem: MemoryClipboard = Default::default();
        let (ctx, mut crx) = tokio::sync::mpsc::unbounded_channel();
        let set = spawn(Backend::Memory(mem.clone()), ctx);
        {
            let mut g = mem.lock().unwrap();
            *g = (1, Some(Content::Text("local".into())));
        }
        std::thread::sleep(POLL * 2);
        assert_eq!(crx.try_recv().ok(), Some(Content::Text("local".into())));
        set.send(Content::Text("from remote".into())).unwrap();
        std::thread::sleep(POLL * 2);
        assert!(crx.try_recv().is_err(), "own write is not reported");
        assert_eq!(mem.lock().unwrap().1, Some(Content::Text("from remote".into())));
        drop(set);
    }
}

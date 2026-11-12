//! Clipboard redirection, client side of MS-RDPECLIP over the "CLIPRDR" static virtual channel
//! (the session side is Windows' own rdpclip.exe). Pure protocol logic plus file I/O for file
//! transfers: PDUs in, PDUs / clipboard updates out.
//!
//! - text: CF_UNICODETEXT
//! - images: CF_DIB
//! - files and folders: "FileGroupDescriptorW" + FILECONTENTS requests. Files copied in the session
//!   are downloaded into a temporary folder and then put on the local clipboard; files copied
//!   locally are read from disk on demand while the session pastes them.
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const CHANNEL: &str = "CLIPRDR";

// msgType
const CB_MONITOR_READY: u16 = 0x0001;
const CB_FORMAT_LIST: u16 = 0x0002;
const CB_FORMAT_LIST_RESPONSE: u16 = 0x0003;
const CB_FORMAT_DATA_REQUEST: u16 = 0x0004;
const CB_FORMAT_DATA_RESPONSE: u16 = 0x0005;
const CB_CLIP_CAPS: u16 = 0x0007;
const CB_FILECONTENTS_REQUEST: u16 = 0x0008;
const CB_FILECONTENTS_RESPONSE: u16 = 0x0009;
// msgFlags
const CB_RESPONSE_OK: u16 = 0x0001;
const CB_RESPONSE_FAIL: u16 = 0x0002;
const CB_ASCII_NAMES: u16 = 0x0004;
// general capability flags
const CB_USE_LONG_FORMAT_NAMES: u32 = 0x0002;
const CB_STREAM_FILECLIP_ENABLED: u32 = 0x0004;
const CB_FILECLIP_NO_FILE_PATHS: u32 = 0x0008;
const CB_HUGE_FILE_SUPPORT_ENABLED: u32 = 0x0020;
// FILECONTENTS_REQUEST dwFlags
const FILECONTENTS_SIZE: u32 = 0x1;
const FILECONTENTS_RANGE: u32 = 0x2;
// FILEDESCRIPTORW
const FD_ATTRIBUTES: u32 = 0x0004;
const FD_WRITESTIME: u32 = 0x0020;
const FD_FILESIZE: u32 = 0x0040;
const FD_SHOWPROGRESSUI: u32 = 0x4000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILEDESCRIPTOR_SIZE: usize = 592;
// clipboard formats
pub const CF_DIB: u32 = 8;
pub const CF_UNICODETEXT: u32 = 13;
pub const CF_DIBV5: u32 = 17;
pub const FILE_GROUP_DESCRIPTOR_W: &str = "FileGroupDescriptorW";
/// id under which we register "FileGroupDescriptorW" in our format lists
const OUR_FILE_LIST_ID: u32 = 0xC0F0;
/// bytes per FILECONTENTS range request when downloading
const DOWNLOAD_CHUNK: u32 = 2 * 1024 * 1024;
/// maximum bytes we return for one range request
const MAX_RANGE: u32 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Text(String),
    /// straight RGBA, top-down
    Image {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// files / folders (top level items)
    Files(Vec<PathBuf>),
}

impl Content {
    pub fn fingerprint(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }

    pub fn describe(&self) -> String {
        match self {
            Content::Text(t) => format!("text, {} characters", t.chars().count()),
            Content::Image { width, height, .. } => format!("image {width}x{height}"),
            Content::Files(f) => format!("{} file(s)/folder(s)", f.len()),
        }
    }
}

impl Hash for Content {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match self {
            Content::Text(t) => (0u8, t).hash(h),
            Content::Image { width, height, rgba } => (1u8, width, height, rgba).hash(h),
            Content::Files(f) => (2u8, f).hash(h),
        }
    }
}

pub enum Action {
    /// PDU for the session side
    Send(Vec<u8>),
    /// remote clipboard content to put on the local clipboard
    SetLocal(Content),
    /// progress / problems for the user
    Info(String),
}

fn pdu(msg_type: u16, flags: u16, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + data.len());
    v.extend_from_slice(&msg_type.to_le_bytes());
    v.extend_from_slice(&flags.to_le_bytes());
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(data);
    v
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn i32_at(b: &[u8], o: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// One entry of a file list we offer.
#[derive(Clone, Debug)]
struct LocalEntry {
    /// relative name with '\' separators
    name: String,
    path: PathBuf,
    size: u64,
    dir: bool,
    modified: Option<std::time::SystemTime>,
}

fn expand(paths: &[PathBuf]) -> Vec<LocalEntry> {
    fn walk(base: &str, p: &Path, out: &mut Vec<LocalEntry>) {
        if out.len() >= MAX_ENTRIES {
            return;
        }
        let Ok(md) = std::fs::symlink_metadata(p) else { return };
        if md.file_type().is_symlink() {
            return; // no link loops, no surprises
        }
        out.push(LocalEntry {
            name: base.to_string(),
            path: p.to_path_buf(),
            size: if md.is_dir() { 0 } else { md.len() },
            dir: md.is_dir(),
            modified: md.modified().ok(),
        });
        if md.is_dir() {
            let Ok(rd) = std::fs::read_dir(p) else { return };
            let mut children: Vec<_> = rd.flatten().collect();
            children.sort_by_key(|e| e.file_name());
            for e in children {
                walk(&format!("{base}\\{}", e.file_name().to_string_lossy()), &e.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    for p in paths {
        if let Some(n) = p.file_name() {
            walk(&n.to_string_lossy(), p, &mut out);
        }
    }
    out
}

fn filetime(t: Option<std::time::SystemTime>) -> u64 {
    // 100 ns intervals since 1601-01-01
    t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() * 10_000_000 + d.subsec_nanos() as u64 / 100 + 116_444_736_000_000_000)
        .unwrap_or(0)
}

fn file_list_data(entries: &[LocalEntry]) -> Vec<u8> {
    let mut d = Vec::with_capacity(4 + entries.len() * FILEDESCRIPTOR_SIZE);
    d.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        let mut fd = vec![0u8; FILEDESCRIPTOR_SIZE];
        fd[0..4].copy_from_slice(&(FD_ATTRIBUTES | FD_FILESIZE | FD_WRITESTIME | FD_SHOWPROGRESSUI).to_le_bytes());
        fd[36..40].copy_from_slice(&(if e.dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL }).to_le_bytes());
        fd[56..64].copy_from_slice(&filetime(e.modified).to_le_bytes());
        fd[64..68].copy_from_slice(&((e.size >> 32) as u32).to_le_bytes());
        fd[68..72].copy_from_slice(&(e.size as u32).to_le_bytes());
        for (i, c) in e.name.encode_utf16().take(259).enumerate() {
            fd[72 + i * 2..74 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        d.extend_from_slice(&fd);
    }
    d
}

/// A file or folder announced by the session.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteEntry {
    rel: PathBuf,
    size: Option<u64>,
    dir: bool,
}

/// Relative, safe local path for a remote name (no absolute paths, drive letters or "..").
fn safe_relative(name: &str) -> Option<PathBuf> {
    let mut p = PathBuf::new();
    for part in name.split(['\\', '/']) {
        if part.is_empty() || part == "." || part == ".." || part.contains(':') || part.chars().any(|c| (c as u32) < 32) {
            return None;
        }
        p.push(part);
    }
    (!p.as_os_str().is_empty()).then_some(p)
}

fn parse_file_list(d: &[u8]) -> Option<Vec<RemoteEntry>> {
    let n = u32_at(d, 0)? as usize;
    if n > MAX_ENTRIES {
        return None;
    }
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let fd = d.get(4 + i * FILEDESCRIPTOR_SIZE..4 + (i + 1) * FILEDESCRIPTOR_SIZE)?;
        let flags = u32_at(fd, 0)?;
        let attrs = u32_at(fd, 36)?;
        let size = (flags & FD_FILESIZE != 0).then(|| (u32_at(fd, 64).unwrap_or(0) as u64) << 32 | u32_at(fd, 68).unwrap_or(0) as u64);
        let name: Vec<u16> = fd[72..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        let rel = safe_relative(&String::from_utf16_lossy(&name))?;
        v.push(RemoteEntry { rel, size, dir: flags & FD_ATTRIBUTES != 0 && attrs & FILE_ATTRIBUTE_DIRECTORY != 0 });
    }
    Some(v)
}

/// Session files being copied to a local folder.
struct Download {
    dir: PathBuf,
    items: Vec<RemoteEntry>,
    index: usize,
    offset: u64,
    size: Option<u64>,
    file: Option<std::fs::File>,
    stream: u32,
    bytes: u64,
}

pub struct Cliprdr {
    long_names: bool,
    huge_files: bool,
    ready: bool,
    /// format we asked the session for (one request at a time)
    requested: Option<u32>,
    /// id of "FileGroupDescriptorW" in the session's current format list
    remote_file_list: Option<u32>,
    /// what we last offered (answers CB_FORMAT_DATA_REQUEST)
    local: Option<Content>,
    /// expanded file list of `local` (Files)
    local_entries: Vec<LocalEntry>,
    download: Option<Download>,
    next_stream: u32,
    /// where session files are downloaded (a new subfolder per copy)
    pub download_root: PathBuf,
}

impl Default for Cliprdr {
    fn default() -> Self {
        Cliprdr {
            long_names: false,
            huge_files: false,
            ready: false,
            requested: None,
            remote_file_list: None,
            local: None,
            local_entries: Vec::new(),
            download: None,
            next_stream: 1,
            download_root: std::env::temp_dir().join("FastRDP-clipboard"),
        }
    }
}

impl Cliprdr {
    /// New channel instance (reconnect, rdpclip restarted): keep what we offer locally.
    pub fn reset(&mut self) {
        let (local, entries, root) = (self.local.take(), std::mem::take(&mut self.local_entries), std::mem::take(&mut self.download_root));
        *self = Cliprdr { local, local_entries: entries, download_root: root, ..Default::default() };
    }

    /// Our capabilities + the current local clipboard (after CB_MONITOR_READY).
    fn start(&mut self) -> Vec<Action> {
        let mut caps = Vec::new();
        caps.extend_from_slice(&1u16.to_le_bytes()); // cCapabilitiesSets
        caps.extend_from_slice(&0u16.to_le_bytes()); // pad
        caps.extend_from_slice(&1u16.to_le_bytes()); // CB_CAPSTYPE_GENERAL
        caps.extend_from_slice(&12u16.to_le_bytes()); // lengthCapability
        caps.extend_from_slice(&2u32.to_le_bytes()); // CB_CAPS_VERSION_2
        caps.extend_from_slice(
            &(CB_USE_LONG_FORMAT_NAMES | CB_STREAM_FILECLIP_ENABLED | CB_FILECLIP_NO_FILE_PATHS | CB_HUGE_FILE_SUPPORT_ENABLED).to_le_bytes(),
        );
        let mut out = vec![Action::Send(pdu(CB_CLIP_CAPS, 0, &caps))];
        self.ready = true;
        out.push(Action::Send(self.format_list()));
        out
    }

    fn format_list(&self) -> Vec<u8> {
        let entry: Option<(u32, &str)> = match &self.local {
            Some(Content::Text(_)) => Some((CF_UNICODETEXT, "")),
            Some(Content::Image { .. }) => Some((CF_DIB, "")),
            Some(Content::Files(_)) if !self.local_entries.is_empty() => Some((OUR_FILE_LIST_ID, FILE_GROUP_DESCRIPTOR_W)),
            _ => None,
        };
        let mut d = Vec::new();
        if let Some((id, name)) = entry {
            d.extend_from_slice(&id.to_le_bytes());
            if self.long_names {
                d.extend(name.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes));
            } else {
                let mut n = [0u8; 32];
                n[..name.len().min(31)].copy_from_slice(&name.as_bytes()[..name.len().min(31)]);
                d.extend_from_slice(&n);
            }
        }
        pdu(CB_FORMAT_LIST, if self.long_names { 0 } else { CB_ASCII_NAMES }, &d)
    }

    /// (format id, name) in a CB_FORMAT_LIST body.
    fn parse_format_list(&self, flags: u16, d: &[u8]) -> Vec<(u32, String)> {
        let mut out = Vec::new();
        let mut o = 0;
        if self.long_names {
            while let Some(id) = u32_at(d, o) {
                o += 4;
                let mut name = Vec::new();
                while let Some(c) = u16_at(d, o) {
                    o += 2;
                    if c == 0 {
                        break;
                    }
                    name.push(c);
                }
                out.push((id, String::from_utf16_lossy(&name)));
            }
        } else {
            while let Some(id) = u32_at(d, o) {
                let raw = d.get(o + 4..o + 36).unwrap_or(&[]);
                let name = if flags & CB_ASCII_NAMES != 0 {
                    String::from_utf8_lossy(raw.split(|&b| b == 0).next().unwrap_or(&[])).into_owned()
                } else {
                    String::from_utf16_lossy(&raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect::<Vec<_>>())
                };
                out.push((id, name));
                o += 36;
            }
        }
        out
    }

    /// One PDU from the session side.
    pub fn on_pdu(&mut self, p: &[u8]) -> Vec<Action> {
        let (Some(ty), Some(flags), Some(len)) = (u16_at(p, 0), u16_at(p, 2), u32_at(p, 4)) else { return vec![] };
        let d = &p[8..p.len().min(8 + len as usize)];
        match ty {
            CB_CLIP_CAPS => {
                // general capability set: flags at offset 4 (sets) + 8 (type, len, version)
                let server_flags = u32_at(d, 12).unwrap_or(0);
                self.long_names = server_flags & CB_USE_LONG_FORMAT_NAMES != 0;
                self.huge_files = server_flags & CB_HUGE_FILE_SUPPORT_ENABLED != 0;
                vec![]
            }
            CB_MONITOR_READY => self.start(),
            CB_FORMAT_LIST => {
                let formats = self.parse_format_list(flags, d);
                let mut out = vec![Action::Send(pdu(CB_FORMAT_LIST_RESPONSE, CB_RESPONSE_OK, &[]))];
                let has = |f: u32| formats.iter().any(|(id, _)| *id == f);
                self.remote_file_list = formats.iter().find(|(_, n)| n.eq_ignore_ascii_case(FILE_GROUP_DESCRIPTOR_W)).map(|(id, _)| *id);
                let want = if let Some(id) = self.remote_file_list {
                    Some(id)
                } else if has(CF_UNICODETEXT) {
                    Some(CF_UNICODETEXT)
                } else if has(CF_DIB) || has(CF_DIBV5) {
                    Some(CF_DIB)
                } else {
                    None
                };
                if let Some(f) = want {
                    self.requested = Some(f);
                    out.push(Action::Send(pdu(CB_FORMAT_DATA_REQUEST, 0, &f.to_le_bytes())));
                }
                out
            }
            CB_FORMAT_DATA_REQUEST => {
                let f = u32_at(d, 0).unwrap_or(0);
                let data = match (&self.local, f) {
                    (Some(Content::Text(t)), CF_UNICODETEXT) => Some(t.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect::<Vec<u8>>()),
                    (Some(Content::Image { width, height, rgba }), CF_DIB) => Some(dib_from_rgba(*width, *height, rgba)),
                    (Some(Content::Files(_)), OUR_FILE_LIST_ID) => Some(file_list_data(&self.local_entries)),
                    _ => None,
                };
                vec![Action::Send(match data {
                    Some(d) => pdu(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_OK, &d),
                    None => pdu(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_FAIL, &[]),
                })]
            }
            CB_FORMAT_DATA_RESPONSE => {
                let Some(f) = self.requested.take() else { return vec![] };
                if flags & CB_RESPONSE_OK == 0 {
                    return vec![];
                }
                if Some(f) == self.remote_file_list {
                    return self.start_download(d);
                }
                let c = match f {
                    CF_UNICODETEXT => {
                        let u: Vec<u16> = d.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
                        Some(Content::Text(String::from_utf16_lossy(&u)))
                    }
                    CF_DIB => rgba_from_dib(d).map(|(width, height, rgba)| Content::Image { width, height, rgba }),
                    _ => None,
                };
                match c {
                    Some(c) => self.set_local(c),
                    None => vec![],
                }
            }
            CB_FILECONTENTS_REQUEST => self.file_contents_request(d),
            CB_FILECONTENTS_RESPONSE => self.file_contents_response(flags, d),
            _ => vec![], // format list response, lock / unlock
        }
    }

    fn set_local(&mut self, c: Content) -> Vec<Action> {
        // what is on the local clipboard now came from the session: do not offer it back
        self.local = Some(c.clone());
        self.local_entries.clear();
        vec![Action::SetLocal(c)]
    }

    /// The session asks for the size or a byte range of one of our files.
    fn file_contents_request(&mut self, d: &[u8]) -> Vec<Action> {
        let (Some(stream), Some(index), Some(flags), Some(lo), Some(hi), Some(want)) =
            (u32_at(d, 0), u32_at(d, 4), u32_at(d, 8), u32_at(d, 12), u32_at(d, 16), u32_at(d, 20))
        else {
            return vec![];
        };
        let fail = || vec![Action::Send(pdu(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_FAIL, &stream.to_le_bytes()))];
        let Some(e) = self.local_entries.get(index as usize) else { return fail() };
        let mut out = stream.to_le_bytes().to_vec();
        if flags & FILECONTENTS_SIZE != 0 {
            out.extend_from_slice(&e.size.to_le_bytes());
        } else if flags & FILECONTENTS_RANGE != 0 && !e.dir {
            let pos = (hi as u64) << 32 | lo as u64;
            let want = want.min(MAX_RANGE) as usize;
            let read = (|| -> std::io::Result<Vec<u8>> {
                let mut f = std::fs::File::open(&e.path)?;
                f.seek(SeekFrom::Start(pos))?;
                let mut buf = Vec::with_capacity(want);
                f.take(want as u64).read_to_end(&mut buf)?;
                Ok(buf)
            })();
            match read {
                Ok(b) => out.extend_from_slice(&b),
                Err(_) => return fail(),
            }
        } else {
            return fail();
        }
        vec![Action::Send(pdu(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, &out))]
    }

    fn start_download(&mut self, list: &[u8]) -> Vec<Action> {
        let Some(items) = parse_file_list(list) else {
            return vec![Action::Info("session file list rejected (invalid names or too many entries)".into())];
        };
        // keep only the most recent downloads
        if let Ok(rd) = std::fs::read_dir(&self.download_root) {
            let mut old: Vec<_> = rd.flatten().filter(|e| e.path().is_dir()).collect();
            old.sort_by_key(|e| e.file_name());
            let excess = old.len().saturating_sub(4);
            for e in old.into_iter().take(excess) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let dir = self.download_root.join(format!("{stamp}"));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return vec![Action::Info(format!("cannot create {}: {e}", dir.display()))];
        }
        let total: u64 = items.iter().filter_map(|i| i.size).sum();
        let mut out = vec![Action::Info(format!("copying {} item(s), {:.1} MB from the session", items.len(), total as f64 / 1e6))];
        self.download = Some(Download { dir, items, index: 0, offset: 0, size: None, file: None, stream: 0, bytes: 0 });
        out.extend(self.download_next());
        out
    }

    /// Advances to the next file (creating folders / empty files on the way) and requests data.
    fn download_next(&mut self) -> Vec<Action> {
        loop {
            let stream = self.next_stream;
            let Some(dl) = self.download.as_mut() else { return vec![] };
            if dl.index >= dl.items.len() {
                let dl = self.download.take().unwrap();
                let mut tops: Vec<PathBuf> = Vec::new();
                for i in &dl.items {
                    let top = dl.dir.join(i.rel.components().next().unwrap().as_os_str());
                    if !tops.contains(&top) {
                        tops.push(top);
                    }
                }
                let mut out =
                    vec![Action::Info(format!("{} item(s), {:.1} MB copied from the session to {}", dl.items.len(), dl.bytes as f64 / 1e6, dl.dir.display()))];
                out.extend(self.set_local(Content::Files(tops)));
                return out;
            }
            let item = dl.items[dl.index].clone();
            let path = dl.dir.join(&item.rel);
            if item.dir {
                let _ = std::fs::create_dir_all(&path);
                dl.index += 1;
                continue;
            }
            if let Some(p) = path.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            match std::fs::File::create(&path) {
                Ok(f) => dl.file = Some(f),
                Err(e) => {
                    let msg = format!("cannot create {}: {e}", path.display());
                    self.download = None;
                    return vec![Action::Info(msg)];
                }
            }
            dl.offset = 0;
            dl.size = item.size;
            if dl.size == Some(0) {
                dl.file = None;
                dl.index += 1;
                continue;
            }
            self.next_stream += 1;
            dl.stream = stream;
            return vec![Action::Send(self.range_request(stream))];
        }
    }

    fn range_request(&self, stream: u32) -> Vec<u8> {
        let dl = self.download.as_ref().unwrap();
        let mut d = Vec::with_capacity(24);
        d.extend_from_slice(&stream.to_le_bytes());
        d.extend_from_slice(&(dl.index as u32).to_le_bytes());
        match dl.size {
            None => {
                d.extend_from_slice(&FILECONTENTS_SIZE.to_le_bytes());
                d.extend_from_slice(&[0; 8]);
                d.extend_from_slice(&8u32.to_le_bytes());
            }
            Some(size) => {
                d.extend_from_slice(&FILECONTENTS_RANGE.to_le_bytes());
                d.extend_from_slice(&(dl.offset as u32).to_le_bytes());
                d.extend_from_slice(&((dl.offset >> 32) as u32).to_le_bytes());
                d.extend_from_slice(&((size - dl.offset).min(DOWNLOAD_CHUNK as u64) as u32).to_le_bytes());
            }
        }
        pdu(CB_FILECONTENTS_REQUEST, 0, &d)
    }

    fn file_contents_response(&mut self, flags: u16, d: &[u8]) -> Vec<Action> {
        let Some(dl) = self.download.as_mut() else { return vec![] };
        if u32_at(d, 0) != Some(dl.stream) {
            return vec![];
        }
        if flags & CB_RESPONSE_OK == 0 {
            let name = dl.items[dl.index].rel.display().to_string();
            self.download = None;
            return vec![Action::Info(format!("the session refused to send {name}"))];
        }
        let body = &d[4..];
        if dl.size.is_none() {
            let size = body.get(..8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
            dl.size = Some(size);
            if size == 0 {
                dl.file = None;
                dl.index += 1;
                return self.download_next();
            }
            let stream = dl.stream;
            return vec![Action::Send(self.range_request(stream))];
        }
        let size = dl.size.unwrap();
        let write = dl.file.as_mut().map(|f| f.write_all(body));
        if let Some(Err(e)) = write {
            self.download = None;
            return vec![Action::Info(format!("download failed: {e}"))];
        }
        dl.offset += body.len() as u64;
        dl.bytes += body.len() as u64;
        if body.is_empty() || dl.offset >= size {
            dl.file = None;
            dl.index += 1;
            return self.download_next();
        }
        let stream = dl.stream;
        vec![Action::Send(self.range_request(stream))]
    }

    /// The local clipboard changed (not by us): offer it to the session.
    pub fn local_changed(&mut self, c: Content) -> Vec<Action> {
        if self.local.as_ref() == Some(&c) {
            return vec![];
        }
        self.local_entries = match &c {
            Content::Files(f) => expand(f),
            _ => Vec::new(),
        };
        self.local = Some(c);
        if !self.ready {
            return vec![];
        }
        vec![Action::Send(self.format_list())]
    }
}

/// Top-down RGBA -> CF_DIB (BITMAPINFOHEADER, 32 bpp BI_RGB, bottom-up BGRA).
pub fn dib_from_rgba(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut d = Vec::with_capacity(40 + rgba.len());
    d.extend_from_slice(&40u32.to_le_bytes());
    d.extend_from_slice(&(w as i32).to_le_bytes());
    d.extend_from_slice(&(h as i32).to_le_bytes());
    d.extend_from_slice(&1u16.to_le_bytes());
    d.extend_from_slice(&32u16.to_le_bytes());
    d.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    d.extend_from_slice(&(w * h * 4).to_le_bytes());
    d.extend_from_slice(&[0u8; 16]); // ppm x/y, clr used/important
    for y in (0..h as usize).rev() {
        for p in rgba[y * w as usize * 4..][..w as usize * 4].chunks_exact(4) {
            d.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    d
}

/// CF_DIB (24/32 bpp, BI_RGB or BI_BITFIELDS) -> top-down RGBA.
pub fn rgba_from_dib(d: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let hsize = u32_at(d, 0)? as usize;
    let w = i32_at(d, 4)?;
    let h = i32_at(d, 8)?;
    let bpp = u16_at(d, 14)?;
    let compression = u32_at(d, 16)?;
    let clr_used = u32_at(d, 32).unwrap_or(0) as usize;
    if w <= 0 || h == 0 || w > 16384 || h.unsigned_abs() > 16384 || !(bpp == 32 || bpp == 24) || !(compression == 0 || compression == 3) {
        return None;
    }
    let (w, top_down, ah) = (w as usize, h < 0, h.unsigned_abs() as usize);
    let mut off = hsize + clr_used * 4;
    if compression == 3 && hsize == 40 {
        off += 12; // colour masks
    }
    let stride = (w * bpp as usize / 8).div_ceil(4) * 4;
    let bits = d.get(off..off + stride * ah)?;
    let mut rgba = Vec::with_capacity(w * ah * 4);
    let mut any_alpha = false;
    for y in 0..ah {
        let row = &bits[(if top_down { y } else { ah - 1 - y }) * stride..];
        for x in 0..w {
            let p = &row[x * bpp as usize / 8..];
            let a = if bpp == 32 { p[3] } else { 255 };
            any_alpha |= a != 0;
            rgba.extend_from_slice(&[p[2], p[1], p[0], a]);
        }
    }
    if !any_alpha {
        // 32 bpp BI_RGB with an unused (zero) alpha channel: opaque
        rgba.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    }
    Some((w as u32, ah as u32, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sends(a: &[Action]) -> Vec<&Vec<u8>> {
        a.iter().filter_map(|a| if let Action::Send(p) = a { Some(p) } else { None }).collect()
    }

    fn server_caps(long: bool) -> Vec<u8> {
        let mut c = vec![1, 0, 0, 0, 1, 0, 12, 0, 2, 0, 0, 0];
        c.extend_from_slice(&(if long { CB_USE_LONG_FORMAT_NAMES } else { 0 }).to_le_bytes());
        pdu(CB_CLIP_CAPS, 0, &c)
    }

    fn utf16z(s: &str) -> Vec<u8> {
        s.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn startup_and_text_both_ways() {
        let mut c = Cliprdr::default();
        assert!(c.on_pdu(&server_caps(true)).is_empty());
        assert!(c.local_changed(Content::Text("before ready".into())).is_empty(), "nothing sent before monitor ready");
        let a = c.on_pdu(&pdu(CB_MONITOR_READY, 0, &[]));
        let s = sends(&a);
        assert_eq!(u16_at(s[0], 0), Some(CB_CLIP_CAPS));
        assert_eq!(u16_at(s[1], 0), Some(CB_FORMAT_LIST));
        assert_eq!(u32_at(s[1], 8), Some(CF_UNICODETEXT), "initial format list offers the local text");

        let a = c.on_pdu(&pdu(CB_FORMAT_DATA_REQUEST, 0, &CF_UNICODETEXT.to_le_bytes()));
        let r = sends(&a)[0];
        assert_eq!((u16_at(r, 0), u16_at(r, 2)), (Some(CB_FORMAT_DATA_RESPONSE), Some(CB_RESPONSE_OK)));
        assert_eq!(&r[8..], utf16z("before ready").as_slice());

        let mut fl = Vec::new();
        fl.extend_from_slice(&0xC0FFu32.to_le_bytes());
        fl.extend(utf16z("HTML Format"));
        fl.extend_from_slice(&CF_UNICODETEXT.to_le_bytes());
        fl.extend_from_slice(&[0, 0]);
        let a = c.on_pdu(&pdu(CB_FORMAT_LIST, 0, &fl));
        let s = sends(&a);
        assert_eq!(u16_at(s[0], 0), Some(CB_FORMAT_LIST_RESPONSE));
        assert_eq!((u16_at(s[1], 0), u32_at(s[1], 8)), (Some(CB_FORMAT_DATA_REQUEST), Some(CF_UNICODETEXT)));
        let a = c.on_pdu(&pdu(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_OK, &utf16z("원격 텍스트")));
        assert!(matches!(&a[0], Action::SetLocal(Content::Text(t)) if t == "원격 텍스트"));
        assert!(c.local_changed(Content::Text("원격 텍스트".into())).is_empty(), "no echo");
        assert_eq!(sends(&c.local_changed(Content::Text("new".into()))).len(), 1);
    }

    #[test]
    fn short_format_names() {
        let mut c = Cliprdr::default();
        c.on_pdu(&server_caps(false));
        c.local = Some(Content::Text("x".into()));
        let a = c.on_pdu(&pdu(CB_MONITOR_READY, 0, &[]));
        let fl = sends(&a)[1];
        assert_eq!((u16_at(fl, 2), u32_at(fl, 4)), (Some(CB_ASCII_NAMES), Some(36)));
        let mut list = CF_DIB.to_le_bytes().to_vec();
        list.extend_from_slice(&[0u8; 32]);
        let a = c.on_pdu(&pdu(CB_FORMAT_LIST, CB_ASCII_NAMES, &list));
        assert_eq!(u32_at(sends(&a)[1], 8), Some(CF_DIB));
    }

    #[test]
    fn dib_roundtrip() {
        let rgba: Vec<u8> = (0..3 * 2 * 4).map(|i| i as u8 * 7).collect();
        let dib = dib_from_rgba(3, 2, &rgba);
        assert_eq!(rgba_from_dib(&dib), Some((3, 2, rgba)));
        let mut d24 = dib_from_rgba(1, 2, &[0; 8])[..40].to_vec();
        d24[14] = 24;
        d24.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 0]);
        let (_, _, px) = rgba_from_dib(&d24).unwrap();
        assert_eq!(px, vec![255, 255, 255, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn unsafe_remote_names_are_rejected() {
        assert_eq!(safe_relative("dir\\a.txt"), Some(PathBuf::from("dir").join("a.txt")));
        for bad in ["..\\x", "C:\\x", "\\\\server\\x", "", "a\\..\\b", "a\u{1}b"] {
            assert!(safe_relative(bad).is_none(), "{bad:?}");
        }
    }

    /// Files offered by one instance are downloaded by another: PDUs passed back and forth exactly
    /// like through rdpclip (one plays the session side).
    #[test]
    fn file_transfer_between_two_instances() {
        let base = std::env::temp_dir().join(format!("fastrdp-cliprdr-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        std::fs::create_dir_all(src.join("folder").join("sub")).unwrap();
        let big: Vec<u8> = (0..(DOWNLOAD_CHUNK as usize * 2 + 1234)).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(src.join("big.bin"), &big).unwrap();
        std::fs::write(src.join("folder").join("a.txt"), b"hi there").unwrap();
        std::fs::write(src.join("folder").join("sub").join("empty"), b"").unwrap();

        let mut offer = Cliprdr { long_names: true, ready: true, ..Default::default() };
        let a = offer.local_changed(Content::Files(vec![src.join("big.bin"), src.join("folder")]));
        let format_list = sends(&a)[0].clone();

        let mut take = Cliprdr { download_root: base.join("dst"), long_names: true, ..Default::default() };
        let mut queue: std::collections::VecDeque<(bool, Vec<u8>)> = [(true, format_list)].into(); // (to `take`, pdu)
        let mut result = None;
        let mut steps = 0;
        while let Some((to_take, p)) = queue.pop_front() {
            steps += 1;
            assert!(steps < 1000);
            let actions = if to_take { take.on_pdu(&p) } else { offer.on_pdu(&p) };
            for a in actions {
                match a {
                    Action::Send(x) => queue.push_back((!to_take, x)),
                    Action::SetLocal(c) => result = Some(c),
                    Action::Info(_) => {}
                }
            }
        }
        let Some(Content::Files(tops)) = result else { panic!("no files delivered") };
        assert_eq!(tops.len(), 2);
        let dir = tops[0].parent().unwrap().to_path_buf();
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), big);
        assert_eq!(std::fs::read(dir.join("folder").join("a.txt")).unwrap(), b"hi there");
        assert_eq!(std::fs::read(dir.join("folder").join("sub").join("empty")).unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&base);
    }
}

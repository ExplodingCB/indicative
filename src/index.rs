//! File/folder name index.
//!
//! Memory layout is deliberately flat: one byte arena of WTF-8 names and
//! one 12-byte record per entry pointing at its parent directory, so 60k
//! entries cost roughly 1.5 MB. A background thread (background IO/memory
//! priority) rescans only after the kernel reports a change *and* the
//! panel is opened, so an idle machine does zero indexing work.

use crate::util::{wide, wtf8_decode, wtf8_push};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Registry::HKEY_CURRENT_USER;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

pub const DIR_BIT: u16 = 0x8000;
const ROOT_BIT: u32 = 0x8000_0000;
const MAX_DEPTH: u8 = 10;

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct Entry {
    name_off: u32,
    parent: u32,   // entry index, or ROOT_BIT | root index
    name_len: u16,
    meta: u16,     // DIR_BIT | days since 2000-01-01 of last write
    mask: u32,     // char_mask() of the name, for O(1) rejection
}

/// Which character classes occur in a (WTF-8) string: one bit per ASCII
/// letter (case-folded), plus digit, '.', other ASCII and non-ASCII bits.
/// A name can only match if it contains every class the query contains.
#[inline]
pub fn char_mask(s: &[u8]) -> u32 {
    let mut m = 0u32;
    for &b in s {
        m |= match b {
            b'a'..=b'z' => 1 << (b - b'a'),
            b'A'..=b'Z' => 1 << (b - b'A'),
            b'0'..=b'9' => 1 << 26,
            b'.' => 1 << 27,
            b' ' => 0, // multi-word queries match words independently
            0..=0x7F => 1 << 28,
            _ => 1 << 29,
        };
    }
    m
}

pub struct Root {
    pub path: Vec<u16>,
    pub label: Vec<u16>,
}

pub struct FileIndex {
    pub names: Vec<u8>,
    pub entries: Vec<Entry>,
    pub roots: Vec<Root>,
    /// Most common extensions (for the icon cache helper).
    pub top_exts: Vec<String>,
}

impl FileIndex {
    #[inline]
    pub fn name(&self, i: usize) -> &[u8] {
        let e = &self.entries[i];
        &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
    }
    #[inline]
    pub fn mask(&self, i: usize) -> u32 {
        self.entries[i].mask
    }
    #[inline]
    pub fn is_dir(&self, i: usize) -> bool {
        self.entries[i].meta & DIR_BIT != 0
    }
    #[inline]
    pub fn days(&self, i: usize) -> u16 {
        self.entries[i].meta & !DIR_BIT
    }

    fn chain(&self, i: usize) -> (usize, Vec<usize>) {
        let mut chain = vec![i];
        let mut p = self.entries[i].parent;
        while p & ROOT_BIT == 0 {
            chain.push(p as usize);
            p = self.entries[p as usize].parent;
        }
        ((p & !ROOT_BIT) as usize, chain)
    }

    /// Number of folders between the root and this entry (0 = in the root).
    pub fn depth(&self, i: usize) -> u32 {
        self.chain(i).1.len() as u32 - 1
    }

    pub fn full_path(&self, i: usize) -> Vec<u16> {
        let (root, chain) = self.chain(i);
        let mut out = self.roots[root].path.clone();
        for &c in chain.iter().rev() {
            out.push(b'\\' as u16);
            wtf8_decode(self.name(c), &mut out);
        }
        out
    }

    /// "Documents › Taxes › 2025" - where the entry lives, for display.
    pub fn location(&self, i: usize) -> Vec<u16> {
        let (root, chain) = self.chain(i);
        let mut out = self.roots[root].label.clone();
        for &c in chain.iter().skip(1).rev() {
            out.extend_from_slice(&[b' ' as u16, 0x203A, b' ' as u16]);
            wtf8_decode(self.name(c), &mut out);
        }
        out
    }

    /// Lowercase extension of a file entry, without the dot.
    pub fn ext(&self, i: usize) -> &[u8] {
        if self.is_dir(i) {
            return &[];
        }
        let n = self.name(i);
        match n.iter().rposition(|&b| b == b'.') {
            Some(p) if p > 0 && n.len() - p <= 16 => &n[p + 1..],
            _ => &[],
        }
    }
}

fn skip_dir(name: &[u16]) -> bool {
    const SKIP: &[&str] = &[
        "node_modules", "__pycache__", "target", "site-packages", "bower_components",
        "$recycle.bin", "appdata", "venv", "obj",
    ];
    if name.len() > 20 {
        return false;
    }
    let s: String = String::from_utf16_lossy(name).to_ascii_lowercase();
    SKIP.contains(&s.as_str())
}

fn filetime_days(ft: &FILETIME) -> u16 {
    let t = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    // 100ns ticks since 1601 -> days since 2000-01-01 (145731 days apart)
    ((t / 864_000_000_000).saturating_sub(145_731)).min(0x7FFF) as u16
}

pub fn scan(roots: Vec<Root>, max: usize) -> FileIndex {
    let mut names = Vec::with_capacity(1 << 20);
    let mut entries: Vec<Entry> = Vec::with_capacity(max.min(1 << 16));
    let mut ext_count: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut queue: VecDeque<(u32, Vec<u16>, u8)> = VecDeque::new();
    for (ri, r) in roots.iter().enumerate() {
        queue.push_back((ROOT_BIT | ri as u32, r.path.clone(), 0));
    }
    let mut fd: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };

    'outer: while let Some((parent, dir, depth)) = queue.pop_front() {
        let mut pat = dir.clone();
        pat.extend_from_slice(&[b'\\' as u16, b'*' as u16, 0]);
        let h = unsafe {
            FindFirstFileExW(pat.as_ptr(), FindExInfoBasic, &mut fd as *mut _ as *mut _, FindExSearchNameMatch, std::ptr::null(), FIND_FIRST_EX_LARGE_FETCH)
        };
        if h == INVALID_HANDLE_VALUE {
            continue;
        }
        loop {
            let n = fd.cFileName.iter().position(|&c| c == 0).unwrap_or(260);
            let name = &fd.cFileName[..n];
            let attr = fd.dwFileAttributes;
            let dot = name.first() == Some(&(b'.' as u16));
            let office_tmp = name.starts_with(&[b'~' as u16, b'$' as u16]);
            let hidden = attr & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0;
            let is_dir = attr & FILE_ATTRIBUTE_DIRECTORY != 0;
            if !dot && !office_tmp && !hidden && n > 0 && !(is_dir && skip_dir(name)) {
                let off = names.len();
                wtf8_push(&mut names, name);
                let len = (names.len() - off).min(u16::MAX as usize);
                let idx = entries.len() as u32;
                entries.push(Entry {
                    name_off: off as u32,
                    parent,
                    name_len: len as u16,
                    meta: filetime_days(&fd.ftLastWriteTime) | if is_dir { DIR_BIT } else { 0 },
                    mask: char_mask(&names[off..off + len]),
                });
                if is_dir {
                    if depth < MAX_DEPTH && attr & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                        let mut sub = dir.clone();
                        sub.push(b'\\' as u16);
                        sub.extend_from_slice(name);
                        queue.push_back((idx, sub, depth + 1));
                    }
                } else {
                    let nm = &names[off..];
                    if let Some(p) = nm.iter().rposition(|&b| b == b'.') {
                        let e = &nm[p + 1..];
                        if p > 0 && !e.is_empty() && e.len() <= 10 && e.iter().all(|b| b.is_ascii_alphanumeric()) {
                            *ext_count.entry(e.to_ascii_lowercase()).or_insert(0) += 1;
                        }
                    }
                }
                if entries.len() >= max {
                    unsafe { FindClose(h) };
                    break 'outer;
                }
            }
            if unsafe { FindNextFileW(h, &mut fd) } == 0 {
                break;
            }
        }
        unsafe { FindClose(h) };
    }

    names.shrink_to_fit();
    entries.shrink_to_fit();
    let mut exts: Vec<_> = ext_count.into_iter().collect();
    exts.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    let top_exts = exts.into_iter().take(160).map(|(e, _)| String::from_utf8_lossy(&e).into_owned()).collect();
    FileIndex { names, entries, roots, top_exts }
}

/// Default roots: the user's shell folders plus `extra_roots` from config.
pub fn default_roots(extra: &[PathBuf], user_folders: bool) -> Vec<Root> {
    const SHELL: &[(&str, &str)] = &[
        ("Desktop", "Desktop"),
        ("Personal", "Documents"),
        ("{374DE290-123F-4565-9164-39C4925E467B}", "Downloads"),
        ("My Pictures", "Pictures"),
        ("My Music", "Music"),
        ("My Video", "Videos"),
    ];
    let mut roots: Vec<Root> = Vec::new();
    let key = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\User Shell Folders";
    let mut add = |path: Vec<u16>, label: Vec<u16>| {
        let mut path = path;
        while path.last() == Some(&(b'\\' as u16)) {
            path.pop();
        }
        let p = PathBuf::from(String::from_utf16_lossy(&path));
        if path.is_empty() || !p.is_dir() {
            return;
        }
        let lower = |v: &[u16]| String::from_utf16_lossy(v).to_lowercase();
        let lp = lower(&path);
        // skip duplicates and folders already covered by another root
        if roots.iter().any(|r| {
            let lr = lower(&r.path);
            lp == lr || lp.starts_with(&(lr.clone() + "\\"))
        }) {
            return;
        }
        roots.retain(|r| !lower(&r.path).starts_with(&(lp.clone() + "\\")));
        roots.push(Root { path, label });
    };
    for (val, label) in SHELL.iter().filter(|_| user_folders) {
        if let Some(p) = crate::util::reg_string(HKEY_CURRENT_USER, key, val) {
            add(p, label.encode_utf16().collect());
        }
    }
    for e in extra {
        let path: Vec<u16> = e.as_os_str().to_string_lossy().encode_utf16().collect();
        let label = e.file_name().map(|n| n.to_string_lossy().encode_utf16().collect()).unwrap_or_else(|| path.clone());
        add(path, label);
    }
    roots
}

pub fn start_menu_dirs() -> Vec<Vec<u16>> {
    let mut v = Vec::new();
    for var in ["APPDATA", "ProgramData"] {
        if let Some(base) = std::env::var_os(var) {
            let p = PathBuf::from(base).join("Microsoft\\Windows\\Start Menu\\Programs");
            if p.is_dir() {
                v.push(p.to_string_lossy().encode_utf16().collect());
            }
        }
    }
    v
}

// ---------------------------------------------------------------------------

pub struct Indexer {
    pub current: Mutex<Option<Arc<FileIndex>>>,
    pub dirty: AtomicBool,
    pub apps_dirty: AtomicBool,
    pub scanning: AtomicBool,
    rescan_evt: isize,
}

impl Indexer {
    /// Ask the worker to rescan if anything changed since the last scan.
    pub fn poke(&self) {
        if self.dirty.load(Ordering::Relaxed) && !self.scanning.load(Ordering::Relaxed) {
            unsafe { SetEvent(self.rescan_evt as HANDLE) };
        }
    }

    pub fn get(&self) -> Option<Arc<FileIndex>> {
        self.current.lock().ok()?.clone()
    }
}

pub fn spawn(hwnd: isize, notify_msg: u32, extra_roots: Vec<PathBuf>, user_folders: bool, max: usize) -> Arc<Indexer> {
    let evt = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    let ix = Arc::new(Indexer {
        current: Mutex::new(None),
        dirty: AtomicBool::new(true),
        apps_dirty: AtomicBool::new(false),
        scanning: AtomicBool::new(false),
        rescan_evt: evt as isize,
    });
    let me = ix.clone();
    std::thread::Builder::new()
        .name("indexer".into())
        .stack_size(256 * 1024)
        .spawn(move || unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
            let roots = default_roots(&extra_roots, user_folders);
            let menus = start_menu_dirs();
            let mut handles: Vec<HANDLE> = vec![me.rescan_evt as HANDLE];
            let filter = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME;
            let n_roots = roots.len();
            for r in roots.iter().map(|r| &r.path).chain(menus.iter()) {
                let h = FindFirstChangeNotificationW(wide(&String::from_utf16_lossy(r)).as_ptr(), 1, filter);
                handles.push(h); // keep index alignment even when invalid
            }
            let roots_template: Vec<(Vec<u16>, Vec<u16>)> = roots.iter().map(|r| (r.path.clone(), r.label.clone())).collect();
            let do_scan = |me: &Indexer| {
                me.scanning.store(true, Ordering::Relaxed);
                me.dirty.store(false, Ordering::Relaxed);
                let rs = roots_template.iter().map(|(p, l)| Root { path: p.clone(), label: l.clone() }).collect();
                let idx = Arc::new(scan(rs, max));
                // swap, then drop the old index outside the lock
                let old = me.current.lock().map(|mut g| g.replace(idx)).ok().flatten();
                drop(old);
                me.scanning.store(false, Ordering::Relaxed);
                PostMessageW(hwnd as _, notify_msg, 0, 0);
            };
            do_scan(&me);
            let valid: Vec<(usize, HANDLE)> = handles.iter().copied().enumerate().filter(|(_, h)| *h != INVALID_HANDLE_VALUE).collect();
            let wait: Vec<HANDLE> = valid.iter().map(|v| v.1).collect();
            loop {
                let r = WaitForMultipleObjects(wait.len() as u32, wait.as_ptr(), 0, INFINITE);
                let k = r.wrapping_sub(WAIT_OBJECT_0) as usize;
                if k >= wait.len() {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    continue;
                }
                let orig = valid[k].0;
                if orig == 0 {
                    if me.dirty.load(Ordering::Relaxed) {
                        do_scan(&me);
                    }
                } else {
                    if orig <= n_roots {
                        me.dirty.store(true, Ordering::Relaxed);
                    } else {
                        me.apps_dirty.store(true, Ordering::Relaxed);
                    }
                    FindNextChangeNotification(wait[k]);
                }
            }
        })
        .expect("spawn indexer");
    ix
}

//! Reader for the application + icon cache written by the helper process
//! (`csrc/shellhelper.c`). The file is memory-mapped read-only: icon pixels
//! are file-backed pages that only become resident when actually drawn and
//! never count against the process's private memory.

use crate::util::wtf8_push;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Memory::*;

const MAGIC: u32 = 0x4158_4449;
const VERSION: u32 = 1;
pub const NO_ICON: u32 = u32::MAX;

pub struct App {
    name8: (u32, u32),  // range in AppsCache::names8 (WTF-8, for matching)
    name16: (u32, u32), // u16 offset/len in the mapped string pool
    launch16: (u32, u32),
    pub icon: u32,
}

pub struct AppsCache {
    file: HANDLE,
    mapping: HANDLE,
    base: *const u8,
    len: usize,
    pub path: PathBuf,
    pub icon_px: u32,
    pub apps: Vec<App>,
    names8: Vec<u8>,
    exts: Vec<(Vec<u8>, u32)>, // sorted (lowercase ext utf8, icon)
    pub folder_icon: u32,
    pub file_icon: u32,
    strings_off: usize,
    icons_off: usize,
    n_icons: u32,
}

impl Drop for AppsCache {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.base as *mut _ });
            CloseHandle(self.mapping);
            CloseHandle(self.file);
        }
    }
}

impl AppsCache {
    fn u32_at(&self, off: usize) -> Option<u32> {
        if off + 4 > self.len {
            return None;
        }
        Some(unsafe { (self.base.add(off) as *const u32).read_unaligned() })
    }

    fn str16(&self, off: u32, len: u32) -> &[u16] {
        let start = self.strings_off + off as usize * 2;
        let end = start + len as usize * 2;
        if end > self.icons_off || end > self.len {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.base.add(start) as *const u16, len as usize) }
    }

    pub fn name(&self, i: usize) -> &[u16] {
        let a = &self.apps[i];
        self.str16(a.name16.0, a.name16.1)
    }

    pub fn name8(&self, i: usize) -> &[u8] {
        let (o, l) = self.apps[i].name8;
        &self.names8[o as usize..(o + l) as usize]
    }

    pub fn launch_id(&self, i: usize) -> &[u16] {
        let a = &self.apps[i];
        self.str16(a.launch16.0, a.launch16.1)
    }

    pub fn icon(&self, idx: u32) -> Option<&[u32]> {
        if idx >= self.n_icons {
            return None;
        }
        let sz = (self.icon_px * self.icon_px) as usize;
        let start = self.icons_off + idx as usize * sz * 4;
        if start + sz * 4 > self.len {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts(self.base.add(start) as *const u32, sz) })
    }

    pub fn ext_icon(&self, ext: &[u8]) -> u32 {
        if ext.is_empty() {
            return self.file_icon;
        }
        let mut lower = [0u8; 16];
        let n = ext.len().min(16);
        for (d, s) in lower.iter_mut().zip(ext) {
            *d = s.to_ascii_lowercase();
        }
        match self.exts.binary_search_by(|(e, _)| e.as_slice().cmp(&lower[..n])) {
            Ok(i) => self.exts[i].1,
            Err(_) => self.file_icon,
        }
    }

    pub fn open(path: &Path) -> Option<AppsCache> {
        let wpath = crate::util::wide(&path.to_string_lossy());
        unsafe {
            let file = CreateFileW(
                wpath.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            );
            if file == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut size = 0i64;
            if GetFileSizeEx(file, &mut size) == 0 || size < 32 {
                CloseHandle(file);
                return None;
            }
            let mapping = CreateFileMappingW(file, std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null());
            if mapping.is_null() {
                CloseHandle(file);
                return None;
            }
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
            if view.Value.is_null() {
                CloseHandle(mapping);
                CloseHandle(file);
                return None;
            }
            let mut c = AppsCache {
                file,
                mapping,
                base: view.Value as *const u8,
                len: size as usize,
                path: path.to_path_buf(),
                icon_px: 0,
                apps: Vec::new(),
                names8: Vec::new(),
                exts: Vec::new(),
                folder_icon: NO_ICON,
                file_icon: NO_ICON,
                strings_off: 0,
                icons_off: 0,
                n_icons: 0,
            };
            c.parse().then_some(c)
        }
    }

    fn parse(&mut self) -> bool {
        let h: Vec<u32> = (0..8).filter_map(|i| self.u32_at(i * 4)).collect();
        if h.len() != 8 || h[0] != MAGIC || h[1] != VERSION {
            return false;
        }
        let (icon_px, n_apps, n_exts) = (h[2], h[3] as usize, h[4] as usize);
        let (strings_off, icons_off, n_icons) = (h[5] as usize, h[6] as usize, h[7]);
        if icon_px == 0 || icon_px > 256 || strings_off > icons_off || icons_off > self.len {
            return false;
        }
        if 32 + n_apps * 20 + n_exts * 12 > strings_off {
            return false;
        }
        if icons_off as u64 + n_icons as u64 * (icon_px as u64 * icon_px as u64 * 4) > self.len as u64 {
            return false;
        }
        self.icon_px = icon_px;
        self.strings_off = strings_off;
        self.icons_off = icons_off;
        self.n_icons = n_icons;

        let mut apps = Vec::with_capacity(n_apps);
        let mut names8 = Vec::with_capacity(n_apps * 20);
        for i in 0..n_apps {
            let o = 32 + i * 20;
            let f: Vec<u32> = (0..5).filter_map(|k| self.u32_at(o + k * 4)).collect();
            let off8 = names8.len() as u32;
            wtf8_push(&mut names8, self.str16(f[0], f[1]));
            apps.push(App {
                name8: (off8, names8.len() as u32 - off8),
                name16: (f[0], f[1]),
                launch16: (f[2], f[3]),
                icon: f[4],
            });
        }
        let mut exts = Vec::with_capacity(n_exts);
        for i in 0..n_exts {
            let o = 32 + n_apps * 20 + i * 12;
            let (eo, el, icon) = (self.u32_at(o).unwrap(), self.u32_at(o + 4).unwrap(), self.u32_at(o + 8).unwrap());
            let e: Vec<u8> = self.str16(eo, el).iter().map(|&c| c as u8).collect();
            match e.as_slice() {
                b"/" => self.folder_icon = icon,
                b"" => self.file_icon = icon,
                _ => exts.push((e, icon)),
            }
        }
        exts.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        exts.dedup_by(|a, b| a.0 == b.0);
        names8.shrink_to_fit();
        self.apps = apps;
        self.names8 = names8;
        self.exts = exts;
        true
    }
}

pub fn cache_dir() -> PathBuf {
    crate::util::local_dir()
}

/// Newest `apps-*.bin` in the cache dir.
pub fn newest_cache() -> Option<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(cache_dir())
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().map_or(false, |n| {
            let n = n.to_string_lossy();
            n.starts_with("apps-") && n.ends_with(".bin")
        }))
        .collect();
    v.sort();
    v.pop()
}

/// Remove every cache file except `keep`.
pub fn prune_caches(keep: &Path) {
    if let Ok(rd) = std::fs::read_dir(cache_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            let n = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if p != keep && n.starts_with("apps-") && (n.ends_with(".bin") || n.ends_with(".tmp")) {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

pub fn new_cache_path() -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    cache_dir().join(format!("apps-{:020}.bin", t))
}

/// Always-included extensions so common files get proper icons even
/// before the first scan finishes.
pub const BASE_EXTS: &[&str] = &[
    "txt", "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "csv", "rtf", "md", "json", "xml",
    "html", "htm", "png", "jpg", "jpeg", "gif", "bmp", "svg", "webp", "heic", "ico", "mp3", "wav",
    "flac", "m4a", "ogg", "mp4", "mkv", "mov", "avi", "webm", "zip", "rar", "7z", "tar", "gz",
    "exe", "msi", "lnk", "url", "bat", "cmd", "ps1", "py", "js", "ts", "rs", "c", "h", "cpp",
    "cs", "java", "go", "css", "iso", "psd", "ai", "blend", "epub", "torrent", "ttf", "otf",
];

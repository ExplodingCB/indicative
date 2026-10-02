//! Small helpers: UTF-16 strings, WTF-8, registry, data folders.

use std::ffi::c_void;
use std::path::PathBuf;
use windows_sys::Win32::System::Registry::*;

/// NUL-terminated UTF-16 from a &str.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}


pub fn wlen(p: &[u16]) -> usize {
    p.iter().position(|&c| c == 0).unwrap_or(p.len())
}

/// Encode UTF-16 (possibly ill-formed) as WTF-8 so file names round-trip
/// exactly while costing one byte per ASCII char.
pub fn wtf8_push(out: &mut Vec<u8>, w: &[u16]) {
    let mut i = 0;
    while i < w.len() {
        let c = w[i] as u32;
        i += 1;
        let cp = if (0xD800..0xDC00).contains(&c) && i < w.len() && (0xDC00..0xE000).contains(&(w[i] as u32)) {
            let lo = w[i] as u32;
            i += 1;
            0x10000 + ((c - 0xD800) << 10) + (lo - 0xDC00)
        } else {
            c
        };
        if cp < 0x80 {
            out.push(cp as u8);
        } else if cp < 0x800 {
            out.extend_from_slice(&[0xC0 | (cp >> 6) as u8, 0x80 | (cp & 0x3F) as u8]);
        } else if cp < 0x10000 {
            out.extend_from_slice(&[0xE0 | (cp >> 12) as u8, 0x80 | ((cp >> 6) & 0x3F) as u8, 0x80 | (cp & 0x3F) as u8]);
        } else {
            out.extend_from_slice(&[
                0xF0 | (cp >> 18) as u8,
                0x80 | ((cp >> 12) & 0x3F) as u8,
                0x80 | ((cp >> 6) & 0x3F) as u8,
                0x80 | (cp & 0x3F) as u8,
            ]);
        }
    }
}

pub fn wtf8_decode(b: &[u8], out: &mut Vec<u16>) {
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as u32;
        let (cp, n) = if c < 0x80 {
            (c, 1)
        } else if c >= 0xF0 && i + 3 < b.len() {
            (((c & 7) << 18) | ((b[i + 1] as u32 & 0x3F) << 12) | ((b[i + 2] as u32 & 0x3F) << 6) | (b[i + 3] as u32 & 0x3F), 4)
        } else if c >= 0xE0 && i + 2 < b.len() {
            (((c & 0xF) << 12) | ((b[i + 1] as u32 & 0x3F) << 6) | (b[i + 2] as u32 & 0x3F), 3)
        } else if c >= 0xC0 && i + 1 < b.len() {
            (((c & 0x1F) << 6) | (b[i + 1] as u32 & 0x3F), 2)
        } else {
            (0xFFFD, 1)
        };
        i += n;
        if cp >= 0x10000 {
            let v = cp - 0x10000;
            out.push(0xD800 + (v >> 10) as u16);
            out.push(0xDC00 + (v & 0x3FF) as u16);
        } else {
            out.push(cp as u16);
        }
    }
}

pub fn reg_dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
    let (k, v) = (wide(key), wide(value));
    let mut data = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut data as *mut u32 as *mut c_void, &mut size)
    };
    (r == 0).then_some(data)
}

/// Reads REG_SZ / REG_EXPAND_SZ (expanded).
pub fn reg_string(root: HKEY, key: &str, value: &str) -> Option<Vec<u16>> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as *mut c_void, &mut size)
    };
    if r != 0 {
        return None;
    }
    buf.truncate(wlen(&buf));
    Some(buf)
}

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Indicative")
}

/// %LOCALAPPDATA%\Indicative (caches, history)
pub fn local_dir() -> PathBuf {
    let d = env_dir("LOCALAPPDATA");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// %APPDATA%\Indicative (config)
pub fn roaming_dir() -> PathBuf {
    let d = env_dir("APPDATA");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Days since 2000-01-01, the unit used for file recency in the index.
pub fn today_days() -> u16 {
    ((now_secs() / 86400).saturating_sub(10957)).min(32767) as u16
}

#[cfg(test)]
mod tests {
    #[test]
    fn wtf8_round_trips_ill_formed_utf16() {
        let samples: [&[u16]; 4] = [
            &[b'a' as u16, 0x00E9, 0x4E2D],
            &[0xD83D, 0xDE00],      // valid surrogate pair
            &[0xD800, b'x' as u16], // lone high surrogate
            &[0xDC00],              // lone low surrogate
        ];
        for s in samples {
            let mut b = Vec::new();
            super::wtf8_push(&mut b, s);
            let mut back = Vec::new();
            super::wtf8_decode(&b, &mut back);
            assert_eq!(back, s);
        }
    }
}

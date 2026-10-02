//! Launch history: lets frequently / recently opened items float up,
//! the way Spotlight learns what you pick.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const MAX_ENTRIES: usize = 800;

pub struct History {
    map: HashMap<Vec<u8>, (u32, u64)>, // WTF-8 key -> (count, last unix secs)
    /// Last path component of every key, so file search can skip building
    /// full paths for names that were never opened.
    names: HashSet<Vec<u8>>,
    path: PathBuf,
}

impl History {
    pub fn load() -> History {
        let path = crate::util::local_dir().join("history.txt");
        let mut map = HashMap::new();
        if let Ok(data) = std::fs::read(&path) {
            for line in data.split(|&b| b == b'\n') {
                let mut it = line.splitn(3, |&b| b == b'\t');
                let (Some(c), Some(t), Some(k)) = (it.next(), it.next(), it.next()) else { continue };
                let c = std::str::from_utf8(c).ok().and_then(|s| s.parse().ok()).unwrap_or(0u32);
                let t = std::str::from_utf8(t).ok().and_then(|s| s.parse().ok()).unwrap_or(0u64);
                let k = k.strip_suffix(b"\r").unwrap_or(k);
                if c > 0 && !k.is_empty() {
                    map.insert(k.to_vec(), (c, t));
                }
            }
        }
        let mut h = History { map, names: HashSet::new(), path };
        h.rebuild_names();
        h
    }

    fn rebuild_names(&mut self) {
        self.names = self.map.keys().map(|k| Self::last_component(k).to_vec()).collect();
    }

    fn last_component(k: &[u8]) -> &[u8] {
        k.rsplit(|&b| b == b'\\').next().unwrap_or(k)
    }

    /// Could a file with this (WTF-8) name have history?
    pub fn knows_name(&self, name: &[u8]) -> bool {
        self.names.contains(name)
    }

    fn key(target: &[u16]) -> Vec<u8> {
        let mut k = Vec::with_capacity(target.len());
        crate::util::wtf8_push(&mut k, target);
        k
    }

    /// Score bonus for a launch target (0 if never used).
    pub fn boost(&self, target: &[u16], now: u64) -> i32 {
        if self.map.is_empty() {
            return 0;
        }
        match self.map.get(&Self::key(target)) {
            None => 0,
            Some(&(count, last)) => {
                let age_days = now.saturating_sub(last) / 86400;
                let recency = match age_days {
                    0 => 450,
                    1..=6 => 300,
                    7..=29 => 120,
                    _ => 0,
                };
                (count.min(30) as i32) * 60 + recency
            }
        }
    }

    pub fn bump(&mut self, target: &[u16]) {
        let now = crate::util::now_secs();
        let e = self.map.entry(Self::key(target)).or_insert((0, now));
        e.0 = e.0.saturating_add(1);
        e.1 = now;
        if self.map.len() > MAX_ENTRIES {
            // drop the least valuable entries
            let mut v: Vec<_> = self.map.iter().map(|(k, &(c, t))| (c as u64 * 86400 * 7 + t, k.clone())).collect();
            v.sort_unstable();
            for (_, k) in v.into_iter().take(self.map.len() - MAX_ENTRIES) {
                self.map.remove(&k);
            }
        }
        self.rebuild_names();
        self.save();
    }

    fn save(&self) {
        let mut out = Vec::with_capacity(self.map.len() * 64);
        for (k, &(c, t)) in &self.map {
            out.extend_from_slice(format!("{c}\t{t}\t").as_bytes());
            out.extend_from_slice(k);
            out.push(b'\n');
        }
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, &out).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

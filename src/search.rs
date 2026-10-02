//! Turns a query into Spotlight-style sections.

use crate::apps::{AppsCache, NO_ICON};
use crate::ffi;
use crate::history::History;
use crate::index::FileIndex;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Calc,
    App,
    File,
    Folder,
    Web,
    Quit,
    Command,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Cache(u32),
    Glyph(u16),
}

pub struct Item {
    pub kind: Kind,
    pub title: Vec<u16>,
    pub detail: Vec<u16>,
    pub icon: Icon,
    /// What to open / copy.
    pub target: Vec<u16>,
    score: i32,
}

impl Item {
    pub fn new(kind: Kind, title: Vec<u16>, detail: Vec<u16>, icon: Icon, target: Vec<u16>) -> Item {
        Item { kind, title, detail, icon, target, score: 0 }
    }
}

pub struct Section {
    pub title: &'static str,
    pub items: Vec<Item>,
}

pub const GLYPH_CALC: u16 = 0xE8EF;
pub const GLYPH_WEB: u16 = 0xE774;
pub const GLYPH_POWER: u16 = 0xE7E8;
pub const GLYPH_APP: u16 = 0xE71D;
pub const GLYPH_FILE: u16 = 0xE8A5;
pub const GLYPH_FOLDER: u16 = 0xE8B7;

const MAX_ROWS: usize = 11;

pub struct Ctx<'a> {
    pub apps: Option<&'a AppsCache>,
    pub files: Option<&'a FileIndex>,
    pub history: &'a History,
    pub web_search: &'a str,
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn trim(q: &[u16]) -> &[u16] {
    let is_ws = |c: &u16| *c == b' ' as u16 || *c == b'\t' as u16;
    let s = q.iter().position(|c| !is_ws(c)).unwrap_or(q.len());
    let e = q.iter().rposition(|c| !is_ws(c)).map_or(s, |p| p + 1);
    &q[s..e]
}

fn url_encode(s: &str) -> String {
    let mut o = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            b' ' => o.push('+'),
            _ => o.push_str(&format!("%{:02X}", b)),
        }
    }
    o
}

pub fn run(query: &[u16], cx: &Ctx) -> Vec<Section> {
    let q16 = trim(query);
    if q16.is_empty() {
        return Vec::new();
    }
    let qs = String::from_utf16_lossy(q16);
    // lowercase + collapse whitespace runs
    let ql: String = qs.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let q8 = ql.as_bytes();
    let now = crate::util::now_secs();

    let mut sections = Vec::new();

    // ---- calculator
    if let Some((_, res)) = ffi::calc(q16) {
        let mut title = q16.to_vec();
        title.extend(w(" = "));
        title.extend_from_slice(&res);
        sections.push(Section {
            title: "Calculator",
            items: vec![Item { kind: Kind::Calc, title, detail: w("Copy result"), icon: Icon::Glyph(GLYPH_CALC), target: res, score: 0 }],
        });
    }

    // ---- applications (fuzzy allowed: small list, sloppy typing is common)
    let mut apps: Vec<Item> = Vec::new();
    if let Some(ac) = cx.apps {
        for i in 0..ac.apps.len() {
            let s = ffi::score(q8, ac.name8(i), true);
            if s <= 0 {
                continue;
            }
            let mut target = w("shell:AppsFolder\\");
            target.extend_from_slice(ac.launch_id(i));
            let score = s + cx.history.boost(&target, now);
            let icon = ac.apps[i].icon;
            apps.push(Item {
                kind: Kind::App,
                title: ac.name(i).to_vec(),
                detail: w("Application"),
                icon: if icon == NO_ICON { Icon::Glyph(GLYPH_APP) } else { Icon::Cache(icon) },
                target,
                score,
            });
        }
    }
    {
        let s = ffi::score(q8, b"Quit Indicative", false);
        if s > 0 && q8.len() >= 2 {
            apps.push(Item {
                kind: Kind::Quit,
                title: w("Quit Indicative"),
                detail: w("Command"),
                icon: Icon::Glyph(GLYPH_POWER),
                target: Vec::new(),
                score: s - 500,
            });
        }
    }
    apps.sort_by(|a, b| b.score.cmp(&a.score));
    // collapse duplicate names (same app pinned twice etc.)
    apps.dedup_by(|a, b| a.title == b.title);

    // ---- files & folders (no fuzzy: 60k names would drown the results)
    let mut docs: Vec<Item> = Vec::new();
    let mut folders: Vec<Item> = Vec::new();
    if let (Some(fi), true) = (cx.files, q8.len() >= 2) {
        let min = if q8.len() < 3 { 7001 } else { 4000 };
        let today = crate::util::today_days();
        const K: usize = 32;
        let mut top: Vec<(i32, usize)> = Vec::with_capacity(K + 1);
        let mut floor = min;
        for i in 0..fi.entries.len() {
            let s = ffi::score(q8, fi.name(i), false);
            if s < floor {
                continue;
            }
            let age = today.saturating_sub(fi.days(i));
            let rec = match age {
                0..=2 => 300,
                3..=14 => 180,
                15..=90 => 60,
                _ => 0,
            };
            let s = s + rec;
            let pos = top.partition_point(|&(ts, _)| ts >= s);
            if pos < K {
                top.insert(pos, (s, i));
                if top.len() > K {
                    top.pop();
                    floor = floor.max(top[K - 1].0 - 300 - 2000); // conservative: history can add up to ~2250
                }
            }
        }
        for (s, i) in top {
            let path = fi.full_path(i);
            // deep build/output trees shouldn't outrank files you actually keep around
            let deep = fi.depth(i).saturating_sub(2).min(8) as i32 * 60;
            let score = s - deep + cx.history.boost(&path, now);
            let mut title = Vec::new();
            crate::util::wtf8_decode(fi.name(i), &mut title);
            let dir = fi.is_dir(i);
            let icon = match cx.apps {
                Some(ac) if dir && ac.folder_icon != NO_ICON => Icon::Cache(ac.folder_icon),
                Some(ac) if !dir && ac.ext_icon(fi.ext(i)) != NO_ICON => Icon::Cache(ac.ext_icon(fi.ext(i))),
                _ => Icon::Glyph(if dir { GLYPH_FOLDER } else { GLYPH_FILE }),
            };
            let it = Item { kind: if dir { Kind::Folder } else { Kind::File }, title, detail: fi.location(i), icon, target: path, score };
            if dir { folders.push(it) } else { docs.push(it) }
        }
        docs.sort_by(|a, b| b.score.cmp(&a.score));
        folders.sort_by(|a, b| b.score.cmp(&a.score));
    }

    // ---- top hit = best of everything, with applications favoured like Spotlight
    const APP_BIAS: i32 = 1500;
    let best = [apps.first().map(|i| i.score + APP_BIAS), docs.first().map(|i| i.score), folders.first().map(|i| i.score - 200)];
    let pick = best.iter().enumerate().filter_map(|(k, s)| s.map(|s| (s, k))).max_by_key(|&(s, k)| (s, std::cmp::Reverse(k)));
    if let Some((_, k)) = pick {
        let top = match k {
            0 => apps.remove(0),
            1 => docs.remove(0),
            _ => folders.remove(0),
        };
        sections.push(Section { title: "Top Hit", items: vec![top] });
    }

    // ---- budget the remaining rows
    let used: usize = sections.iter().map(|s| s.items.len()).sum::<usize>() + 1; // +1 web row
    let mut budget = MAX_ROWS.saturating_sub(used);
    let mut want = [apps.len().min(4), docs.len().min(4), folders.len().min(3)];
    while want.iter().sum::<usize>() > budget {
        // shrink the largest bucket first
        let k = (0..3).max_by_key(|&k| (want[k], k)).unwrap();
        want[k] -= 1;
    }
    apps.truncate(want[0]);
    docs.truncate(want[1]);
    folders.truncate(want[2]);
    budget -= want.iter().sum::<usize>();
    let _ = budget;
    if !apps.is_empty() {
        sections.push(Section { title: "Applications", items: apps });
    }
    if !docs.is_empty() {
        sections.push(Section { title: "Documents", items: docs });
    }
    if !folders.is_empty() {
        sections.push(Section { title: "Folders", items: folders });
    }

    // ---- web fallback
    let url = cx.web_search.replace("%s", &url_encode(&qs));
    let host = url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or("").trim_start_matches("www.").to_string();
    let mut title = w("Search the web for \u{201C}");
    title.extend_from_slice(q16);
    title.extend(w("\u{201D}"));
    sections.push(Section {
        title: "Web",
        items: vec![Item { kind: Kind::Web, title, detail: w(&host), icon: Icon::Glyph(GLYPH_WEB), target: w(&url), score: 0 }],
    });

    sections
}

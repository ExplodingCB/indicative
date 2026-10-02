//! %APPDATA%\Indicative\config.ini - a tiny hand-parsed INI.

use std::path::PathBuf;

const DEFAULT: &str = "\
; Indicative configuration.
; Restart to apply: run `indicative.exe --quit`, then start indicative.exe again.
[indicative]

; Global shortcut. Modifiers: alt, ctrl, shift, win. Keys: space, a-z, 0-9, f1-f24, `
hotkey = win+space

; Web search fallback; %s is replaced with the URL-encoded query.
web_search = https://www.google.com/search?q=%s

; Extra folders to index for files, separated by ;  (Desktop, Documents,
; Downloads, Pictures, Music and Videos are always included.)
extra_roots =

; Index Desktop/Documents/Downloads/Pictures/Music/Videos. Set to false to
; index only extra_roots.
index_user_folders = true

; Upper bound on indexed files + folders (~30 bytes of RAM each).
max_files = 60000

; auto | dark | light
theme = auto

; Give memory back to Windows a few seconds after the panel hides.
trim_memory = true
";

#[derive(Clone, Copy, PartialEq)]
pub enum Theme {
    Auto,
    Dark,
    Light,
}

pub struct Config {
    pub hotkey_mods: u32,
    pub hotkey_vk: u32,
    pub hotkey_label: String,
    pub web_search: String,
    pub extra_roots: Vec<PathBuf>,
    pub index_user_folders: bool,
    pub max_files: usize,
    pub theme: Theme,
    pub trim_memory: bool,
}

impl Config {
    pub fn load() -> Config {
        let path = crate::util::roaming_dir().join("config.ini");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                let _ = std::fs::write(&path, DEFAULT);
                DEFAULT.to_string()
            }
        };
        let mut c = Config {
            hotkey_mods: 0x8, // MOD_WIN
            hotkey_vk: 0x20,  // VK_SPACE
            hotkey_label: "Win+Space".into(),
            web_search: "https://www.google.com/search?q=%s".into(),
            extra_roots: Vec::new(),
            index_user_folders: true,
            max_files: 60000,
            theme: Theme::Auto,
            trim_memory: true,
        };
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with(';') || line.starts_with('#') || line.starts_with('[') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            match k.as_str() {
                "hotkey" => {
                    if let Some((m, vk, label)) = parse_hotkey(v) {
                        c.hotkey_mods = m;
                        c.hotkey_vk = vk;
                        c.hotkey_label = label;
                    }
                }
                "web_search" if v.contains("%s") => c.web_search = v.to_string(),
                "extra_roots" => {
                    c.extra_roots = v.split(';').map(str::trim).filter(|s| !s.is_empty()).map(PathBuf::from).collect()
                }
                "index_user_folders" => c.index_user_folders = !is_false(v),
                "max_files" => c.max_files = v.parse().unwrap_or(c.max_files).clamp(1000, 2_000_000),
                "theme" => {
                    c.theme = match v.to_ascii_lowercase().as_str() {
                        "dark" => Theme::Dark,
                        "light" => Theme::Light,
                        _ => Theme::Auto,
                    }
                }
                "trim_memory" => c.trim_memory = !is_false(v),
                _ => {}
            }
        }
        c
    }
}

fn is_false(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "false" | "0" | "no" | "off")
}

fn parse_hotkey(s: &str) -> Option<(u32, u32, String)> {
    let mut mods = 0u32;
    let mut vk = 0u32;
    let mut label = Vec::new();
    for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "alt" => { mods |= 0x1; label.push("Alt".to_string()) }
            "ctrl" | "control" => { mods |= 0x2; label.push("Ctrl".to_string()) }
            "shift" => { mods |= 0x4; label.push("Shift".to_string()) }
            "win" | "super" | "meta" => { mods |= 0x8; label.push("Win".to_string()) }
            "space" => { vk = 0x20; label.push("Space".to_string()) }
            "`" | "backtick" | "grave" => { vk = 0xC0; label.push("`".to_string()) }
            k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => {
                vk = k.as_bytes()[0].to_ascii_uppercase() as u32;
                label.push(k.to_ascii_uppercase());
            }
            k if k.starts_with('f') && k[1..].parse::<u32>().map_or(false, |n| (1..=24).contains(&n)) => {
                vk = 0x6F + k[1..].parse::<u32>().unwrap();
                label.push(k.to_ascii_uppercase());
            }
            _ => return None,
        }
    }
    (vk != 0).then(|| (mods, vk, label.join("+")))
}

#[cfg(test)]
mod tests {
    use super::parse_hotkey;

    #[test]
    fn hotkeys() {
        assert_eq!(parse_hotkey("win+space").map(|h| (h.0, h.1)), Some((0x8, 0x20)));
        assert_eq!(parse_hotkey("Ctrl + Shift + K").map(|h| (h.0, h.1)), Some((0x6, b'K' as u32)));
        assert_eq!(parse_hotkey("alt+f12").map(|h| h.1), Some(0x7B));
        assert!(parse_hotkey("alt+nonsense").is_none());
        assert!(parse_hotkey("ctrl").is_none());
    }
}

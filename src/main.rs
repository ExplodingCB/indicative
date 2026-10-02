//! Indicative - a Spotlight-style launcher for Windows.
//!
//! One exe, three roles:
//!   indicative.exe                 resident launcher (hotkey, UI, index)
//!   indicative.exe --apps ...      short-lived helper: enumerates apps + icons via COM
//!   indicative.exe --open ...      short-lived helper: ShellExecute a target
//! The helpers keep shell32/COM (several MB each) out of the resident process.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod apps;
mod bench;
mod config;
mod ffi;
mod history;
mod hotkey;
mod index;
mod search;
mod ui;
mod util;

use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;

fn wide_os(s: &OsString) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// `--apps <icon_px> <out_path> <ext|ext|...>`
fn helper_apps(args: &[OsString]) -> i32 {
    let px: i32 = args.get(1).and_then(|s| s.to_str()).and_then(|s| s.parse().ok()).unwrap_or(32);
    let Some(out) = args.get(2) else { return 2 };
    let exts = args.get(3).cloned().unwrap_or_default();
    let mut tmp = out.clone();
    tmp.push(".tmp");
    let ok = unsafe { ffi::sh_build_apps_cache(wide_os(&tmp).as_ptr(), px, wide_os(&exts).as_ptr()) } != 0;
    if ok && std::fs::rename(&tmp, out).is_ok() {
        0
    } else {
        let _ = std::fs::remove_file(&tmp);
        1
    }
}

/// `--open <verb> <target>`
fn helper_open(args: &[OsString]) -> i32 {
    let (Some(verb), Some(target)) = (args.get(1), args.get(2)) else { return 2 };
    let ok = unsafe { ffi::sh_open(wide_os(target).as_ptr(), wide_os(verb).as_ptr()) } != 0;
    if ok { 0 } else { 1 }
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let first = args.first().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match first.as_str() {
        "--apps" => std::process::exit(helper_apps(&args)),
        "--open" => std::process::exit(helper_open(&args)),
        "--quit" => {
            ui::signal_running(ui::WM_APP_QUIT);
        }
        "--install" => {
            ui::set_autostart(true);
            ui::run(true);
        }
        "--uninstall" => {
            ui::set_autostart(false);
            ui::signal_running(ui::WM_APP_QUIT);
        }
        "--background" => ui::run(false),
        "--bench" => bench::run(&args[1..].iter().map(std::path::PathBuf::from).collect::<Vec<_>>()),
        _ => ui::run(true),
    }
}

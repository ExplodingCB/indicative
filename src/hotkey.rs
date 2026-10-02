//! Global shortcut.
//!
//! Plain combos (Alt+Space, Ctrl+`, ...) use RegisterHotKey. Anything with
//! the Win key goes through a low-level keyboard hook instead, because
//! Windows reserves combos like Win+Space (input-language switcher) and
//! RegisterHotKey refuses them. The hook lives on its own small thread so
//! keystrokes system-wide never wait on the UI thread.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const MOD_ALT_: u32 = 0x1;
const MOD_CTRL_: u32 = 0x2;
const MOD_SHIFT_: u32 = 0x4;
const MOD_WIN_: u32 = 0x8;
/// Unassigned virtual key, used as a harmless "something else was pressed"
/// marker so releasing Win doesn't open the Start menu.
const VK_NOOP: u16 = 0xE8;

static TARGET: AtomicIsize = AtomicIsize::new(0);
static MODS: AtomicU32 = AtomicU32::new(0);
static VK: AtomicU32 = AtomicU32::new(0);
static HELD: AtomicBool = AtomicBool::new(false);

fn down(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

/// Inject a no-op key press. Also gives this process "last input" status,
/// which Windows requires before it lets us take the foreground.
pub fn tap_noop_key() {
    unsafe {
        let mut inputs: [INPUT; 2] = std::mem::zeroed();
        for (i, inp) in inputs.iter_mut().enumerate() {
            inp.r#type = INPUT_KEYBOARD;
            inp.Anonymous.ki.wVk = VK_NOOP;
            inp.Anonymous.ki.dwFlags = if i == 1 { KEYEVENTF_KEYUP } else { 0 };
        }
        SendInput(2, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
}

unsafe extern "system" fn ll_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = &*(lp as *const KBDLLHOOKSTRUCT);
        if k.flags & LLKHF_INJECTED == 0 && k.vkCode == VK.load(Ordering::Relaxed) {
            let is_down = wp as u32 == WM_KEYDOWN || wp as u32 == WM_SYSKEYDOWN;
            if is_down {
                let want = MODS.load(Ordering::Relaxed);
                let have = (if down(VK_MENU) { MOD_ALT_ } else { 0 })
                    | (if down(VK_CONTROL) { MOD_CTRL_ } else { 0 })
                    | (if down(VK_SHIFT) { MOD_SHIFT_ } else { 0 })
                    | (if down(VK_LWIN) || down(VK_RWIN) { MOD_WIN_ } else { 0 });
                if have == want {
                    // fire once per press, swallow auto-repeat
                    if !HELD.swap(true, Ordering::Relaxed) {
                        tap_noop_key();
                        PostMessageW(TARGET.load(Ordering::Relaxed) as HWND, WM_HOTKEY, 1, 0);
                    }
                    return 1;
                }
            } else if HELD.swap(false, Ordering::Relaxed) {
                return 1; // swallow the matching key-up
            }
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

/// Install the shortcut. Returns false if it couldn't be registered.
pub fn install(hwnd: HWND, mods: u32, vk: u32) -> bool {
    if mods & MOD_WIN_ == 0 {
        return unsafe { RegisterHotKey(hwnd, 1, mods | MOD_NOREPEAT, vk) } != 0;
    }
    TARGET.store(hwnd as isize, Ordering::Relaxed);
    MODS.store(mods, Ordering::Relaxed);
    VK.store(vk, Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("hotkey".into()).stack_size(64 * 1024).spawn(move || unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), GetModuleHandleW(std::ptr::null()), 0);
        let _ = tx.send(!hook.is_null());
        if hook.is_null() {
            return;
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {}
        UnhookWindowsHookEx(hook);
    });
    spawned.is_ok() && rx.recv().unwrap_or(false)
}

//! The Spotlight panel: a borderless DWM-backdrop popup rendered in
//! horizontal strips through the C compositor.
//!
//! Memory notes: nothing is drawn while hidden; the strip/mask bitmaps
//! (window width x ~60px) are freed on hide and the working set is trimmed
//! shortly after, so the idle process is just the index + a few handles.

use crate::apps::{self, AppsCache};
use crate::config::{Config, Theme};
use crate::ffi::{self, RsSurface};
use crate::history::History;
use crate::index::{self, Indexer};
use crate::search::{self, Icon, Kind, Section};
use crate::util::{reg_dword, wide};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Controls::MARGINS;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_SHOW: u32 = WM_APP + 1;
pub const WM_APP_INDEXED: u32 = WM_APP + 2;
pub const WM_APP_APPS_READY: u32 = WM_APP + 3;
pub const WM_APP_QUIT: u32 = WM_APP + 4;

const CLASS: &str = "IndicativeSpotlight";
const TIMER_CARET: usize = 1;
const TIMER_TRIM: usize = 2;
/// Strip height in rows. Taller strips were measured: ~0.1 ms faster per
/// keystroke but double the memory while open, so strips stay one row tall.
const STRIP_ROWS: i32 = 1;

static UI: AtomicPtr<Ui> = AtomicPtr::new(null_mut());

// ---------------------------------------------------------------------------
// metrics / theme

#[derive(Clone, Copy)]
struct Metrics {
    s: f32,
    w: i32,
    input_h: i32,
    row_h: i32,
    header_h: i32,
    pad_top: i32,
    pad_bottom: i32,
    side: i32,
    icon: i32,
    text_x: i32,
}

impl Metrics {
    fn new(dpi: u32) -> Metrics {
        let s = dpi as f32 / 96.0;
        let p = |v: f32| (v * s).round() as i32;
        Metrics {
            s,
            w: p(680.0),
            input_h: p(58.0),
            row_h: p(40.0),
            header_h: p(26.0),
            pad_top: p(4.0),
            pad_bottom: p(8.0),
            side: p(8.0),
            icon: p(28.0),
            text_x: p(52.0),
        }
    }
    fn p(&self, v: f32) -> i32 {
        (v * self.s).round() as i32
    }
}

#[derive(Clone, Copy)]
struct Colors {
    bg: u32,
    text: u32,
    muted: u32,
    header: u32,
    sep: u32,
    placeholder: u32,
    glyph: u32,
    glyph_bg: u32,
    accent: u32,
    on_accent: u32,
    on_accent_muted: u32,
    text_sel: u32,
}

fn system_dark() -> bool {
    reg_dword(HKEY_CURRENT_USER, "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize", "AppsUseLightTheme") == Some(0)
}

fn accent_color() -> u32 {
    match reg_dword(HKEY_CURRENT_USER, "Software\\Microsoft\\Windows\\DWM", "AccentColor") {
        Some(v) => 0xFF00_0000 | ((v & 0xFF) << 16) | (v & 0xFF00) | ((v >> 16) & 0xFF),
        None => 0xFF0A_84FF,
    }
}

fn colors(dark: bool, backdrop: bool) -> Colors {
    let accent = accent_color();
    let (r, g, b) = (((accent >> 16) & 255) as f32, ((accent >> 8) & 255) as f32, (accent & 255) as f32);
    let lum = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0;
    let (on_accent, on_accent_muted) = if lum > 0.62 { (0xFF10_1010, 0xB310_1010) } else { (0xFFFF_FFFF, 0xCCFF_FFFF) };
    let text_sel = (accent & 0x00FF_FFFF) | 0x6600_0000;
    if dark {
        Colors {
            bg: if backdrop { 0x861C_1C1E } else { 0xF526_2628 },
            text: 0xFFF5_F5F7,
            muted: 0x99EB_EBF5,
            header: 0x8CEB_EBF5,
            sep: 0x24FF_FFFF,
            placeholder: 0x5CEB_EBF5,
            glyph: 0xCCEB_EBF5,
            glyph_bg: 0x24FF_FFFF,
            accent,
            on_accent,
            on_accent_muted,
            text_sel,
        }
    } else {
        Colors {
            bg: if backdrop { 0x80F4_F4F6 } else { 0xF7F6_F6F8 },
            text: 0xFF1D_1D1F,
            muted: 0x993C_3C43,
            header: 0x8C3C_3C43,
            sep: 0x1A00_0000,
            placeholder: 0x4D3C_3C43,
            glyph: 0xB33C_3C43,
            glyph_bg: 0x14000000,
            accent,
            on_accent,
            on_accent_muted,
            text_sel,
        }
    }
}

// ---------------------------------------------------------------------------
// GDI resources

struct Surface {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    w: i32,
    h: i32,
}

impl Surface {
    fn new(w: i32, h: i32) -> Option<Surface> {
        unsafe {
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB;
            let mut bits = null_mut();
            let bmp = CreateDIBSection(null_mut(), &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bmp.is_null() {
                return None;
            }
            let dc = CreateCompatibleDC(null_mut());
            let old = SelectObject(dc, bmp);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x00FF_FFFF);
            Some(Surface { dc, bmp, old, bits: bits as *mut u32, w, h })
        }
    }
    fn rs(&self, h: i32) -> RsSurface {
        RsSurface { px: self.bits, w: self.w, h: h.min(self.h), stride: self.w }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bmp);
            DeleteDC(self.dc);
        }
    }
}

struct Fonts {
    input: HFONT,
    title: HFONT,
    detail: HFONT,
    header: HFONT,
    glyph: HFONT,
    glyph_big: HFONT,
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for f in [self.input, self.title, self.detail, self.header, self.glyph, self.glyph_big] {
            unsafe { DeleteObject(f) };
        }
    }
}

fn make_font(mdc: HDC, px: i32, weight: i32, faces: &[&str]) -> HFONT {
    for (k, face) in faces.iter().enumerate() {
        let wf = wide(face);
        let f = unsafe {
            CreateFontW(-px, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_TT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, ANTIALIASED_QUALITY as u32, DEFAULT_PITCH as u32, wf.as_ptr())
        };
        if k + 1 == faces.len() {
            return f;
        }
        // keep it only if GDI actually matched the face
        let mut got = [0u16; 64];
        unsafe {
            let old = SelectObject(mdc, f);
            let n = GetTextFaceW(mdc, got.len() as i32, got.as_mut_ptr());
            SelectObject(mdc, old);
            let got = String::from_utf16_lossy(&got[..(n.max(1) - 1) as usize]);
            if got.eq_ignore_ascii_case(face) {
                return f;
            }
            DeleteObject(f);
        }
    }
    null_mut()
}

// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum RowKind {
    Header(usize),
    Item(usize),
}

#[derive(Clone, Copy)]
struct Row {
    y: i32,
    h: i32,
    kind: RowKind,
}

pub struct Ui {
    hwnd: HWND,
    cfg: Config,
    exe: PathBuf,
    mdc: HDC, // measuring DC
    dpi: u32,
    m: Metrics,
    fonts: Option<Fonts>,
    strip: Option<Surface>,
    mask: Option<Surface>,
    dark: bool,
    backdrop: bool,
    c: Colors,

    text: Vec<u16>,
    caret: usize,
    anchor: usize,
    scroll: i32,
    caret_on: bool,

    sections: Vec<Section>,
    flat: Vec<(usize, usize)>,
    rows: Vec<Row>,
    sel: usize,
    win_h: i32,
    win_pos: (i32, i32),
    visible: bool,

    last_mouse: (i32, i32),
    drag_text: bool,
    press_row: Option<usize>,

    apps: Option<AppsCache>,
    indexer: Arc<Indexer>,
    history: History,
    helper_running: bool,
    apps_refreshed: u64,
    indexed_once: bool,
    /// INDICATIVE_TRACE=<file>: log keystroke/hotkey -> pixels latency.
    trace: Option<std::fs::File>,
}

impl Ui {
    fn icon_px(&self) -> u32 {
        self.m.icon as u32
    }

    fn set_dpi(&mut self, dpi: u32) {
        if dpi == self.dpi && self.fonts.is_some() {
            return;
        }
        self.dpi = dpi;
        self.m = Metrics::new(dpi);
        let m = self.m;
        let text = ["Segoe UI Variable Text", "Segoe UI"];
        let display = ["Segoe UI Variable Display", "Segoe UI"];
        let icons = ["Segoe Fluent Icons", "Segoe MDL2 Assets"];
        self.fonts = None;
        self.fonts = Some(Fonts {
            input: make_font(self.mdc, m.p(25.0), 300, &display),
            title: make_font(self.mdc, m.p(14.5), 400, &text),
            detail: make_font(self.mdc, m.p(12.5), 400, &text),
            header: make_font(self.mdc, m.p(11.5), 600, &text),
            glyph: make_font(self.mdc, m.p(16.0), 400, &icons),
            glyph_big: make_font(self.mdc, m.p(19.0), 400, &icons),
        });
        self.strip = None;
        self.mask = None;
    }

    fn apply_theme(&mut self) {
        self.dark = match self.cfg.theme {
            Theme::Dark => true,
            Theme::Light => false,
            Theme::Auto => system_dark(),
        };
        unsafe {
            let v: i32 = self.dark as i32;
            DwmSetWindowAttribute(self.hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, &v as *const i32 as _, 4);
        }
        self.c = colors(self.dark, self.backdrop);
    }

    // ---------------- measuring

    fn measure(&self, font: HFONT, s: &[u16]) -> i32 {
        if s.is_empty() {
            return 0;
        }
        unsafe {
            let old = SelectObject(self.mdc, font);
            let mut sz = SIZE { cx: 0, cy: 0 };
            GetTextExtentPoint32W(self.mdc, s.as_ptr(), s.len() as i32, &mut sz);
            SelectObject(self.mdc, old);
            sz.cx
        }
    }

    /// x offsets of every caret position (len + 1 entries).
    fn caret_stops(&self) -> Vec<i32> {
        let mut v = vec![0i32; self.text.len() + 1];
        if self.text.is_empty() {
            return v;
        }
        unsafe {
            let f = self.fonts.as_ref().unwrap().input;
            let old = SelectObject(self.mdc, f);
            let mut fit = 0;
            let mut sz = SIZE { cx: 0, cy: 0 };
            GetTextExtentExPointW(self.mdc, self.text.as_ptr(), self.text.len() as i32, i32::MAX, &mut fit, v.as_mut_ptr().add(1), &mut sz);
            SelectObject(self.mdc, old);
        }
        v
    }

    // ---------------- results & layout

    fn refresh(&mut self, reset_sel: bool) {
        let files = self.indexer.get();
        let cx = search::Ctx { apps: self.apps.as_ref(), files: files.as_deref(), history: &self.history, web_search: &self.cfg.web_search };
        self.sections = search::run(&self.text, &cx);
        drop(files);
        let commands = self.command_items();
        if !commands.is_empty() {
            // put commands right after the top hit
            let at = self.sections.iter().position(|s| s.title != "Calculator" && s.title != "Top Hit").unwrap_or(self.sections.len());
            self.sections.insert(at, Section { title: "Indicative", items: commands });
        }
        self.flat.clear();
        for (si, s) in self.sections.iter().enumerate() {
            for ii in 0..s.items.len() {
                self.flat.push((si, ii));
            }
        }
        if reset_sel || self.sel >= self.flat.len() {
            self.sel = 0;
        }
        self.layout();
    }

    fn command_items(&self) -> Vec<search::Item> {
        let q: String = String::from_utf16_lossy(&self.text).trim().to_lowercase();
        if q.len() < 3 {
            return Vec::new();
        }
        let autostart = crate::autostart::enabled();
        let cmds: [(&str, &str, u16); 3] = [
            ("Indicative Settings", "settings", 0xE713),
            ("Rebuild Indicative Index", "reindex", 0xE72C),
            (if autostart { "Don't Start Indicative at Login" } else { "Start Indicative at Login" }, if autostart { "autostart-off" } else { "autostart-on" }, 0xE7E8),
        ];
        let mut out = Vec::new();
        for (title, id, glyph) in cmds {
            let tl = title.to_lowercase();
            if ffi::score(q.as_bytes(), tl.as_bytes(), false) >= 6000 {
                out.push(search::Item::command(title, id, glyph));
            }
        }
        out
    }

    fn layout(&mut self) {
        let m = self.m;
        self.rows.clear();
        let mut y = m.input_h;
        if !self.sections.is_empty() {
            y += m.pad_top;
            let mut fi = 0;
            for (si, s) in self.sections.iter().enumerate() {
                self.rows.push(Row { y, h: m.header_h, kind: RowKind::Header(si) });
                y += m.header_h;
                for _ in &s.items {
                    self.rows.push(Row { y, h: m.row_h, kind: RowKind::Item(fi) });
                    y += m.row_h;
                    fi += 1;
                }
            }
            y += m.pad_bottom;
        }
        if y != self.win_h {
            self.win_h = y;
            if self.visible {
                unsafe { SetWindowPos(self.hwnd, null_mut(), 0, 0, m.w, y, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW) };
            }
        }
    }

    fn item(&self, flat: usize) -> Option<&search::Item> {
        let &(s, i) = self.flat.get(flat)?;
        self.sections.get(s)?.items.get(i)
    }

    // ---------------- painting

    fn ensure_surfaces(&mut self) -> bool {
        let w = self.m.w;
        let h = self.m.input_h.max(self.m.row_h) * STRIP_ROWS;
        if self.strip.as_ref().map_or(true, |s| s.w != w || s.h != h) {
            self.strip = Surface::new(w, h);
            self.mask = Surface::new(w, h);
        }
        self.strip.is_some() && self.mask.is_some()
    }

    fn redraw(&mut self) {
        self.redraw_range(0, i32::MAX);
    }

    fn redraw_range(&mut self, y0: i32, y1: i32) {
        if !self.visible {
            return;
        }
        unsafe {
            let dc = GetDC(self.hwnd);
            self.paint(dc, y0, y1);
            ReleaseDC(self.hwnd, dc);
        }
    }

    fn paint(&mut self, dc: HDC, y0: i32, y1: i32) {
        if !self.ensure_surfaces() {
            return;
        }
        let band = self.strip.as_ref().unwrap().h;
        let total = self.win_h;
        let mut y = (y0.max(0) / band) * band;
        while y < total && y < y1 {
            let bh = band.min(total - y);
            let mut rs = self.strip.as_ref().unwrap().rs(bh);
            unsafe { ffi::rs_clear(&mut rs, self.c.bg) };
            self.draw(&mut rs, y, bh);
            unsafe { BitBlt(dc, 0, y, self.m.w, bh, self.strip.as_ref().unwrap().dc, 0, 0, SRCCOPY) };
            y += bh;
        }
    }

    /// Draw text into the band through the GDI coverage mask.
    #[allow(clippy::too_many_arguments)]
    fn text(&self, dst: &mut RsSurface, off: i32, font: HFONT, s: &[u16], x: i32, y: i32, w: i32, h: i32, color: u32, flags: u32, clip: Option<(i32, i32)>) {
        let ty = y - off;
        if s.is_empty() || w <= 0 || ty >= dst.h || ty + h <= 0 {
            return;
        }
        let mask = self.mask.as_ref().unwrap();
        let mut mrs = mask.rs(dst.h);
        unsafe {
            ffi::rs_zero_rect(&mut mrs, x, ty, w, h);
            let old = SelectObject(mask.dc, font);
            let mut rc = RECT { left: x, top: ty, right: x + w, bottom: ty + h };
            DrawTextW(mask.dc, s.as_ptr(), s.len() as i32, &mut rc, flags | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
            SelectObject(mask.dc, old);
            GdiFlush();
            let (cx0, cx1) = clip.unwrap_or((x, x + w));
            let (bx0, bx1) = (cx0.max(x), cx1.min(x + w));
            ffi::rs_mask_blend(dst, &mrs, bx0, ty, bx1 - bx0, h, color);
        }
    }

    fn draw(&self, dst: &mut RsSurface, off: i32, bh: i32) {
        let m = self.m;
        let c = self.c;
        let f = self.fonts.as_ref().unwrap();
        let vis = |y: i32, h: i32| y + h > off && y < off + bh;

        // ---- search field
        if vis(0, m.input_h) {
            self.text(dst, off, f.glyph_big, &[0xE721], m.p(14.0), 0, m.p(28.0), m.input_h, c.muted, DT_CENTER, None);
            let tx = m.text_x;
            let right = m.w - m.p(18.0);
            if self.text.is_empty() {
                let ph = wide("Indicative Search");
                self.text(dst, off, f.input, &ph[..ph.len() - 1], tx, 0, right - tx, m.input_h, c.placeholder, 0, None);
                let hint: Vec<u16> = self.cfg.hotkey_label.encode_utf16().collect();
                self.text(dst, off, f.detail, &hint, tx, 0, right - tx, m.input_h, c.placeholder, DT_RIGHT, None);
            } else {
                let stops = self.caret_stops();
                let tw = *stops.last().unwrap();
                let line_h = m.p(30.0);
                let ly = (m.input_h - line_h) / 2;
                if self.caret != self.anchor {
                    let (a, b) = (self.caret.min(self.anchor), self.caret.max(self.anchor));
                    let x0 = (tx + stops[a] - self.scroll).max(tx);
                    let x1 = (tx + stops[b] - self.scroll).min(right);
                    unsafe { ffi::rs_fill_rect(dst, x0, ly - off, x1 - x0, line_h, c.text_sel) };
                }
                self.text(dst, off, f.input, &self.text, tx - self.scroll, 0, tw + m.p(8.0), m.input_h, c.text, 0, Some((tx, right)));
                if self.caret_on && self.caret == self.anchor {
                    let cx = tx + stops[self.caret] - self.scroll;
                    let cw = ((1.5 * m.s).round() as i32).max(1);
                    unsafe { ffi::rs_fill_rect(dst, cx, ly - off, cw, line_h, c.accent) };
                }
            }
        }
        if self.sections.is_empty() {
            return;
        }
        if vis(m.input_h, 1) {
            unsafe { ffi::rs_fill_rect(dst, 0, m.input_h - off, m.w, ((m.s).round() as i32).max(1), c.sep) };
        }

        // ---- rows
        let x0 = m.side;
        let x1 = m.w - m.side;
        for r in &self.rows {
            if !vis(r.y, r.h) {
                continue;
            }
            match r.kind {
                RowKind::Header(si) => {
                    let t: Vec<u16> = self.sections[si].title.encode_utf16().collect();
                    self.text(dst, off, f.header, &t, x0 + m.p(10.0), r.y + m.p(4.0), x1 - x0, r.h - m.p(4.0), c.header, 0, None);
                }
                RowKind::Item(fi) => {
                    let Some(it) = self.item(fi) else { continue };
                    let selected = fi == self.sel;
                    if selected {
                        unsafe { ffi::rs_fill_round(dst, x0 as f32, (r.y - off) as f32 + 1.0, (x1 - x0) as f32, r.h as f32 - 2.0, 7.0 * m.s, c.accent) };
                    }
                    let ix = x0 + m.p(8.0);
                    let iy = r.y + (r.h - m.icon) / 2;
                    match it.icon {
                        Icon::Cache(idx) => {
                            if let Some(px) = self.apps.as_ref().and_then(|a| a.icon(idx).map(|p| (p, a.icon_px))) {
                                unsafe { ffi::rs_blit_icon(dst, ix, iy - off, m.icon, px.0.as_ptr(), px.1 as i32) };
                            }
                        }
                        Icon::Glyph(g) => {
                            let bg = if selected { 0x33FF_FFFF } else { c.glyph_bg };
                            unsafe { ffi::rs_fill_round(dst, ix as f32, (iy - off) as f32, m.icon as f32, m.icon as f32, 6.5 * m.s, bg) };
                            let col = if selected { c.on_accent } else { c.glyph };
                            self.text(dst, off, f.glyph, &[g], ix, iy, m.icon, m.icon, col, DT_CENTER, None);
                        }
                    }
                    let tx = ix + m.icon + m.p(10.0);
                    let rx = x1 - m.p(12.0);
                    let dw = if it.detail.is_empty() { 0 } else { self.measure(f.detail, &it.detail).min(((rx - tx) as f32 * 0.45) as i32) };
                    let gap = if dw > 0 { m.p(16.0) } else { 0 };
                    let (tc, dc) = if selected { (c.on_accent, c.on_accent_muted) } else { (c.text, c.muted) };
                    self.text(dst, off, f.title, &it.title, tx, r.y, rx - dw - gap - tx, r.h, tc, DT_END_ELLIPSIS, None);
                    if dw > 0 {
                        self.text(dst, off, f.detail, &it.detail, rx - dw, r.y, dw, r.h, dc, DT_RIGHT | DT_END_ELLIPSIS, None);
                    }
                }
            }
        }
    }

    // ---------------- show / hide

    fn show(&mut self) {
        if self.visible {
            unsafe { SetForegroundWindow(self.hwnd) };
            return;
        }
        unsafe {
            KillTimer(self.hwnd, TIMER_TRIM);
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            GetMonitorInfoW(mon, &mut mi);
            let (mut dx, mut dy) = (96u32, 96u32);
            if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) < 0 {
                dx = 96;
            }
            self.set_dpi(dx);
            self.apply_theme();

            // Spotlight keeps the last query, selected, so typing replaces it.
            self.anchor = 0;
            self.caret = self.text.len();
            self.scroll = 0;
            self.caret_on = true;
            self.refresh(true);

            let wa = mi.rcWork;
            let x = wa.left + ((wa.right - wa.left) - self.m.w) / 2;
            let y = wa.top + ((wa.bottom - wa.top) as f32 * 0.22) as i32;
            self.win_pos = (x, y);
            SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, self.m.w, self.win_h, SWP_NOACTIVATE);
            self.visible = true;
            ShowWindow(self.hwnd, SW_SHOW);
            SetForegroundWindow(self.hwnd);
            if GetForegroundWindow() != self.hwnd {
                // Windows only hands focus to the process that produced the
                // last input; a no-op key press makes that us.
                crate::hotkey::tap_noop_key();
                SetForegroundWindow(self.hwnd);
            }
            SetFocus(self.hwnd);
            self.redraw();
            SetTimer(self.hwnd, TIMER_CARET, GetCaretBlinkTime().clamp(200, 2000), None);
            let mut cur = POINT { x: 0, y: 0 };
            GetCursorPos(&mut cur);
            self.last_mouse = (cur.x - x, cur.y - y);
        }
        self.indexer.poke();
        self.maybe_refresh_apps(false);
    }

    fn hide(&mut self) {
        if !self.visible {
            return;
        }
        self.visible = false;
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
            KillTimer(self.hwnd, TIMER_CARET);
            if self.drag_text {
                ReleaseCapture();
                self.drag_text = false;
            }
        }
        self.strip = None;
        self.mask = None;
        self.sections = Vec::new();
        self.flat = Vec::new();
        self.rows = Vec::new();
        if self.cfg.trim_memory {
            unsafe { SetTimer(self.hwnd, TIMER_TRIM, 3000, None) };
        }
    }

    fn trim(&mut self) {
        unsafe {
            KillTimer(self.hwnd, TIMER_TRIM);
            if self.visible {
                return;
            }
            HeapCompact(GetProcessHeap(), 0);
            SetProcessWorkingSetSizeEx(GetCurrentProcess(), usize::MAX, usize::MAX, 0);
        }
    }

    // ---------------- apps cache helper process

    fn maybe_refresh_apps(&mut self, force: bool) {
        let stale = crate::util::now_secs().saturating_sub(self.apps_refreshed) > 3600;
        let wrong_size = self.apps.as_ref().map_or(true, |a| a.icon_px != self.icon_px());
        let dirty = self.indexer.apps_dirty.load(Ordering::Relaxed);
        if !(force || stale || wrong_size || dirty) || self.helper_running {
            return;
        }
        let mut exts: Vec<String> = apps::BASE_EXTS.iter().map(|s| s.to_string()).collect();
        if let Some(fi) = self.indexer.get() {
            for e in &fi.top_exts {
                if !exts.contains(e) {
                    exts.push(e.clone());
                }
            }
        }
        let out = apps::new_cache_path();
        let child = std::process::Command::new(&self.exe)
            .arg("--apps")
            .arg(self.icon_px().to_string())
            .arg(&out)
            .arg(exts.join("|"))
            .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
            .spawn();
        if let Ok(mut child) = child {
            self.helper_running = true;
            self.apps_refreshed = crate::util::now_secs();
            self.indexer.apps_dirty.store(false, Ordering::Relaxed);
            let hwnd = self.hwnd as isize;
            let _ = std::thread::Builder::new().stack_size(64 * 1024).spawn(move || {
                let ok = child.wait().map(|s| s.success()).unwrap_or(false);
                unsafe { PostMessageW(hwnd as HWND, WM_APP_APPS_READY, ok as usize, 0) };
            });
        }
    }

    fn load_newest_apps(&mut self) {
        if let Some(p) = apps::newest_cache() {
            if self.apps.as_ref().map_or(false, |a| a.path == p) {
                return;
            }
            if let Some(c) = AppsCache::open(&p) {
                self.apps = Some(c);
                apps::prune_caches(&p);
            }
        }
    }

    // ---------------- actions

    fn spawn_open(&self, target: &[u16], verb: &str) {
        unsafe { AllowSetForegroundWindow(ASFW_ANY) };
        let _ = std::process::Command::new(&self.exe)
            .arg("--open")
            .arg(verb)
            .arg(OsString::from_wide(target))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }

    fn activate(&mut self, flat: usize) {
        let ctrl = key_down(VK_CONTROL);
        let shift = key_down(VK_SHIFT);
        let Some(it) = self.item(flat) else { return };
        let (kind, target) = (it.kind, it.target.clone());
        match kind {
            Kind::Calc => {
                set_clipboard(self.hwnd, &target);
                self.hide();
            }
            Kind::Quit => unsafe {
                DestroyWindow(self.hwnd);
            },
            Kind::Command => {
                self.hide();
                self.run_command(&String::from_utf16_lossy(&target));
            }
            Kind::App | Kind::File | Kind::Folder | Kind::Web => {
                if ctrl && !shift && matches!(kind, Kind::File | Kind::Folder) {
                    unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                    let _ = std::process::Command::new("explorer.exe")
                        .raw_arg(format!("/select,\"{}\"", String::from_utf16_lossy(&target)))
                        .spawn();
                } else {
                    self.spawn_open(&target, if ctrl && shift { "runas" } else { "" });
                }
                self.history.bump(&target);
                self.hide();
            }
        }
    }

    fn run_command(&mut self, id: &str) {
        match id {
            "settings" => {
                let p = crate::util::roaming_dir().join("config.ini");
                let w: Vec<u16> = p.to_string_lossy().encode_utf16().collect();
                self.spawn_open(&w, "");
            }
            "reindex" => {
                self.indexer.dirty.store(true, Ordering::Relaxed);
                self.indexer.poke();
                self.maybe_refresh_apps(true);
            }
            "autostart-on" | "autostart-off" => {
                let on = id == "autostart-on";
                // schtasks takes ~100 ms; keep it off the UI thread
                let _ = std::thread::Builder::new().stack_size(64 * 1024).spawn(move || crate::autostart::set(on));
            }
            _ => {}
        }
    }

    // ---------------- text editing

    fn text_changed(&mut self) {
        self.caret_on = true;
        unsafe { SetTimer(self.hwnd, TIMER_CARET, GetCaretBlinkTime().clamp(200, 2000), None) };
        self.refresh(true);
        self.fix_scroll();
        self.redraw();
    }

    fn caret_moved(&mut self) {
        self.caret_on = true;
        unsafe { SetTimer(self.hwnd, TIMER_CARET, GetCaretBlinkTime().clamp(200, 2000), None) };
        self.fix_scroll();
        self.redraw_range(0, self.m.input_h);
    }

    fn fix_scroll(&mut self) {
        let avail = self.m.w - self.m.p(18.0) - self.m.text_x - self.m.p(4.0);
        let stops = self.caret_stops();
        let cx = stops[self.caret];
        let tw = *stops.last().unwrap();
        if cx - self.scroll > avail {
            self.scroll = cx - avail;
        }
        if cx < self.scroll {
            self.scroll = cx;
        }
        if tw - self.scroll < avail {
            self.scroll = (tw - avail).max(0);
        }
    }

    fn sel_range(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    fn delete_sel(&mut self) -> bool {
        let (a, b) = self.sel_range();
        if a == b {
            return false;
        }
        self.text.drain(a..b);
        self.caret = a;
        self.anchor = a;
        true
    }

    fn insert(&mut self, s: &[u16]) {
        self.delete_sel();
        let room = 512usize.saturating_sub(self.text.len());
        let s = &s[..s.len().min(room)];
        self.text.splice(self.caret..self.caret, s.iter().copied());
        self.caret += s.len();
        self.anchor = self.caret;
        self.text_changed();
    }

    fn prev_pos(&self, p: usize) -> usize {
        if p >= 2 && is_lo(self.text[p - 1]) && is_hi(self.text[p - 2]) { p - 2 } else { p.saturating_sub(1) }
    }

    fn next_pos(&self, p: usize) -> usize {
        let n = self.text.len();
        if p + 2 <= n && is_hi(self.text[p]) && is_lo(self.text[p + 1]) { p + 2 } else { (p + 1).min(n) }
    }

    fn word_left(&self, mut p: usize) -> usize {
        while p > 0 && !is_word(self.text[p - 1]) {
            p -= 1;
        }
        while p > 0 && is_word(self.text[p - 1]) {
            p -= 1;
        }
        p
    }

    fn word_right(&self, mut p: usize) -> usize {
        let n = self.text.len();
        while p < n && is_word(self.text[p]) {
            p += 1;
        }
        while p < n && !is_word(self.text[p]) {
            p += 1;
        }
        p
    }

    fn move_caret(&mut self, to: usize, extend: bool) {
        self.caret = to.min(self.text.len());
        if !extend {
            self.anchor = self.caret;
        }
        self.caret_moved();
    }

    fn move_sel(&mut self, d: i32) {
        if self.flat.is_empty() {
            return;
        }
        let n = self.flat.len() as i32;
        let s = (self.sel as i32 + d).clamp(0, n - 1) as usize;
        if s != self.sel {
            self.sel = s;
            self.redraw_range(self.m.input_h, i32::MAX);
        }
    }

    fn jump_section(&mut self, d: i32) {
        let Some(&(cur, _)) = self.flat.get(self.sel) else { return };
        let target = if d > 0 { self.flat.iter().position(|&(s, _)| s > cur) } else { self.flat.iter().position(|&(s, _)| s + 1 == cur) };
        if let Some(t) = target.or(if d < 0 { Some(0) } else { None }) {
            self.sel = t;
            self.redraw_range(self.m.input_h, i32::MAX);
        }
    }

    fn on_key(&mut self, vk: u16) -> bool {
        let ctrl = key_down(VK_CONTROL);
        let shift = key_down(VK_SHIFT);
        let has_sel = self.caret != self.anchor;
        match vk {
            VK_ESCAPE => {
                if self.text.is_empty() {
                    self.hide();
                } else {
                    self.text.clear();
                    self.caret = 0;
                    self.anchor = 0;
                    self.text_changed();
                }
            }
            VK_RETURN => self.activate(self.sel),
            VK_UP if ctrl => self.jump_section(-1),
            VK_DOWN if ctrl => self.jump_section(1),
            VK_UP => self.move_sel(-1),
            VK_DOWN => self.move_sel(1),
            VK_TAB => self.move_sel(if shift { -1 } else { 1 }),
            VK_PRIOR => self.move_sel(-100),
            VK_NEXT => self.move_sel(100),
            VK_LEFT => {
                let to = if has_sel && !shift { self.sel_range().0 } else if ctrl { self.word_left(self.caret) } else { self.prev_pos(self.caret) };
                self.move_caret(to, shift);
            }
            VK_RIGHT => {
                let to = if has_sel && !shift { self.sel_range().1 } else if ctrl { self.word_right(self.caret) } else { self.next_pos(self.caret) };
                self.move_caret(to, shift);
            }
            VK_HOME => self.move_caret(0, shift),
            VK_END => self.move_caret(self.text.len(), shift),
            VK_BACK => {
                if !self.delete_sel() && self.caret > 0 {
                    let from = if ctrl { self.word_left(self.caret) } else { self.prev_pos(self.caret) };
                    self.text.drain(from..self.caret);
                    self.caret = from;
                    self.anchor = from;
                } else if !has_sel {
                    return true;
                }
                self.text_changed();
            }
            VK_DELETE => {
                if !self.delete_sel() && self.caret < self.text.len() {
                    let to = if ctrl { self.word_right(self.caret) } else { self.next_pos(self.caret) };
                    self.text.drain(self.caret..to);
                    self.anchor = self.caret;
                } else if !has_sel {
                    return true;
                }
                self.text_changed();
            }
            k if ctrl && k == b'A' as u16 => {
                self.anchor = 0;
                self.caret = self.text.len();
                self.caret_moved();
            }
            k if ctrl && (k == b'C' as u16 || k == b'X' as u16) => {
                if has_sel {
                    let (a, b) = self.sel_range();
                    set_clipboard(self.hwnd, &self.text[a..b]);
                    if k == b'X' as u16 {
                        self.delete_sel();
                        self.text_changed();
                    }
                } else if let Some(it) = self.item(self.sel) {
                    if matches!(it.kind, Kind::File | Kind::Folder | Kind::Calc | Kind::Web) {
                        set_clipboard(self.hwnd, &it.target.clone());
                    }
                }
            }
            k if ctrl && k == b'V' as u16 => {
                if let Some(mut s) = get_clipboard(self.hwnd) {
                    for c in s.iter_mut() {
                        if *c < 0x20 {
                            *c = b' ' as u16;
                        }
                    }
                    self.insert(&s);
                }
            }
            _ => return false,
        }
        true
    }

    // ---------------- mouse

    fn row_at(&self, y: i32) -> Option<usize> {
        self.rows.iter().find(|r| y >= r.y && y < r.y + r.h).and_then(|r| match r.kind {
            RowKind::Item(i) => Some(i),
            _ => None,
        })
    }

    fn caret_from_x(&self, x: i32) -> usize {
        let rel = x - self.m.text_x + self.scroll;
        let stops = self.caret_stops();
        let mut best = 0;
        for (i, &s) in stops.iter().enumerate() {
            if (s - rel).abs() < (stops[best] - rel).abs() {
                best = i;
            }
        }
        best
    }

    // ---------------- window procedure

    unsafe fn handle(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            WM_HOTKEY => {
                if self.visible { self.hide() } else { self.show() }
                0
            }
            WM_APP_SHOW => {
                self.show();
                0
            }
            WM_APP_QUIT => {
                DestroyWindow(hwnd);
                0
            }
            WM_APP_INDEXED => {
                if !self.indexed_once {
                    self.indexed_once = true;
                    // first scan done: refresh the app/icon cache in the background
                    self.maybe_refresh_apps(true);
                }
                if self.visible {
                    let keep = self.sel;
                    self.refresh(false);
                    self.sel = keep.min(self.flat.len().saturating_sub(1));
                    self.redraw();
                }
                0
            }
            WM_APP_APPS_READY => {
                self.helper_running = false;
                if wp != 0 {
                    self.load_newest_apps();
                    if self.visible {
                        self.refresh(false);
                        self.redraw();
                    }
                }
                0
            }
            WM_ACTIVATE => {
                if (wp & 0xFFFF) as u32 == WA_INACTIVE && self.visible {
                    self.hide();
                }
                0
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                if self.on_key(wp as u16) {
                    return 0;
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_CHAR => {
                let c = wp as u16;
                if c >= 0x20 && c != 0x7F {
                    self.insert(&[c]);
                }
                0
            }
            WM_SYSCHAR => 0,
            WM_SYSCOMMAND if (wp as u32 & 0xFFF0) == SC_KEYMENU => 0,
            WM_MOUSEMOVE => {
                let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
                if self.drag_text {
                    let p = self.caret_from_x(x);
                    if p != self.caret {
                        self.caret = p;
                        self.caret_moved();
                    }
                } else if (x, y) != self.last_mouse {
                    if let Some(i) = self.row_at(y) {
                        if i != self.sel {
                            self.sel = i;
                            self.redraw_range(self.m.input_h, i32::MAX);
                        }
                    }
                }
                self.last_mouse = (x, y);
                0
            }
            WM_LBUTTONDOWN => {
                let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
                if y < self.m.input_h {
                    let p = self.caret_from_x(x);
                    self.move_caret(p, key_down(VK_SHIFT));
                    self.drag_text = true;
                    SetCapture(hwnd);
                } else if let Some(i) = self.row_at(y) {
                    self.sel = i;
                    self.press_row = Some(i);
                    self.redraw_range(self.m.input_h, i32::MAX);
                }
                0
            }
            WM_LBUTTONUP => {
                if self.drag_text {
                    self.drag_text = false;
                    ReleaseCapture();
                }
                let y = ((lp >> 16) & 0xFFFF) as i16 as i32;
                if let (Some(p), Some(i)) = (self.press_row.take(), self.row_at(y)) {
                    if p == i {
                        self.activate(i);
                    }
                }
                0
            }
            WM_TIMER => {
                match wp {
                    TIMER_CARET => {
                        self.caret_on = !self.caret_on;
                        self.redraw_range(0, self.m.input_h);
                    }
                    TIMER_TRIM => self.trim(),
                    _ => {}
                }
                0
            }
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let dc = BeginPaint(hwnd, &mut ps);
                if self.visible {
                    self.paint(dc, ps.rcPaint.top, ps.rcPaint.bottom);
                }
                EndPaint(hwnd, &ps);
                0
            }
            WM_ERASEBKGND => 1,
            WM_DPICHANGED => {
                self.set_dpi((wp & 0xFFFF) as u32);
                if self.visible {
                    self.layout();
                    let (x, y) = self.win_pos;
                    SetWindowPos(hwnd, null_mut(), x, y, self.m.w, self.win_h, SWP_NOZORDER | SWP_NOACTIVATE);
                    self.redraw();
                }
                0
            }
            WM_CLOSE => {
                self.hide();
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

impl search::Item {
    fn command(title: &str, id: &str, glyph: u16) -> search::Item {
        search::Item::new(Kind::Command, title.encode_utf16().collect(), "Command".encode_utf16().collect(), Icon::Glyph(glyph), id.encode_utf16().collect())
    }
}

fn is_hi(c: u16) -> bool {
    (0xD800..0xDC00).contains(&c)
}
fn is_lo(c: u16) -> bool {
    (0xDC00..0xE000).contains(&c)
}
fn is_word(c: u16) -> bool {
    !(c == b' ' as u16 || c == b'\t' as u16 || (c < 0x80 && (c as u8).is_ascii_punctuation()))
}
fn key_down(vk: u16) -> bool {
    unsafe { GetKeyState(vk as i32) < 0 }
}

fn set_clipboard(hwnd: HWND, s: &[u16]) {
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, (s.len() + 1) * 2);
        if !h.is_null() {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(s.as_ptr(), p, s.len());
                *p.add(s.len()) = 0;
                GlobalUnlock(h);
                SetClipboardData(CF_UNICODETEXT as u32, h as HANDLE);
            }
        }
        CloseClipboard();
    }
}

fn get_clipboard(hwnd: HWND) -> Option<Vec<u16>> {
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return None;
        }
        let mut out = None;
        let h = GetClipboardData(CF_UNICODETEXT as u32);
        if !h.is_null() {
            let p = GlobalLock(h as _) as *const u16;
            if !p.is_null() {
                let mut n = 0;
                while *p.add(n) != 0 && n < 4096 {
                    n += 1;
                }
                out = Some(std::slice::from_raw_parts(p, n).to_vec());
                GlobalUnlock(h as _);
            }
        }
        CloseClipboard();
        out
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ui = UI.load(Ordering::Relaxed);
    if ui.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    if (*ui).trace.is_none() {
        return (*ui).handle(hwnd, msg, wp, lp);
    }
    // Timed path: search + layout + render + BitBlt all happen synchronously
    // inside these handlers, so this is input -> pixels handed to DWM.
    let t = std::time::Instant::now();
    let r = (*ui).handle(hwnd, msg, wp, lp);
    let kind = match msg {
        WM_CHAR => "key",
        WM_HOTKEY | WM_APP_SHOW => "show",
        _ => return r,
    };
    if let Some(f) = (*ui).trace.as_mut() {
        use std::io::Write;
        let _ = writeln!(f, "{kind}	{}	{}", t.elapsed().as_micros(), (*ui).text.len());
    }
    r
}

/// Post `msg` to an already running instance. Returns false if none.
pub fn signal_running(msg: u32) -> bool {
    unsafe {
        let cls = wide(CLASS);
        let h = FindWindowW(cls.as_ptr(), null());
        if h.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, &mut pid);
        AllowSetForegroundWindow(pid);
        PostMessageW(h, msg, 0, 0);
        if msg == WM_APP_QUIT {
            // wait so callers (installer, --uninstall) can replace/delete the exe
            let ph = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if !ph.is_null() {
                WaitForSingleObject(ph, 3000);
                CloseHandle(ph);
            }
        }
        true
    }
}

pub fn run(show_now: bool) {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let mutex_name = wide("Local\\Indicative.Spotlight.Instance");
        let mutex = CreateMutexW(null(), 1, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if show_now {
                signal_running(WM_APP_SHOW);
            }
            return;
        }

        let cfg = Config::load();
        let _ = std::thread::Builder::new().stack_size(64 * 1024).spawn(crate::autostart::migrate_legacy);
        let hinst = GetModuleHandleW(null());
        let cls = wide(CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: cls.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc);
        let title = wide("Indicative");
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST, cls.as_ptr(), title.as_ptr(), WS_POPUP, 0, 0, 680, 58, null_mut(), null_mut(), hinst, null());
        if hwnd.is_null() {
            return;
        }

        // DWM: rounded corners, acrylic backdrop, no fade animation.
        let round: i32 = DWMWCP_ROUND;
        DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as u32, &round as *const i32 as _, 4);
        let no_anim: i32 = 1;
        DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED as u32, &no_anim as *const i32 as _, 4);
        let bd: i32 = DWMSBT_TRANSIENTWINDOW;
        let backdrop = DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE as u32, &bd as *const i32 as _, 4) >= 0;
        let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
        DwmExtendFrameIntoClientArea(hwnd, &margins);

        let indexer = index::spawn(hwnd as isize, WM_APP_INDEXED, cfg.extra_roots.clone(), cfg.index_user_folders, cfg.max_files);

        let ui = Box::new(Ui {
            hwnd,
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("indicative.exe")),
            mdc: CreateCompatibleDC(null_mut()),
            dpi: 0,
            m: Metrics::new(96),
            fonts: None,
            strip: None,
            mask: None,
            dark: true,
            backdrop,
            c: colors(true, backdrop),
            text: Vec::new(),
            caret: 0,
            anchor: 0,
            scroll: 0,
            caret_on: true,
            sections: Vec::new(),
            flat: Vec::new(),
            rows: Vec::new(),
            sel: 0,
            win_h: 0,
            win_pos: (0, 0),
            visible: false,
            last_mouse: (-1, -1),
            drag_text: false,
            press_row: None,
            apps: None,
            indexer,
            history: History::load(),
            helper_running: false,
            apps_refreshed: 0,
            indexed_once: false,
            trace: std::env::var_os("INDICATIVE_TRACE").and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()),
            cfg,
        });
        let ui = Box::into_raw(ui);
        UI.store(ui, Ordering::Relaxed);
        (*ui).set_dpi(GetDpiForWindow(hwnd).max(96));
        (*ui).load_newest_apps();
        if (*ui).apps.is_none() {
            (*ui).maybe_refresh_apps(true);
        }

        if !crate::hotkey::install(hwnd, (*ui).cfg.hotkey_mods, (*ui).cfg.hotkey_vk) {
            let msg = wide(&format!(
                "Indicative couldn't register {} (another app is using it).\n\nChange `hotkey` in:\n{}\n\nYou can still open Indicative by running it again.",
                (*ui).cfg.hotkey_label,
                crate::util::roaming_dir().join("config.ini").display()
            ));
            MessageBoxW(null_mut(), msg.as_ptr(), title.as_ptr(), MB_ICONWARNING | MB_OK);
        }

        if show_now {
            (*ui).show();
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        UnregisterHotKey(hwnd, 1);
        UI.store(null_mut(), Ordering::Relaxed);
        drop(Box::from_raw(ui));
        CloseHandle(mutex);
    }
}

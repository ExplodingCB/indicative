//! Bindings to the C core in `csrc/`.

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RsSurface {
    pub px: *mut u32,
    pub w: i32,
    pub h: i32,
    pub stride: i32,
}

extern "C" {
    pub fn fz_score_query(q: *const u8, qlen: i32, s: *const u8, slen: i32, allow_fuzzy: i32) -> i32;

    pub fn calc_eval(expr: *const u16, len: i32, out: *mut f64) -> i32;
    pub fn calc_format(v: f64, buf: *mut u16, cap: i32) -> i32;

    pub fn rs_clear(s: *mut RsSurface, argb: u32);
    pub fn rs_fill_rect(s: *mut RsSurface, x: i32, y: i32, w: i32, h: i32, argb: u32);
    pub fn rs_fill_round(s: *mut RsSurface, x: f32, y: f32, w: f32, h: f32, r: f32, argb: u32);
    pub fn rs_zero_rect(s: *mut RsSurface, x: i32, y: i32, w: i32, h: i32);
    pub fn rs_mask_blend(dst: *mut RsSurface, mask: *const RsSurface, x: i32, y: i32, w: i32, h: i32, argb: u32);
    pub fn rs_blit_icon(dst: *mut RsSurface, dx: i32, dy: i32, dsize: i32, src: *const u32, ssize: i32);

    pub fn sh_build_apps_cache(out_path: *const u16, icon_px: i32, exts: *const u16) -> i32;
    pub fn sh_open(target: *const u16, verb: *const u16) -> i32;
}

#[inline]
pub fn score(q: &[u8], s: &[u8], fuzzy: bool) -> i32 {
    unsafe { fz_score_query(q.as_ptr(), q.len() as i32, s.as_ptr(), s.len() as i32, fuzzy as i32) }
}

pub fn calc(expr: &[u16]) -> Option<(f64, Vec<u16>)> {
    let mut v = 0.0;
    if unsafe { calc_eval(expr.as_ptr(), expr.len() as i32, &mut v) } == 0 {
        return None;
    }
    let mut buf = [0u16; 64];
    let n = unsafe { calc_format(v, buf.as_mut_ptr(), buf.len() as i32) };
    Some((v, buf[..n as usize].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(q: &str, name: &str, fuzzy: bool) -> i32 {
        score(q.as_bytes(), name.as_bytes(), fuzzy)
    }

    #[test]
    fn match_tiers_are_ordered() {
        let exact = s("code", "Code", false);
        let prefix = s("code", "Codex", false);
        let word = s("code", "Visual Studio Code", false);
        let acronym = s("vsc", "Visual Studio Code", false);
        let sub = s("ode", "Encoder", false);
        assert!(exact > prefix && prefix > word && word > acronym && acronym > sub && sub > 0);
    }

    #[test]
    fn fuzzy_needs_word_start() {
        assert_eq!(s("code", "Sound Recorder", true), 0);
        assert!(s("chrme", "Google Chrome", true) > 0);
        assert_eq!(s("chrme", "Google Chrome", false), 0);
    }

    #[test]
    fn multi_word_and_latin1_case() {
        assert!(s("vis code", "Visual Studio Code", false) > 0);
        // capital E-acute folds to e-acute, so case doesn't change the score
        let upper = s("\u{e9}t\u{e9}", "\u{c9}t\u{e9} 2024.pdf", false);
        assert!(upper > 8000);
        assert_eq!(upper, s("\u{e9}t\u{e9}", "\u{e9}t\u{e9} 2024.pdf", false));
    }

    fn c(e: &str) -> Option<String> {
        let w: Vec<u16> = e.encode_utf16().collect();
        calc(&w).map(|(_, r)| String::from_utf16_lossy(&r))
    }

    #[test]
    fn calculator() {
        assert_eq!(c("2^10/4+sqrt(16)").as_deref(), Some("260"));
        assert_eq!(c("2*(3+4)").as_deref(), Some("14"));
        assert_eq!(c("1/3").as_deref(), Some("0.3333333333"));
        assert_eq!(c("5!").as_deref(), Some("120"));
        assert_eq!(c("1,000 * 3").as_deref(), Some("3000"));
        assert_eq!(c("2pi").as_deref(), Some("6.283185307"));
        assert_eq!(c("42"), None); // a bare number is not a calculation
        assert_eq!(c("discord"), None);
        assert_eq!(c("1/0"), None);
    }
}

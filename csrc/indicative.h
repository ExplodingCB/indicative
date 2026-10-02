/* indicative.h - C core shared with the Rust front end.
 *
 * Everything here is allocation-free and CRT-light so the resident
 * process stays tiny. The shell helper (sh_*) is the only part that
 * touches COM / shell32, and it only ever runs in a short-lived child
 * process; it resolves those DLLs at runtime so the main exe never
 * imports them.
 */
#ifndef INDICATIVE_H
#define INDICATIVE_H

#include <stdint.h>

/* ---------- fuzzy.c ---------- */

/* Score `s` (UTF-8, original case) against `q` (UTF-8, already lowercase).
 * Returns 0 for no match, larger is better:
 *   10000 exact, 9000 prefix, 8000 word-prefix, 7000 acronym,
 *   6000 substring, 1..4999 fuzzy (only when allow_fuzzy). */
int32_t fz_score(const uint8_t *q, int32_t qlen,
                 const uint8_t *s, int32_t slen, int32_t allow_fuzzy);

/* Multi-word aware wrapper: "vis code" matches "Visual Studio Code". */
int32_t fz_score_query(const uint8_t *q, int32_t qlen,
                       const uint8_t *s, int32_t slen, int32_t allow_fuzzy);

/* ---------- calc.c ---------- */

/* Evaluate an arithmetic expression. Returns 1 and writes *out when `expr`
 * is a complete expression that actually computes something (not a bare
 * number). */
int32_t calc_eval(const uint16_t *expr, int32_t len, double *out);

/* Format a result for display; returns number of UTF-16 units written. */
int32_t calc_format(double v, uint16_t *buf, int32_t cap);

/* ---------- raster.c ---------- */
/* Surfaces are premultiplied BGRA, top-down, stride in pixels.
 * Colors are passed as straight (non-premultiplied) 0xAARRGGBB. */

typedef struct {
    uint32_t *px;
    int32_t w, h, stride;
} rs_surface;

void rs_clear(rs_surface *s, uint32_t argb);
void rs_fill_rect(rs_surface *s, int32_t x, int32_t y, int32_t w, int32_t h, uint32_t argb);
void rs_fill_round(rs_surface *s, float x, float y, float w, float h, float r, uint32_t argb);
/* Zero a rectangle (used to reset the GDI text mask). */
void rs_zero_rect(rs_surface *s, int32_t x, int32_t y, int32_t w, int32_t h);
/* Blend `argb` through a GDI-rendered white-on-black coverage mask. */
void rs_mask_blend(rs_surface *dst, const rs_surface *mask,
                   int32_t x, int32_t y, int32_t w, int32_t h, uint32_t argb);
/* Composite a premultiplied square icon of side `ssize` scaled to `dsize`. */
void rs_blit_icon(rs_surface *dst, int32_t dx, int32_t dy, int32_t dsize,
                  const uint32_t *src, int32_t ssize);

/* ---------- shellhelper.c (child process only) ---------- */

/* Enumerate shell:AppsFolder + file-type icons into a binary cache file.
 * `exts` is a '|' separated list of lowercase extensions without dots. */
int32_t sh_build_apps_cache(const uint16_t *out_path, int32_t icon_px, const uint16_t *exts);

/* ShellExecuteEx wrapper. verb may be NULL. Returns 1 on success. */
int32_t sh_open(const uint16_t *target, const uint16_t *verb);

#endif

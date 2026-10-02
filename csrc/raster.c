/* raster.c - tiny premultiplied-BGRA compositor.
 *
 * The window draws into a small strip buffer that is then BitBlt'ed onto
 * a DWM-backed surface, so every pixel's alpha has to be correct (DWM uses
 * it to blend over the acrylic backdrop). GDI can't do that, so GDI only
 * renders coverage masks and this file does the actual compositing.
 */
#include "indicative.h"
#include <math.h>

static inline uint32_t premul(uint32_t argb, uint32_t cov /*0..255*/) {
    uint32_t a = ((argb >> 24) * cov + 127) / 255;
    uint32_t r = (((argb >> 16) & 255) * a + 127) / 255;
    uint32_t g = (((argb >> 8) & 255) * a + 127) / 255;
    uint32_t b = ((argb & 255) * a + 127) / 255;
    return (a << 24) | (r << 16) | (g << 8) | b;
}

/* src-over, both premultiplied */
static inline uint32_t over(uint32_t d, uint32_t s) {
    uint32_t sa = s >> 24;
    if (sa == 255) return s;
    if (sa == 0 && (s & 0xFFFFFF) == 0) return d;
    uint32_t ia = 255 - sa;
    uint32_t rb = (d & 0x00FF00FF) * ia + 0x00800080;
    uint32_t ag = ((d >> 8) & 0x00FF00FF) * ia + 0x00800080;
    rb = ((rb + ((rb >> 8) & 0x00FF00FF)) >> 8) & 0x00FF00FF;
    ag = ((ag + ((ag >> 8) & 0x00FF00FF))) & 0xFF00FF00;
    return s + (rb | ag);
}

void rs_clear(rs_surface *s, uint32_t argb) {
    uint32_t p = premul(argb, 255);
    for (int y = 0; y < s->h; y++) {
        uint32_t *row = s->px + (size_t)y * s->stride;
        for (int x = 0; x < s->w; x++) row[x] = p;
    }
}

static int clip(const rs_surface *s, int *x, int *y, int *w, int *h) {
    if (*x < 0) { *w += *x; *x = 0; }
    if (*y < 0) { *h += *y; *y = 0; }
    if (*x + *w > s->w) *w = s->w - *x;
    if (*y + *h > s->h) *h = s->h - *y;
    return *w > 0 && *h > 0;
}

void rs_zero_rect(rs_surface *s, int32_t x, int32_t y, int32_t w, int32_t h) {
    if (!clip(s, &x, &y, &w, &h)) return;
    for (int j = 0; j < h; j++) {
        uint32_t *row = s->px + (size_t)(y + j) * s->stride + x;
        for (int i = 0; i < w; i++) row[i] = 0;
    }
}

void rs_fill_rect(rs_surface *s, int32_t x, int32_t y, int32_t w, int32_t h, uint32_t argb) {
    if (!clip(s, &x, &y, &w, &h)) return;
    uint32_t p = premul(argb, 255);
    for (int j = 0; j < h; j++) {
        uint32_t *row = s->px + (size_t)(y + j) * s->stride + x;
        for (int i = 0; i < w; i++) row[i] = over(row[i], p);
    }
}

void rs_fill_round(rs_surface *s, float fx, float fy, float fw, float fh, float r, uint32_t argb) {
    int x0 = (int)floorf(fx), y0 = (int)floorf(fy);
    int x1 = (int)ceilf(fx + fw), y1 = (int)ceilf(fy + fh);
    if (x0 < 0) x0 = 0;
    if (y0 < 0) y0 = 0;
    if (x1 > s->w) x1 = s->w;
    if (y1 > s->h) y1 = s->h;
    float cx = fx + fw * 0.5f, cy = fy + fh * 0.5f;
    float hx = fw * 0.5f - r, hy = fh * 0.5f - r;
    uint32_t solid = premul(argb, 255);
    for (int y = y0; y < y1; y++) {
        uint32_t *row = s->px + (size_t)y * s->stride;
        float py = fabsf(y + 0.5f - cy) - hy;
        for (int x = x0; x < x1; x++) {
            float px = fabsf(x + 0.5f - cx) - hx;
            float d;
            if (px <= 0 && py <= 0) d = (px > py ? px : py) - r;
            else {
                float ax = px > 0 ? px : 0, ay = py > 0 ? py : 0;
                d = sqrtf(ax * ax + ay * ay) - r;
            }
            float cov = 0.5f - d;
            if (cov <= 0) continue;
            row[x] = over(row[x], cov >= 1 ? solid : premul(argb, (uint32_t)(cov * 255.0f + 0.5f)));
        }
    }
}

/* Light text on dark backgrounds reads thin with linear coverage; a mild
 * gamma lift matches the weight of the macOS rendering more closely. */
static uint8_t g_lut[256];
static int g_lut_ready;

static void lut_init(void) {
    for (int i = 0; i < 256; i++) g_lut[i] = (uint8_t)(powf(i / 255.0f, 0.78f) * 255.0f + 0.5f);
    g_lut_ready = 1;
}

void rs_mask_blend(rs_surface *dst, const rs_surface *mask, int32_t x, int32_t y,
                   int32_t w, int32_t h, uint32_t argb) {
    if (!g_lut_ready) lut_init();
    if (!clip(dst, &x, &y, &w, &h)) return;
    if (x + w > mask->w) w = mask->w - x;
    if (y + h > mask->h) h = mask->h - y;
    for (int j = 0; j < h; j++) {
        uint32_t *d = dst->px + (size_t)(y + j) * dst->stride + x;
        const uint32_t *m = mask->px + (size_t)(y + j) * mask->stride + x;
        for (int i = 0; i < w; i++) {
            uint32_t mv = m[i];
            /* grayscale AA: channels are equal, but take the max to be safe */
            uint32_t r = (mv >> 16) & 255, g = (mv >> 8) & 255, b = mv & 255;
            uint32_t c = r > g ? r : g;
            if (b > c) c = b;
            if (!c) continue;
            d[i] = over(d[i], premul(argb, g_lut[c]));
        }
    }
}

void rs_blit_icon(rs_surface *dst, int32_t dx, int32_t dy, int32_t dsize,
                  const uint32_t *src, int32_t ssize) {
    if (!src || dsize <= 0 || ssize <= 0) return;
    if (dsize == ssize) {
        for (int j = 0; j < dsize; j++) {
            int y = dy + j;
            if (y < 0 || y >= dst->h) continue;
            uint32_t *d = dst->px + (size_t)y * dst->stride;
            const uint32_t *s = src + (size_t)j * ssize;
            for (int i = 0; i < dsize; i++) {
                int x = dx + i;
                if (x < 0 || x >= dst->w) continue;
                d[x] = over(d[x], s[i]);
            }
        }
        return;
    }
    /* area-average (down) / bilinear-ish (up) resample in 16.16 fixed point */
    float scale = (float)ssize / (float)dsize;
    for (int j = 0; j < dsize; j++) {
        int y = dy + j;
        if (y < 0 || y >= dst->h) continue;
        float sy0 = j * scale, sy1 = sy0 + scale;
        uint32_t *d = dst->px + (size_t)y * dst->stride;
        for (int i = 0; i < dsize; i++) {
            int x = dx + i;
            if (x < 0 || x >= dst->w) continue;
            float sx0 = i * scale, sx1 = sx0 + scale;
            float acc[4] = {0, 0, 0, 0}, wsum = 0;
            int iy0 = (int)sy0, iy1 = (int)ceilf(sy1);
            int ix0 = (int)sx0, ix1 = (int)ceilf(sx1);
            if (iy1 > ssize) iy1 = ssize;
            if (ix1 > ssize) ix1 = ssize;
            for (int sy = iy0; sy < iy1; sy++) {
                float wy = fminf(sy + 1.0f, sy1) - fmaxf((float)sy, sy0);
                if (wy <= 0) continue;
                for (int sx = ix0; sx < ix1; sx++) {
                    float wx = fminf(sx + 1.0f, sx1) - fmaxf((float)sx, sx0);
                    if (wx <= 0) continue;
                    float wgt = wx * wy;
                    uint32_t p = src[(size_t)sy * ssize + sx];
                    acc[0] += (p & 255) * wgt;
                    acc[1] += ((p >> 8) & 255) * wgt;
                    acc[2] += ((p >> 16) & 255) * wgt;
                    acc[3] += (p >> 24) * wgt;
                    wsum += wgt;
                }
            }
            if (wsum <= 0) continue;
            uint32_t b = (uint32_t)(acc[0] / wsum + 0.5f), g = (uint32_t)(acc[1] / wsum + 0.5f);
            uint32_t r = (uint32_t)(acc[2] / wsum + 0.5f), a = (uint32_t)(acc[3] / wsum + 0.5f);
            d[x] = over(d[x], (a << 24) | (r << 16) | (g << 8) | b);
        }
    }
}

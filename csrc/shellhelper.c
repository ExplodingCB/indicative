/* shellhelper.c - everything that needs COM / shell32.
 *
 * This code only ever runs inside a short-lived child process
 * (`indicative.exe --apps ...` / `--open ...`). shell32 + ole32 are
 * resolved with LoadLibrary so they never appear in the exe's import
 * table and are never mapped into the resident launcher process.
 *
 * Cache file layout (little endian), read by src/apps.rs:
 *   u32 magic 'IDXA', u32 version, u32 icon_px, u32 n_apps, u32 n_exts,
 *   u32 strings_off, u32 icons_off, u32 n_icons
 *   n_apps * { u32 name_off, u32 name_len, u32 launch_off, u32 launch_len, u32 icon }
 *   n_exts * { u32 ext_off, u32 ext_len, u32 icon }
 *   UTF-16 string pool (offsets/lengths in u16 units)
 *   n_icons * icon_px*icon_px premultiplied BGRA
 */
#define COBJMACROS
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#include <objbase.h>
#include <shobjidl.h>
#include <shellapi.h>
#include <stdlib.h>
#include <string.h>
#include "indicative.h"

#define CACHE_MAGIC 0x41584449u /* 'IDXA' */
#define CACHE_VERSION 1u
#define NO_ICON 0xFFFFFFFFu

/* Private GUID copies so we don't depend on libuuid symbol availability. */
static const GUID kFOLDERID_AppsFolder = {0x1e87508d, 0x89c2, 0x42f0, {0x8a, 0x7e, 0x64, 0x5a, 0x0f, 0x50, 0xca, 0x58}};
static const IID kIID_IShellItem = {0x43826d1e, 0xe718, 0x42ee, {0xbc, 0x55, 0xa1, 0xe2, 0x61, 0xc3, 0x7b, 0xfe}};
static const GUID kBHID_EnumItems = {0x94f60519, 0x2850, 0x4924, {0xaa, 0x5a, 0xd1, 0x5e, 0x84, 0x86, 0x80, 0x39}};
static const IID kIID_IEnumShellItems = {0x70629033, 0xe363, 0x4a28, {0xa5, 0x67, 0x0d, 0xb7, 0x80, 0x06, 0xe6, 0xd7}};
static const IID kIID_IShellItemImageFactory = {0xbcc18b79, 0xba16, 0x442f, {0x80, 0xc4, 0x8a, 0x59, 0xc3, 0x0c, 0x46, 0x3b}};
static const IID kIID_IImageList = {0x46eb5926, 0x582e, 0x4017, {0x9f, 0xdf, 0xe8, 0x99, 0x8d, 0xaa, 0x09, 0x50}};

/* Minimal IImageList vtable (commoncontrols.h order). */
typedef struct ImgList ImgList;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(ImgList *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(ImgList *);
    ULONG(STDMETHODCALLTYPE *Release)(ImgList *);
    void *Add, *ReplaceIcon, *SetOverlayImage, *Replace, *AddMasked, *Draw, *Remove;
    HRESULT(STDMETHODCALLTYPE *GetIcon)(ImgList *, int, UINT, HICON *);
    void *GetImageInfo, *Copy, *Merge, *Clone, *GetImageRect;
    HRESULT(STDMETHODCALLTYPE *GetIconSize)(ImgList *, int *, int *);
} ImgListVtbl;
struct ImgList { const ImgListVtbl *lpVtbl; };

typedef HRESULT(WINAPI *PFN_CoInitializeEx)(LPVOID, DWORD);
typedef void(WINAPI *PFN_CoUninitialize)(void);
typedef void(WINAPI *PFN_CoTaskMemFree)(LPVOID);
typedef HRESULT(WINAPI *PFN_SHGetKnownFolderItem)(REFKNOWNFOLDERID, DWORD, HANDLE, REFIID, void **);
typedef DWORD_PTR(WINAPI *PFN_SHGetFileInfoW)(LPCWSTR, DWORD, SHFILEINFOW *, UINT, UINT);
typedef HRESULT(WINAPI *PFN_SHGetImageList)(int, REFIID, void **);
typedef BOOL(WINAPI *PFN_ShellExecuteExW)(SHELLEXECUTEINFOW *);

static struct {
    PFN_CoInitializeEx CoInitializeEx;
    PFN_CoUninitialize CoUninitialize;
    PFN_CoTaskMemFree CoTaskMemFree;
    PFN_SHGetKnownFolderItem SHGetKnownFolderItem;
    PFN_SHGetFileInfoW SHGetFileInfoW;
    PFN_SHGetImageList SHGetImageList;
    PFN_ShellExecuteExW ShellExecuteExW;
} api;

static int load_api(void) {
    HMODULE ole = LoadLibraryW(L"ole32.dll");
    HMODULE sh = LoadLibraryW(L"shell32.dll");
    if (!ole || !sh) return 0;
    api.CoInitializeEx = (PFN_CoInitializeEx)(void *)GetProcAddress(ole, "CoInitializeEx");
    api.CoUninitialize = (PFN_CoUninitialize)(void *)GetProcAddress(ole, "CoUninitialize");
    api.CoTaskMemFree = (PFN_CoTaskMemFree)(void *)GetProcAddress(ole, "CoTaskMemFree");
    api.SHGetKnownFolderItem = (PFN_SHGetKnownFolderItem)(void *)GetProcAddress(sh, "SHGetKnownFolderItem");
    api.SHGetFileInfoW = (PFN_SHGetFileInfoW)(void *)GetProcAddress(sh, "SHGetFileInfoW");
    api.SHGetImageList = (PFN_SHGetImageList)(void *)GetProcAddress(sh, "SHGetImageList");
    if (!api.SHGetImageList) api.SHGetImageList = (PFN_SHGetImageList)(void *)GetProcAddress(sh, MAKEINTRESOURCEA(727));
    api.ShellExecuteExW = (PFN_ShellExecuteExW)(void *)GetProcAddress(sh, "ShellExecuteExW");
    return api.CoInitializeEx && api.CoUninitialize && api.CoTaskMemFree;
}

/* ---------------- growable buffers ---------------- */

typedef struct { uint8_t *p; size_t len, cap; } Buf;

static int buf_push(Buf *b, const void *data, size_t n) {
    if (b->len + n > b->cap) {
        size_t nc = b->cap ? b->cap * 2 : 4096;
        while (nc < b->len + n) nc *= 2;
        uint8_t *np = (uint8_t *)realloc(b->p, nc);
        if (!np) return 0;
        b->p = np;
        b->cap = nc;
    }
    memcpy(b->p + b->len, data, n);
    b->len += n;
    return 1;
}

static uint32_t str_push(Buf *pool, const wchar_t *s, uint32_t *len_out) {
    uint32_t off = (uint32_t)(pool->len / 2);
    uint32_t n = (uint32_t)wcslen(s);
    buf_push(pool, s, n * 2);
    *len_out = n;
    return off;
}

/* ---------------- bitmap -> premultiplied BGRA ---------------- */

static void resample(const uint32_t *src, int sw, int sh, uint32_t *dst, int size) {
    /* fit (keep aspect), centered, area-average */
    float scale = (float)(sw > sh ? sw : sh) / (float)size;
    int ow = (int)(sw / scale + 0.5f), oh = (int)(sh / scale + 0.5f);
    int ox = (size - ow) / 2, oy = (size - oh) / 2;
    memset(dst, 0, (size_t)size * size * 4);
    for (int j = 0; j < oh; j++) {
        float sy0 = j * scale, sy1 = sy0 + scale;
        for (int i = 0; i < ow; i++) {
            float sx0 = i * scale, sx1 = sx0 + scale;
            float acc[4] = {0, 0, 0, 0}, wsum = 0;
            for (int sy = (int)sy0; sy < (int)(sy1 + 0.999f) && sy < sh; sy++) {
                float wy = (sy + 1 < sy1 ? sy + 1 : sy1) - (sy > sy0 ? sy : sy0);
                if (wy <= 0) continue;
                for (int sx = (int)sx0; sx < (int)(sx1 + 0.999f) && sx < sw; sx++) {
                    float wx = (sx + 1 < sx1 ? sx + 1 : sx1) - (sx > sx0 ? sx : sx0);
                    if (wx <= 0) continue;
                    uint32_t p = src[(size_t)sy * sw + sx];
                    float w = wx * wy;
                    acc[0] += (p & 255) * w;
                    acc[1] += ((p >> 8) & 255) * w;
                    acc[2] += ((p >> 16) & 255) * w;
                    acc[3] += (p >> 24) * w;
                    wsum += w;
                }
            }
            if (wsum <= 0) continue;
            uint32_t b = (uint32_t)(acc[0] / wsum + .5f), g = (uint32_t)(acc[1] / wsum + .5f);
            uint32_t r = (uint32_t)(acc[2] / wsum + .5f), a = (uint32_t)(acc[3] / wsum + .5f);
            dst[(size_t)(oy + j) * size + ox + i] = (a << 24) | (r << 16) | (g << 8) | b;
        }
    }
}

/* Normalize alpha: shell bitmaps are usually premultiplied, legacy icons
 * may be straight alpha or have no alpha at all. */
static void fix_alpha(uint32_t *px, size_t n, const uint32_t *mask_px) {
    int any_alpha = 0, straight = 0;
    for (size_t i = 0; i < n; i++) {
        uint32_t a = px[i] >> 24;
        if (a) any_alpha = 1;
        if (((px[i] >> 16) & 255) > a || ((px[i] >> 8) & 255) > a || (px[i] & 255) > a) straight = 1;
    }
    if (!any_alpha) {
        for (size_t i = 0; i < n; i++) {
            int opaque = mask_px ? ((mask_px[i] & 0xFFFFFF) == 0) : ((px[i] & 0xFFFFFF) != 0);
            px[i] = opaque ? (px[i] | 0xFF000000u) : 0;
        }
        return;
    }
    if (straight) {
        for (size_t i = 0; i < n; i++) {
            uint32_t a = px[i] >> 24;
            uint32_t r = (((px[i] >> 16) & 255) * a + 127) / 255;
            uint32_t g = (((px[i] >> 8) & 255) * a + 127) / 255;
            uint32_t b = ((px[i] & 255) * a + 127) / 255;
            px[i] = (a << 24) | (r << 16) | (g << 8) | b;
        }
    }
}

static uint32_t *bitmap_pixels(HBITMAP hbm, int *w, int *h) {
    BITMAP bm;
    if (!GetObjectW(hbm, sizeof bm, &bm) || bm.bmWidth <= 0 || bm.bmHeight == 0) return NULL;
    int bw = bm.bmWidth, bh = bm.bmHeight < 0 ? -bm.bmHeight : bm.bmHeight;
    BITMAPINFO bi;
    memset(&bi, 0, sizeof bi);
    bi.bmiHeader.biSize = sizeof bi.bmiHeader;
    bi.bmiHeader.biWidth = bw;
    bi.bmiHeader.biHeight = -bh;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;
    uint32_t *px = (uint32_t *)malloc((size_t)bw * bh * 4);
    if (!px) return NULL;
    HDC dc = GetDC(NULL);
    int ok = GetDIBits(dc, hbm, 0, (UINT)bh, px, &bi, DIB_RGB_COLORS);
    ReleaseDC(NULL, dc);
    if (!ok) { free(px); return NULL; }
    *w = bw;
    *h = bh;
    return px;
}

static int push_icon_pixels(Buf *icons, uint32_t *px, int w, int h, int size, uint32_t *idx_out) {
    uint32_t *out = (uint32_t *)malloc((size_t)size * size * 4);
    if (!out) return 0;
    if (w == size && h == size) memcpy(out, px, (size_t)size * size * 4);
    else resample(px, w, h, out, size);
    *idx_out = (uint32_t)(icons->len / ((size_t)size * size * 4));
    int ok = buf_push(icons, out, (size_t)size * size * 4);
    free(out);
    return ok;
}

static int push_hicon(Buf *icons, HICON hi, int size, uint32_t *idx_out) {
    ICONINFO ii;
    if (!GetIconInfo(hi, &ii)) return 0;
    int ok = 0, w = 0, h = 0, mw = 0, mh = 0;
    uint32_t *px = ii.hbmColor ? bitmap_pixels(ii.hbmColor, &w, &h) : NULL;
    uint32_t *mask = ii.hbmMask ? bitmap_pixels(ii.hbmMask, &mw, &mh) : NULL;
    if (px) {
        fix_alpha(px, (size_t)w * h, (mask && mw == w && mh >= h) ? mask : NULL);
        ok = push_icon_pixels(icons, px, w, h, size, idx_out);
    }
    free(px);
    free(mask);
    if (ii.hbmColor) DeleteObject(ii.hbmColor);
    if (ii.hbmMask) DeleteObject(ii.hbmMask);
    return ok;
}

/* ---------------- app enumeration ---------------- */

static int ends_with_i(const wchar_t *s, const wchar_t *suffix) {
    size_t a = wcslen(s), b = wcslen(suffix);
    return a >= b && _wcsicmp(s + a - b, suffix) == 0;
}

static int starts_with_i(const wchar_t *s, const wchar_t *prefix) {
    return _wcsnicmp(s, prefix, wcslen(prefix)) == 0;
}

static int skip_app(const wchar_t *name, const wchar_t *parse) {
    static const wchar_t *doc_ext[] = {L".txt", L".htm", L".html", L".chm", L".pdf", L".rtf",
                                       L".md", L".ini", L".log", L".xml", L".hlp", L".mht"};
    for (unsigned i = 0; i < sizeof doc_ext / sizeof doc_ext[0]; i++)
        if (ends_with_i(parse, doc_ext[i])) return 1;
    if (starts_with_i(name, L"Uninstall") || starts_with_i(name, L"Readme") ||
        starts_with_i(name, L"Read Me") || starts_with_i(name, L"Release Notes"))
        return 1;
    if (starts_with_i(parse, L"http://") || starts_with_i(parse, L"https://")) return 1;
    if (wcsstr(name, L" FAQ") || wcsstr(name, L" Documentation") || wcsstr(name, L" Help") ||
        wcsstr(name, L" Website") || wcsstr(name, L" Manual"))
        return 1;
    /* plain folders pinned in the Start menu are not applications */
    if (parse[0] && parse[1] == L':') {
        DWORD a = GetFileAttributesW(parse);
        if (a != INVALID_FILE_ATTRIBUTES && (a & FILE_ATTRIBUTE_DIRECTORY)) return 1;
    }
    return 0;
}

typedef struct { uint32_t name_off, name_len, launch_off, launch_len, icon; } AppRec;
typedef struct { uint32_t ext_off, ext_len, icon; } ExtRec;

static ImgList *pick_image_list(int icon_px) {
    /* SHIL_SMALL=1(16) SHIL_LARGE=0(32) SHIL_EXTRALARGE=2(48) SHIL_JUMBO=4(256) */
    static const int order[] = {1, 0, 2, 4};
    ImgList *best = NULL;
    for (unsigned i = 0; i < 4; i++) {
        ImgList *il = NULL;
        if (FAILED(api.SHGetImageList(order[i], &kIID_IImageList, (void **)&il)) || !il) continue;
        int cx = 0, cy = 0;
        il->lpVtbl->GetIconSize(il, &cx, &cy);
        if (best) best->lpVtbl->Release(best);
        best = il;
        if (cx >= icon_px) break;
    }
    return best;
}

static int ext_icon(Buf *icons, ImgList *il, const wchar_t *probe, DWORD attr, int size, uint32_t *idx) {
    SHFILEINFOW sfi;
    memset(&sfi, 0, sizeof sfi);
    if (!api.SHGetFileInfoW(probe, attr, &sfi, sizeof sfi, SHGFI_USEFILEATTRIBUTES | SHGFI_SYSICONINDEX))
        return 0;
    HICON hi = NULL;
    if (FAILED(il->lpVtbl->GetIcon(il, sfi.iIcon, 0x1 /*ILD_TRANSPARENT*/, &hi)) || !hi) return 0;
    int ok = push_hicon(icons, hi, size, idx);
    DestroyIcon(hi);
    return ok;
}

int32_t sh_build_apps_cache(const uint16_t *out_path_u, int32_t icon_px, const uint16_t *exts_u) {
    const wchar_t *out_path = (const wchar_t *)out_path_u;
    const wchar_t *exts = (const wchar_t *)exts_u;
    if (icon_px < 8 || icon_px > 256) icon_px = 32;
    if (!load_api() || !api.SHGetKnownFolderItem) return 0;
    if (FAILED(api.CoInitializeEx(NULL, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE))) return 0;

    Buf apps = {0}, extrecs = {0}, pool = {0}, icons = {0};
    uint32_t n_apps = 0, n_exts = 0;

    IShellItem *folder = NULL;
    IEnumShellItems *en = NULL;
    if (SUCCEEDED(api.SHGetKnownFolderItem(&kFOLDERID_AppsFolder, 0, NULL, &kIID_IShellItem, (void **)&folder)) &&
        SUCCEEDED(IShellItem_BindToHandler(folder, NULL, &kBHID_EnumItems, &kIID_IEnumShellItems, (void **)&en))) {
        IShellItem *it = NULL;
        while (IEnumShellItems_Next(en, 1, &it, NULL) == S_OK) {
            LPWSTR name = NULL, parse = NULL;
            if (SUCCEEDED(IShellItem_GetDisplayName(it, SIGDN_NORMALDISPLAY, &name)) &&
                SUCCEEDED(IShellItem_GetDisplayName(it, SIGDN_PARENTRELATIVEPARSING, &parse)) &&
                name[0] && parse[0] && !skip_app(name, parse)) {
                AppRec r;
                r.icon = NO_ICON;
                r.name_off = str_push(&pool, name, &r.name_len);
                r.launch_off = str_push(&pool, parse, &r.launch_len);

                IShellItemImageFactory *fac = NULL;
                if (SUCCEEDED(IShellItem_QueryInterface(it, &kIID_IShellItemImageFactory, (void **)&fac))) {
                    SIZE sz = {icon_px, icon_px};
                    HBITMAP hbm = NULL;
                    if (SUCCEEDED(IShellItemImageFactory_GetImage(fac, sz, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK, &hbm)) && hbm) {
                        int w = 0, h = 0;
                        uint32_t *px = bitmap_pixels(hbm, &w, &h);
                        if (px) {
                            fix_alpha(px, (size_t)w * h, NULL);
                            if (!push_icon_pixels(&icons, px, w, h, icon_px, &r.icon)) r.icon = NO_ICON;
                            free(px);
                        }
                        DeleteObject(hbm);
                    }
                    IShellItemImageFactory_Release(fac);
                }
                buf_push(&apps, &r, sizeof r);
                n_apps++;
            }
            if (name) api.CoTaskMemFree(name);
            if (parse) api.CoTaskMemFree(parse);
            IShellItem_Release(it);
            it = NULL;
        }
    }
    if (en) IEnumShellItems_Release(en);
    if (folder) IShellItem_Release(folder);

    /* File-type icons: "/" = folder, "" = generic file, then each extension. */
    ImgList *il = (api.SHGetFileInfoW && api.SHGetImageList) ? pick_image_list(icon_px) : NULL;
    if (il) {
        ExtRec e;
        if (ext_icon(&icons, il, L"folder", FILE_ATTRIBUTE_DIRECTORY, icon_px, &e.icon)) {
            e.ext_off = str_push(&pool, L"/", &e.ext_len);
            buf_push(&extrecs, &e, sizeof e);
            n_exts++;
        }
        if (ext_icon(&icons, il, L"file", FILE_ATTRIBUTE_NORMAL, icon_px, &e.icon)) {
            e.ext_off = str_push(&pool, L"", &e.ext_len);
            buf_push(&extrecs, &e, sizeof e);
            n_exts++;
        }
        const wchar_t *p = exts ? exts : L"";
        while (*p) {
            const wchar_t *st = p;
            while (*p && *p != L'|') p++;
            size_t n = (size_t)(p - st);
            if (*p) p++;
            if (n == 0 || n > 15) continue;
            wchar_t probe[24] = L"x.";
            memcpy(probe + 2, st, n * 2);
            probe[2 + n] = 0;
            if (ext_icon(&icons, il, probe, FILE_ATTRIBUTE_NORMAL, icon_px, &e.icon)) {
                e.ext_off = str_push(&pool, probe + 2, &e.ext_len);
                buf_push(&extrecs, &e, sizeof e);
                n_exts++;
            }
        }
        il->lpVtbl->Release(il);
    }

    /* pad string pool to 4 bytes so the icon block is aligned */
    while (pool.len % 4) buf_push(&pool, "\0", 1);

    uint32_t hdr[8];
    uint32_t hdr_size = sizeof hdr;
    uint32_t strings_off = hdr_size + (uint32_t)apps.len + (uint32_t)extrecs.len;
    uint32_t icons_off = strings_off + (uint32_t)pool.len;
    hdr[0] = CACHE_MAGIC;
    hdr[1] = CACHE_VERSION;
    hdr[2] = (uint32_t)icon_px;
    hdr[3] = n_apps;
    hdr[4] = n_exts;
    hdr[5] = strings_off;
    hdr[6] = icons_off;
    hdr[7] = (uint32_t)(icons.len / ((size_t)icon_px * icon_px * 4));

    int ok = 0;
    HANDLE f = CreateFileW(out_path, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (f != INVALID_HANDLE_VALUE) {
        DWORD wr;
        ok = WriteFile(f, hdr, hdr_size, &wr, NULL) &&
             (!apps.len || WriteFile(f, apps.p, (DWORD)apps.len, &wr, NULL)) &&
             (!extrecs.len || WriteFile(f, extrecs.p, (DWORD)extrecs.len, &wr, NULL)) &&
             (!pool.len || WriteFile(f, pool.p, (DWORD)pool.len, &wr, NULL)) &&
             (!icons.len || WriteFile(f, icons.p, (DWORD)icons.len, &wr, NULL));
        CloseHandle(f);
        if (!ok) DeleteFileW(out_path);
    }
    free(apps.p);
    free(extrecs.p);
    free(pool.p);
    free(icons.p);
    api.CoUninitialize();
    return ok && n_apps > 0;
}

int32_t sh_open(const uint16_t *target, const uint16_t *verb) {
    if (!load_api() || !api.ShellExecuteExW) return 0;
    api.CoInitializeEx(NULL, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    SHELLEXECUTEINFOW sei;
    memset(&sei, 0, sizeof sei);
    sei.cbSize = sizeof sei;
    sei.fMask = SEE_MASK_NOASYNC;
    sei.lpVerb = (verb && verb[0]) ? (LPCWSTR)verb : NULL;
    sei.lpFile = (LPCWSTR)target;
    sei.nShow = SW_SHOWNORMAL;
    BOOL ok = api.ShellExecuteExW(&sei);
    api.CoUninitialize();
    return ok ? 1 : 0;
}

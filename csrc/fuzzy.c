/* fuzzy.c - ranking for launcher queries.
 *
 * Tiered scoring: exact > prefix > word-prefix > acronym > substring >
 * fuzzy subsequence. Tiers keep results predictable (typing "code" never
 * ranks "Encoder" above "Code") while the fuzzy DP rescues sloppy typing
 * for the small application list.
 */
#include "indicative.h"
#include <string.h>

#define MAXN 255
#define NEG (-1000000)

static int is_sep(uint8_t c) {
    return c == ' ' || c == '_' || c == '-' || c == '.' || c == '(' || c == ')' ||
           c == '[' || c == ']' || c == '/' || c == '\\' || c == '+' || c == ',' ||
           c == '&' || c == '\'' || c == ':';
}

/* Lowercase ASCII and the Latin-1 block (U+00C0..U+00DE = C3 80..C3 9E).
 * Branch-light so it vectorizes; Latin-1 is patched in a second pass only
 * when a C3 lead byte was seen. */
static int fold(const uint8_t *s, int slen, uint8_t *b) {
    int n = slen > MAXN ? MAXN : slen;
    unsigned hi = 0;
    for (int i = 0; i < n; i++) {
        uint8_t c = s[i];
        b[i] = (uint8_t)(c + (((unsigned)(c - 'A') < 26u) << 5));
        hi |= c;
    }
    if (hi & 0x80) {
        for (int i = 1; i < n; i++)
            if (s[i - 1] == 0xC3 && s[i] >= 0x80 && s[i] <= 0x9E && s[i] != 0x97) b[i] = (uint8_t)(s[i] + 0x20);
    }
    return n;
}

/* Word start on the original casing: after a separator, camelCase hump,
 * or a letter->digit transition. Never on a UTF-8 continuation byte. */
static int word_start(const uint8_t *s, int i) {
    if (i == 0) return 1;
    uint8_t c = s[i], p = s[i - 1];
    if ((c & 0xC0) == 0x80) return 0;
    if (is_sep(p)) return !is_sep(c);
    int pl = p >= 'a' && p <= 'z', pu = p >= 'A' && p <= 'Z';
    if (c >= 'A' && c <= 'Z' && pl) return 1;
    if (c >= '0' && c <= '9' && (pl || pu)) return 1;
    return 0;
}

/* Smith-Waterman-ish alignment of q as a subsequence of b. */
static int fuzzy_dp(const uint8_t *q, int qlen, const uint8_t *b, const uint8_t *ws, int n) {
    static const int BASE = 16, WORD = 30, FIRST = 20, CONSEC = 20, GAP = 2;
    int rowa[MAXN], rowb[MAXN];
    int *prev = rowa, *cur = rowb;

    for (int j = 0; j < n; j++) {
        /* the first query char must start a word: "code" shouldn't hit "Recorder" */
        if (b[j] != q[0] || !ws[j]) { prev[j] = NEG; continue; }
        int lead = j < 10 ? j : 10;
        prev[j] = BASE + (ws[j] ? WORD : 0) + (j == 0 ? FIRST : 0) - lead;
    }
    for (int i = 1; i < qlen; i++) {
        int g = NEG; /* best prev[k] - GAP*(j-k-1) for k <= j-2 */
        for (int j = 0; j < n; j++) {
            if (j >= 2) {
                int cand = prev[j - 2] > NEG ? prev[j - 2] - GAP : NEG;
                g = (g > NEG ? g - GAP : NEG);
                if (cand > g) g = cand;
            }
            if (b[j] != q[i]) { cur[j] = NEG; continue; }
            int bonus = BASE + (ws[j] ? WORD : 0);
            int best = NEG;
            if (j > 0 && prev[j - 1] > NEG) best = prev[j - 1] + bonus + CONSEC;
            if (g > NEG && g + bonus > best) best = g + bonus;
            cur[j] = best;
        }
        int *t = prev; prev = cur; cur = t;
    }
    int best = NEG;
    for (int j = 0; j < n; j++) if (prev[j] > best) best = prev[j];
    return best;
}

int32_t fz_score(const uint8_t *q, int32_t qlen, const uint8_t *s, int32_t slen, int32_t allow_fuzzy) {
    uint8_t b[MAXN];
    if (qlen <= 0 || slen <= 0) return 0;
    int n = fold(s, slen, b);
    if (qlen > n) return 0;

    int lenpen = n - qlen;
    if (lenpen > 60) lenpen = 60;

    if (n == qlen && memcmp(b, q, (size_t)n) == 0) return 10000;

    int first_sub = -1, first_ws = -1;
    const uint8_t q0 = q[0];
    const uint8_t *p = b, *end = b + n - qlen + 1;
    while (p < end && (p = memchr(p, q0, (size_t)(end - p))) != NULL) {
        int i = (int)(p - b);
        if (memcmp(p, q, (size_t)qlen) == 0) {
            if (first_sub < 0) first_sub = i;
            if (word_start(s, i)) { first_ws = i; break; }
        }
        p++;
    }
    if (first_ws == 0) return 9000 - lenpen * 10;
    if (first_ws > 0) {
        int sc = 8000 - first_ws * 15 - lenpen * 5;
        return sc < 7001 ? 7001 : sc;
    }

    /* acronym: every query char lands on a word start, in order */
    if (qlen >= 2) {
        int qi = 0;
        for (int i = 0; i < n && qi < qlen; i++)
            if (b[i] == q[qi] && word_start(s, i)) qi++;
        if (qi == qlen) {
            int sc = 7000 - lenpen * 5;
            return sc < 6001 ? 6001 : sc;
        }
    }

    if (first_sub >= 0) {
        int sc = 6000 - first_sub * 15 - lenpen * 5;
        return sc < 5001 ? 5001 : sc;
    }

    if (!allow_fuzzy || qlen < 2) return 0;

    /* cheap subsequence reject before the DP */
    {
        int qi = 0;
        for (int i = 0; i < n && qi < qlen; i++) if (b[i] == q[qi]) qi++;
        if (qi < qlen) return 0;
    }
    uint8_t ws[MAXN];
    for (int i = 0; i < n; i++) ws[i] = (uint8_t)word_start(s, i);
    int qn = qlen > 64 ? 64 : qlen;
    int raw = fuzzy_dp(q, qn, b, ws, n);
    /* require on average more than a bare match per character */
    if (raw < qn * 22) return 0;
    int maxraw = qn * 66 + 20;
    int sc = 1000 + (raw * 3000) / maxraw - lenpen * 3;
    if (sc > 4999) sc = 4999;
    return sc < 1 ? 1 : sc;
}

int32_t fz_score_query(const uint8_t *q, int32_t qlen, const uint8_t *s, int32_t slen, int32_t allow_fuzzy) {
    int32_t whole = fz_score(q, qlen, s, slen, allow_fuzzy);
    if (whole >= 5001) return whole;

    /* split on spaces; every term must hit at substring level or better */
    int terms = 0, sum = 0, minv = 1 << 30;
    int i = 0;
    while (i < qlen) {
        while (i < qlen && q[i] == ' ') i++;
        int st = i;
        while (i < qlen && q[i] != ' ') i++;
        if (i > st) {
            int sc = fz_score(q + st, i - st, s, slen, 0);
            if (sc < 5001) return whole;
            terms++;
            sum += sc;
            if (sc < minv) minv = sc;
        }
    }
    if (terms < 2) return whole;
    /* rank multi-term hits just under single-term substring hits */
    int sc = 4000 + (minv - 5000) / 5 + (sum / terms - 5000) / 10;
    if (sc > 5000) sc = 5000;
    return sc > whole ? sc : whole;
}

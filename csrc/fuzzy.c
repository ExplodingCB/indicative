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

/* Lowercase ASCII and the Latin-1 block (U+00C0..U+00DE as C3 80..C3 9E),
 * and flag word starts on the original casing (separators, camelCase,
 * letter->digit transitions). */
static int fold(const uint8_t *s, int slen, uint8_t *b, uint8_t *ws) {
    int n = slen > MAXN ? MAXN : slen;
    uint8_t prev = 0;
    int prev_lower = 0, prev_digit = 0;
    for (int i = 0; i < n; i++) {
        uint8_t c = s[i];
        int upper = (c >= 'A' && c <= 'Z');
        int lower = (c >= 'a' && c <= 'z');
        int digit = (c >= '0' && c <= '9');
        uint8_t f = upper ? (uint8_t)(c + 32) : c;
        if (prev == 0xC3 && c >= 0x80 && c <= 0x9E && c != 0x97) f = (uint8_t)(c + 0x20);
        b[i] = f;
        int start;
        if (i == 0) start = 1;
        else if (is_sep(prev)) start = !is_sep(c);
        else if (upper && prev_lower) start = 1;
        else if (digit && !prev_digit && (prev_lower || (prev >= 'A' && prev <= 'Z'))) start = 1;
        else start = 0;
        /* never mark a UTF-8 continuation byte as a word start */
        if ((c & 0xC0) == 0x80) start = 0;
        ws[i] = (uint8_t)start;
        prev = c;
        prev_lower = lower;
        prev_digit = digit;
    }
    return n;
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
    uint8_t b[MAXN], ws[MAXN];
    if (qlen <= 0 || slen <= 0) return 0;
    int n = fold(s, slen, b, ws);
    if (qlen > n) return 0;

    int lenpen = n - qlen;
    if (lenpen > 60) lenpen = 60;

    if (n == qlen && memcmp(b, q, (size_t)n) == 0) return 10000;

    int first_sub = -1, first_ws = -1;
    const uint8_t q0 = q[0];
    for (int i = 0; i + qlen <= n; i++) {
        if (b[i] != q0 || memcmp(b + i, q, (size_t)qlen) != 0) continue;
        if (first_sub < 0) first_sub = i;
        if (ws[i]) { first_ws = i; break; }
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
            if (ws[i] && b[i] == q[qi]) qi++;
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

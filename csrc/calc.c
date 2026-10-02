/* calc.c - inline calculator ("2^10 / 3", "sqrt(2)*pi", "15% of 80"-free).
 *
 * Recursive descent over a tiny token stream. Only reports a result when
 * the whole input parses AND it contains an operator or function, so a
 * plain number or a word never hijacks the top result.
 */
#include "indicative.h"
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifndef M_PI
#define M_PI 3.14159265358979323846
#endif
#ifndef M_E
#define M_E 2.71828182845904523536
#endif

typedef struct {
    const char *s;
    int pos, len;
    int ops;   /* operators / functions seen */
    int err;
} P;

static double expr(P *p);

static void ws(P *p) { while (p->pos < p->len && (p->s[p->pos] == ' ' || p->s[p->pos] == '\t')) p->pos++; }
static int peek(P *p) { ws(p); return p->pos < p->len ? (unsigned char)p->s[p->pos] : 0; }
static int is_alpha(int c) { return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z'); }
static int is_digit(int c) { return c >= '0' && c <= '9'; }

static int ident(P *p, char *out, int cap) {
    ws(p);
    int n = 0;
    while (p->pos < p->len && is_alpha(p->s[p->pos])) {
        char c = p->s[p->pos++];
        if (c >= 'A' && c <= 'Z') c = (char)(c + 32);
        if (n < cap - 1) out[n++] = c;
    }
    out[n] = 0;
    return n;
}

static double number(P *p) {
    ws(p);
    char buf[64];
    int n = 0, dots = 0;
    while (p->pos < p->len) {
        char c = p->s[p->pos];
        if (is_digit(c)) { if (n < 62) buf[n++] = c; p->pos++; }
        else if (c == '.') { if (++dots > 1) { p->err = 1; return 0; } if (n < 62) buf[n++] = c; p->pos++; }
        else if (c == ',' && n > 0 && p->pos + 3 < p->len && is_digit(p->s[p->pos + 1]) &&
                 is_digit(p->s[p->pos + 2]) && is_digit(p->s[p->pos + 3])) { p->pos++; } /* 1,000 */
        else if ((c == 'e' || c == 'E') && p->pos + 1 < p->len &&
                 (is_digit(p->s[p->pos + 1]) ||
                  ((p->s[p->pos + 1] == '-' || p->s[p->pos + 1] == '+') && p->pos + 2 < p->len && is_digit(p->s[p->pos + 2])))) {
            if (n < 60) buf[n++] = 'e';
            p->pos++;
            if (p->s[p->pos] == '-' || p->s[p->pos] == '+') { if (n < 62) buf[n++] = p->s[p->pos]; p->pos++; }
        } else break;
    }
    buf[n] = 0;
    if (n == 0 || (n == 1 && buf[0] == '.')) { p->err = 1; return 0; }
    return strtod(buf, NULL);
}

static double unary(P *p);

static double primary(P *p) {
    int c = peek(p);
    if (c == '(') {
        p->pos++;
        double v = expr(p);
        if (peek(p) == ')') p->pos++;
        else if (peek(p) != 0) p->err = 1; /* allow a missing trailing ')' while typing */
        return v;
    }
    if (is_digit(c) || c == '.') return number(p);
    if (is_alpha(c)) {
        char id[16];
        int save = p->pos;
        ident(p, id, sizeof id);
        if (!strcmp(id, "pi")) return M_PI;
        if (!strcmp(id, "tau")) return 2.0 * M_PI;
        if (!strcmp(id, "e")) return M_E;
        typedef double (*fn)(double);
        static const struct { const char *n; fn f; } F[] = {
            {"sqrt", sqrt}, {"sin", sin}, {"cos", cos}, {"tan", tan},
            {"asin", asin}, {"acos", acos}, {"atan", atan}, {"ln", log},
            {"log", log10}, {"log2", log2}, {"abs", fabs}, {"floor", floor},
            {"ceil", ceil}, {"round", round}, {"exp", exp}, {"sinh", sinh},
            {"cosh", cosh}, {"tanh", tanh}, {"cbrt", cbrt},
        };
        for (unsigned i = 0; i < sizeof F / sizeof F[0]; i++) {
            if (strcmp(id, F[i].n)) continue;
            p->ops++;
            double a;
            if (peek(p) == '(') a = primary(p);
            else a = unary(p); /* "sqrt 2" */
            return F[i].f(a);
        }
        p->pos = save;
        p->err = 1;
        return 0;
    }
    p->err = 1;
    return 0;
}

static double postfix(P *p) {
    double v = primary(p);
    while (!p->err && peek(p) == '!') {
        p->pos++;
        p->ops++;
        if (v < 0 || v > 170 || v != floor(v)) { p->err = 1; return 0; }
        double r = 1;
        for (int i = 2; i <= (int)v; i++) r *= i;
        v = r;
    }
    return v;
}

static double power(P *p) {
    double b = postfix(p);
    if (!p->err && peek(p) == '^') {
        p->pos++;
        p->ops++;
        double e = unary(p);   /* right associative */
        return pow(b, e);
    }
    if (!p->err && p->pos + 1 < p->len && p->s[p->pos] == '*' && p->s[p->pos + 1] == '*') {
        p->pos += 2;
        p->ops++;
        return pow(b, unary(p));
    }
    return b;
}

static double unary(P *p) {
    int c = peek(p);
    if (c == '-') { p->pos++; return -unary(p); }
    if (c == '+') { p->pos++; return unary(p); }
    return power(p);
}

static double term(P *p) {
    double v = unary(p);
    while (!p->err) {
        int c = peek(p);
        if (c == '*' && !(p->pos + 1 < p->len && p->s[p->pos + 1] == '*')) { p->pos++; p->ops++; v *= unary(p); }
        else if (c == '/') { p->pos++; p->ops++; v /= unary(p); }
        else if (c == '%') { p->pos++; p->ops++; v = fmod(v, unary(p)); }
        else if ((c == 'x' || c == 'X') && p->pos + 1 <= p->len &&
                 !(p->pos + 1 < p->len && is_alpha(p->s[p->pos + 1]))) { p->pos++; p->ops++; v *= unary(p); }
        else if (c == '(' || is_alpha(c)) { p->ops++; v *= power(p); } /* implicit: 2pi, 3(4) */
        else break;
    }
    return v;
}

static double expr(P *p) {
    double v = term(p);
    while (!p->err) {
        int c = peek(p);
        if (c == '+') { p->pos++; p->ops++; v += term(p); }
        else if (c == '-') { p->pos++; p->ops++; v -= term(p); }
        else break;
    }
    return v;
}

int32_t calc_eval(const uint16_t *e, int32_t len, double *out) {
    char s[256];
    int n = 0, digits = 0;
    if (len <= 0 || len > 250) return 0;
    for (int i = 0; i < len; i++) {
        uint16_t c = e[i];
        if (c == 0x00D7) c = '*';        /* × */
        else if (c == 0x00F7) c = '/';   /* ÷ */
        else if (c == 0x2212) c = '-';   /* − */
        else if (c == 0x03C0) { s[n++] = 'p'; c = 'i'; } /* π */
        if (c >= 128) return 0;
        if (c == '=' && i == len - 1) break; /* trailing "=" */
        if (is_digit(c)) digits++;
        s[n++] = (char)c;
    }
    s[n] = 0;
    if (!digits && !strstr(s, "pi")) return 0;

    P p = {s, 0, n, 0, 0};
    double v = expr(&p);
    if (p.err || peek(&p) != 0 || p.ops == 0) return 0;
    if (isnan(v) || isinf(v)) return 0;
    *out = v;
    return 1;
}

int32_t calc_format(double v, uint16_t *buf, int32_t cap) {
    char s[64];
    if (fabs(v) < 1e-12) v = 0;
    if (fabs(v) < 1e15 && v == floor(v)) snprintf(s, sizeof s, "%.0f", v);
    else {
        snprintf(s, sizeof s, "%.10g", v);
        /* trim trailing zeros in the mantissa */
        char *e = strchr(s, 'e');
        if (!e && strchr(s, '.')) {
            int l = (int)strlen(s);
            while (l > 0 && s[l - 1] == '0') s[--l] = 0;
            if (l > 0 && s[l - 1] == '.') s[--l] = 0;
        }
    }
    int n = 0;
    for (; s[n] && n < cap; n++) buf[n] = (uint16_t)(unsigned char)s[n];
    return n;
}

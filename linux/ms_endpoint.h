/*
 * ms_endpoint.h - default telemetry endpoint, obfuscated.
 *
 * The default endpoint (host "135.125.79.15", port 28571) is stored base64
 * encoded so the deployed server address does not sit in the source or the
 * binary as a readable string.  Base64 is obfuscation, not cryptography: the
 * value has to be recoverable at runtime, and the endpoint is still sent in
 * the clear on the telemetry link.  --telemetry / --no-telemetry still
 * override it.
 *
 * Header-only and dependency-free (pure C, <stddef.h> only) so it can be
 * shared by ms_main.c and ms_net.c.
 *
 * MIT License
 */
#ifndef MS_ENDPOINT_H
#define MS_ENDPOINT_H

#include <stddef.h>

static const char MS_ENDPOINT_HOST_B64[] = "MTM1LjEyNS43OS4xNQ=="; /* 135.125.79.15 */
static const char MS_ENDPOINT_PORT_B64[] = "Mjg1NzE=";              /* 28571 */

static inline int ms_endpoint_b64_val(unsigned char c) {
    if (c >= 'A' && c <= 'Z') return c - 'A';
    if (c >= 'a' && c <= 'z') return c - 'a' + 26;
    if (c >= '0' && c <= '9') return c - '0' + 52;
    if (c == '+') return 62;
    if (c == '/') return 63;
    return -1;
}

static inline size_t ms_endpoint_b64_decode(const char *in, char *out,
                                            size_t outsz) {
    size_t o = 0, i;
    unsigned v = 0;
    int bits = 0;
    for (i = 0; in[i] && in[i] != '='; i++) {
        int d = ms_endpoint_b64_val((unsigned char)in[i]);
        if (d < 0) continue;
        v = (v << 6) | (unsigned)d;
        bits += 6;
        if (bits >= 8) {
            bits -= 8;
            if (o < outsz) out[o++] = (char)((v >> bits) & 0xFF);
        }
    }
    return o;
}

/* Decode the obfuscated default host into out (NUL-terminated).  Returns the
 * decoded length (excluding the NUL), or 0 on failure. */
static inline size_t ms_endpoint_default_host(char *out, size_t outsz) {
    size_t n;
    if (!out || outsz == 0) return 0;
    n = ms_endpoint_b64_decode(MS_ENDPOINT_HOST_B64, out, outsz - 1);
    out[n] = 0;
    return n;
}

/* Decode the obfuscated default port.  Returns the port, or 0 on failure. */
static inline unsigned ms_endpoint_default_port(void) {
    char buf[16];
    size_t n, i;
    unsigned p = 0;
    n = ms_endpoint_b64_decode(MS_ENDPOINT_PORT_B64, buf, sizeof(buf) - 1);
    buf[n] = 0;
    if (n == 0) return 0;
    for (i = 0; i < n; i++) {
        if (buf[i] < '0' || buf[i] > '9') return 0;
        p = p * 10 + (unsigned)(buf[i] - '0');
    }
    return p;
}

#endif /* MS_ENDPOINT_H */

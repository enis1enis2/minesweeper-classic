/*
 * ms_sha256.h - self-contained SHA-256 / HMAC-SHA256 for the Linux port.
 *
 * No OpenSSL or libcrypto dependency: the exact public-domain implementation
 * carried in the Win32 sources (src/network.c) is extracted here so the
 * solver HMAC challenge-response works identically on both platforms.
 *
 * MIT License
 */
#ifndef MS_SHA256_H
#define MS_SHA256_H

#include <stddef.h>
#include <stdint.h>

void ms_sha256(const void *data, size_t n, uint8_t out[32]);
void ms_hmac_sha256(const uint8_t *key, size_t klen,
                    const uint8_t *msg, size_t mlen, uint8_t out[32]);
void ms_hex_encode(const uint8_t *src, size_t n, char *dst);

#endif /* MS_SHA256_H */

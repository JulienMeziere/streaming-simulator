/*
 * Force-included shim that lets MSVC compile libsbc-sys's vendored BlueZ
 * SBC C source. Two GCC-isms in <sbc.c> trip cl.exe:
 *
 *   1. `__attribute__((aligned(...)))` etc. — MSVC parses `((...))` as a
 *      function call expression, producing cascades of "function returns
 *      function" / "missing ')' before '('" syntax errors.
 *   2. Missing POSIX types: <stdint.h> isn't transitively included via
 *      <sys/types.h>, and `ssize_t` isn't provided at all (MSVC has the
 *      uppercase Windows-typed SSIZE_T but not the lowercase POSIX alias).
 *
 * Used via `cl.exe -FI <abs-path>` from the Release workflow only.
 */

#pragma once

#include <stddef.h>
#include <stdint.h>

#ifdef _MSC_VER

#ifndef __attribute__
#define __attribute__(x)
#endif

#ifndef _SSIZE_T_DEFINED
typedef intptr_t ssize_t;
#define _SSIZE_T_DEFINED
#endif

#endif /* _MSC_VER */

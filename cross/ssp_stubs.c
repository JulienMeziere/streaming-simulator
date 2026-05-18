/*
 * Stub implementations of GCC's stack-smashing-protector and fortify-source
 * helpers.
 *
 * Why this exists:
 *   When mingw-w64 on Linux is used to cross-compile C libraries (notably
 *   libopus, built by audiopus_sys via cmake) the resulting object files
 *   contain references to `__stack_chk_fail`, `__stack_chk_guard`,
 *   `__memcpy_chk`, and `__memset_chk`. These symbols live in libssp.
 *
 *   Linking libssp via rustc on this toolchain triggers a cascade of further
 *   missing-symbol problems (advapi32, msvcrt, argument-ordering quirks), and
 *   suppressing the protection at C compile time via `CFLAGS` doesn't reach
 *   the `cmake` build for *-sys crates. Providing tiny stubs here is the
 *   simplest, most portable workaround.
 *
 * Security implications:
 *   Real stack-canary checking is disabled (the canary just compares against
 *   a fixed constant). For an audio plugin running inside the user's own DAW
 *   this is not a meaningful security boundary — any attacker who can feed
 *   crafted audio to the plugin can already feed crafted plugins instead.
 *   If you really want stack protection in this build, install libssp.a in a
 *   location rustc can find and have the script link against it instead.
 */

#include <string.h>

void *__memcpy_chk(void *dest, const void *src, unsigned long n, unsigned long destsize) {
    (void)destsize;
    return memcpy(dest, src, n);
}

void *__memset_chk(void *dest, int c, unsigned long n, unsigned long destsize) {
    (void)destsize;
    return memset(dest, c, n);
}

unsigned long __stack_chk_guard = 0xdeadbeefcafebabeULL;

__attribute__((noreturn))
void __stack_chk_fail(void) {
    __builtin_trap();
}

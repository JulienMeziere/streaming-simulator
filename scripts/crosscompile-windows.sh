#!/usr/bin/env bash
# Cross-compile the plugin into Windows VST3 + CLAP bundles from any
# Linux/macOS host. Useful when you want to test in a Windows DAW (FL Studio,
# Ableton, etc.) without leaving your dev machine (or from WSL). Run from anywhere — the
# script `cd`s into the repo root itself.
#
# Usage:
#   ./scripts/crosscompile-windows.sh                       # release build (default)
#   ./scripts/crosscompile-windows.sh --debug               # debug build (faster compile)
#   ./scripts/crosscompile-windows.sh --features fdk-aac    # cargo features
#
# Environment overrides:
#   TARGET   Rust target triple (default: x86_64-pc-windows-gnu)
#
# Note about `--features fdk-aac`: this enables AAC-LC and HE-AAC v2 codecs
# by linking Fraunhofer's FDK-AAC. Official release builds use this feature.
# See docs/licensing.md for the GPL/FDK situation.
#
# One-time setup:
#   rustup target add x86_64-pc-windows-gnu
#   # plus a MinGW toolchain providing x86_64-w64-mingw32-gcc
#   #   Debian/Ubuntu: sudo apt install mingw-w64
#   #   Fedora:        sudo dnf install mingw64-gcc
#   #   Arch:          sudo pacman -S mingw-w64-gcc
#   #   macOS:         brew install mingw-w64
#
# After the script finishes, copy the resulting bundle directories into your
# DAW's plugin search path on the Windows machine. Example destinations:
#   VST3: %COMMONPROGRAMFILES%\VST3   (i.e. C:\Program Files\Common Files\VST3)
#   CLAP: %COMMONPROGRAMFILES%\CLAP

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PLUGIN_CRATE="streaming-simulator"
PLUGIN_NAME="Streaming Simulator"

TARGET="${TARGET:-x86_64-pc-windows-gnu}"
PROFILE_FLAG="--release"
PROFILE_DIR="release"
FEATURES=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug)
            PROFILE_FLAG=""
            PROFILE_DIR="debug"
            shift
            ;;
        --features)
            FEATURES="$2"
            shift 2
            ;;
        --features=*)
            FEATURES="${1#--features=}"
            shift
            ;;
        -h|--help)
            sed -n '2,29p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "error: unknown argument '$1'" >&2
            echo "run with --help for usage" >&2
            exit 2
            ;;
    esac
done

if ! rustup target list --installed | grep -qx "$TARGET"; then
    echo "error: rust target '$TARGET' is not installed." >&2
    echo "  fix:  rustup target add $TARGET" >&2
    exit 1
fi

if [[ "$TARGET" == *"-windows-gnu" ]] && ! command -v x86_64-w64-mingw32-gcc >/dev/null; then
    echo "error: mingw-w64 toolchain not found (need x86_64-w64-mingw32-gcc)." >&2
    echo "  fix:  install your distro's mingw-w64 package (see --help)" >&2
    exit 1
fi

# nih_plug_xtask writes everything to `target/bundled/` regardless of which
# Rust target was used. Clear any previous bundle for this plugin before
# rebuilding so we don't end up with a multi-arch VST3 (the bundler merges new
# arches into existing bundles instead of replacing them).
BUNDLE_DIR="target/bundled"
rm -rf "$BUNDLE_DIR/$PLUGIN_NAME.vst3" "$BUNDLE_DIR/$PLUGIN_NAME.clap"

# Cross-built C deps from *-sys crates (notably libopus via audiopus_sys)
# reference GCC's stack-canary / fortify-source helpers (`__stack_chk_fail`,
# `__memcpy_chk`, …) that live in libssp. Linking libssp through rustc on
# mingw-w64 leads to a cascade of further missing symbols (advapi32, msvcrt,
# argument-ordering quirks), and disabling the protection via CFLAGS doesn't
# reach the cmake build. Instead we compile a tiny stub object that defines
# those symbols ourselves and link it in. See `cross/ssp_stubs.c` for the
# full rationale.
if [[ "$TARGET" == "x86_64-pc-windows-gnu" ]]; then
    STUBS_C="$REPO_ROOT/cross/ssp_stubs.c"
    STUBS_O="$REPO_ROOT/target/$TARGET/ssp_stubs.o"
    mkdir -p "$(dirname "$STUBS_O")"
    if [[ ! -f "$STUBS_O" || "$STUBS_C" -nt "$STUBS_O" ]]; then
        echo ">> compiling cross/ssp_stubs.c → $STUBS_O"
        x86_64-w64-mingw32-gcc -c -O2 -fno-stack-protector "$STUBS_C" -o "$STUBS_O"
    fi
    export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS="-C link-arg=$STUBS_O"

    # FDK-AAC's `common_fix.h` has a conditional block (lines ~406-413) that
    # was meant for 32-bit hosts (`!defined(__LP64__)`) but fires on mingw-w64
    # too — Windows 64-bit is LLP64, so `__LP64__` isn't defined even though
    # `__x86_64__` is. Inside that block it emits a `fMin(SHORT, SHORT)`
    # overload that collides with `fMin(FIXP_SGL, FIXP_SGL)` from earlier in
    # the same header (FIXP_SGL aliases to SHORT). Force the LP64 define so
    # the whole block is skipped. There are no real LP64-vs-LLP64 size
    # assumptions in FDK-AAC's data layout that this affects.
    #
    # `-include stdint.h` is for libsbc-sys: BlueZ's libsbc relies on
    # `<sys/types.h>` to transitively pull in `int32_t` / `int16_t`,
    # which works on Linux glibc but not on mingw-w64. Force-including
    # stdint.h ahead of every translation unit fixes the missing
    # types without patching the vendored source. It's a no-op for
    # the other -sys crates that already `#include <stdint.h>`.
    export CFLAGS="${CFLAGS:-} -D__LP64__=1 -include stdint.h"
    export CXXFLAGS="${CXXFLAGS:-} -D__LP64__=1 -include stdint.h"
fi

FEATURES_FLAG=""
if [[ -n "$FEATURES" ]]; then
    FEATURES_FLAG="--features $FEATURES"
    echo ">> features: $FEATURES"
fi

echo ">> cross-compiling $PLUGIN_CRATE for $TARGET ($PROFILE_DIR)"
# shellcheck disable=SC2086
cargo xtask bundle "$PLUGIN_CRATE" --target "$TARGET" $PROFILE_FLAG $FEATURES_FLAG

echo
echo ">> built bundles in $BUNDLE_DIR/"
[[ -e "$BUNDLE_DIR/$PLUGIN_NAME.vst3" ]] && echo "   $PLUGIN_NAME.vst3/"
[[ -e "$BUNDLE_DIR/$PLUGIN_NAME.clap" ]] && echo "   $PLUGIN_NAME.clap"
true

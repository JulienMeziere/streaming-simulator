# Security policy

## Reporting a vulnerability

Please **do not** file a public issue for security problems.

Email **julien@meziere.org** with the subject prefix `[security]` and
include:

- A description of the issue and its impact (e.g. crash, RCE, audio
  buffer corruption, undefined behaviour in `unsafe` FFI).
- A minimal reproducer (host DAW + sample rate + buffer size + steps).
- The plugin version (visible in the editor's `?` info popup).

You should expect an acknowledgement within a few days. Confirmed issues
will be fixed in a patch release; the reporter will be credited in the
release notes unless they prefer to remain anonymous.

## Scope

In-scope:

- Crashes / undefined behaviour in the audio thread.
- Memory-safety bugs in the `unsafe` FFI bindings (Opus, Vorbis, MP3,
  FDK-AAC, SBC, LC3).
- Build-time supply-chain issues (e.g. malicious dependency).

Out of scope:

- Audio quality complaints — those are bug reports, file an issue.
- Sandbox escapes from a host DAW — please report those to the DAW
  vendor, not here.

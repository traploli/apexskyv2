# apexskyv2

Apexsky source tree with validated offset updating and Linux release tooling.

## Scope

This repository contains the source, native module, WASM offset parser, runtime
package manifests, and build/update scripts. Generated binaries, packages,
archives, runtime logs, and local updater caches are intentionally ignored.

## Offset updater

The updater polls the read-only Apex offsets API and uses ETag caching:

```text
https://www.cheatoffsets.com/api/games/apex/current
```

Run it with:

```bash
python3 tools/update_offsets.py \
  --profile apex1_common/resource/default/offsets-v3.0.2.23.ini \
  --etag-file apex1_common/resource/default/offsets-v3.0.2.23.ini.etag \
  --min-updates 100
```

Updates are validated and written atomically. An unchanged response returns
HTTP 304 and preserves the existing profile. Missing API fields preserve the
last known-good native values.

## Build

The release builder requires Rust nightly, the WASI target, Python 3, and the
OhOSky module packer:

```bash
./tools/build_release.sh dist
```

The local builder targets x86_64 Linux, builds the native DMA library and
overlay, builds the WASM offset parser with the runtime-compatible pinned WASM
toolchain, and produces `apex1.spk` and `apex1_sg.spk`.

Run the test suites with:

```bash
python3 -m unittest tools.test_update_offsets -v
cargo +nightly-2025-10-12 test -p apex1_offsets_parser --no-default-features
```

## Launch

The package runtime is supplied separately as `skyrun`. With the generated
packages in one directory, launch as a normal user:

```bash
./skyrun --load apex1.spk --load apex1_sg.spk kvm
```

Do not use `sudo`; the runtime and overlay reject root execution, and root
launches can create root-owned runtime files.

## CR3 handling and Windows 10 20H1 reference

The CR3/shadow-origin path includes cached secondary-DTB resolution, PE-header
validation, distance-based validation for local-player duplicate origins, and
explicit DTB-scoped reads. The implementation is intended to avoid treating a
stale or invalid CR3 as valid; it does not blindly rotate page-table roots.

Windows 10 version 2004 (20H1) is a reference environment for compatibility
work, not a guarantee that every Windows 10 20H1 build exposes identical
kernel or game memory behavior. Validate the exact build, hypervisor, memory
connector, and target module before enabling direct DTB reads. Keep
`use_memflow_direct = false` unless the configured connector has been tested
with the target VM and its DTB acquisition path.

Reference media:

```text
https://archive.org/details/win-10-2004-english-x-64_202010
```

This link identifies the Windows 10 2004 x64 reference media. It is not a
source of offsets or a substitute for validating the running system. Do not
publish or redistribute Microsoft installation media through this repository.

For a Windows 10 20H1 compatibility test, record the exact OS build, kernel
CR3 source, connector type, module base, validation result, and whether the
shadow-origin distance check remains within its configured bound. Report a
failed validation and retain the last known-good DTB rather than forcing a
new CR3.

### Version support: KB4598291

KB4598291 is supported as a Windows 10 version 2004/20H1 reference update for
x64 compatibility testing. Microsoft Update Catalog identifies it as the
2021-01 cumulative update preview for Windows 10 Version 2004 x64, published
February 2, 2021. The same KB also has separate 20H2, x86, ARM64, and Server
packages; those packages are not interchangeable with the Windows 10 2004
x64 reference environment.

Update Catalog:

```text
https://www.catalog.update.microsoft.com/Search.aspx?q=kb4598291
```

Record the installed OS build and architecture alongside the KB number. The
KB alone is not sufficient to identify the kernel revision or guarantee CR3,
DTB, offset, or connector compatibility. Revalidate PE headers, secondary-DTB
resolution, and the local-player shadow-origin distance check after installing
or changing the update.

## License


The checked-in native profile and generated packages are intended for the
matching x86_64 Linux runtime and current game build. A separate target build
is required for Windows. Offsets are loaded at startup; rebuild and restart
after an update.

No license has been declared yet. Add an appropriate `LICENSE` file before
publishing a public repository if the source is intended for redistribution.

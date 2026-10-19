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

## Compatibility

The checked-in native profile and generated packages are intended for the
matching x86_64 Linux runtime and current game build. A separate target build
is required for Windows. Offsets are loaded at startup; rebuild and restart
after an update.

## License

No license has been declared yet. Add an appropriate `LICENSE` file before
publishing a public repository if the source is intended for redistribution.

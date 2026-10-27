#!/usr/bin/env bash
# Build and package the three Apexsky deliverables.
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
REPO="$(cd -- "$ROOT/.." && pwd)"
PACKER="$REPO/ohosky_sdk/bin/x86_64-unknown-linux-gnu/ohosky_mod_packer"
OUT="${1:-$ROOT/dist}"
TARGET="${CARGO_BUILD_TARGET:-x86_64-unknown-linux-gnu}"
# The bundled skyrun runtime expects the older WIT world emitted by this
# pinned nightly/toolchain combination.
WASM_TOOLCHAIN="${WASM_TOOLCHAIN:-nightly-2025-10-12}"

if [[ "$TARGET" != "x86_64-unknown-linux-gnu" ]]; then
  echo "This local packager currently supports x86_64-unknown-linux-gnu only" >&2
  exit 2
fi
command -v cargo >/dev/null || { echo "cargo is required" >&2; exit 2; }
test -x "$PACKER" || { echo "missing packer: $PACKER" >&2; exit 2; }

# Network updating is opt-in: APEXSKY_AUTO_UPDATE=1 ./tools/build_release.sh
if [[ "${APEXSKY_AUTO_UPDATE:-0}" == "1" ]]; then
  python3 "$ROOT/tools/update_offsets.py" \
    --profile "$ROOT/apex1_common/resource/default/offsets-v3.0.2.23.ini" \
    --etag-file "$ROOT/apex1_common/resource/default/offsets-v3.0.2.23.ini.etag" \
    --min-updates 100
else
  echo "offset auto-update disabled; using the selected local profile"
fi

cargo +nightly build --release --target "$TARGET" -p apexsky_dma -p apexsky_overlay
rustup target add wasm32-wasip2 --toolchain "$WASM_TOOLCHAIN"
cargo +"$WASM_TOOLCHAIN" build --release --target wasm32-wasip2 -p apex1_offsets_parser

# Build into a fresh user-owned directory. A prior root launch may have left
# root-owned cache files in the requested output directory.
if [[ -e "$OUT" ]]; then
  if ! rm -rf "$OUT" 2>/dev/null; then
    echo "cannot remove output directory (likely root-owned): $OUT" >&2
    echo "choose a new output path or remove it with appropriate privileges" >&2
    exit 1
  fi
fi
mkdir -p "$OUT/apex1/native/x86_64-linux" "$OUT/apex1_sg"
cp "$ROOT/target/$TARGET/release/libapexsky_dma.so" "$OUT/apex1/native/x86_64-linux/"
cp "$ROOT/target/$TARGET/release/apexsky_overlay" "$OUT/"
cp "$ROOT/target/wasm32-wasip2/release/apex1_offsets_parser.wasm" "$OUT/apex1/"
# Refuse to package a parser whose WIT world the bundled runtime cannot load.
if ! strings "$OUT/apex1/apex1_offsets_parser.wasm" | grep -q 'wasi:cli/exit@0.2.6'; then
  echo "parser WIT world mismatch: expected wasi:cli/exit@0.2.6" >&2
  exit 1
fi
cp "$ROOT/mods/apex1/skyapex.wasm" "$OUT/apex1/"
cp "$ROOT/mods/apex1/manifest.json" "$OUT/apex1/"
cp "$ROOT/mods/apex1_sg/worker.js" "$OUT/apex1_sg/"
cp "$ROOT/mods/apex1_sg/manifest.json" "$OUT/apex1_sg/"
cp "$ROOT/mods/script_types.js" "$OUT/"

# A Linux build cannot package the Windows DLL referenced by the shared
# manifest. Limit this local package to the artifact actually built here.
python3 - "$OUT/apex1/manifest.json" <<'PY'
import json
import sys
from pathlib import Path
path = Path(sys.argv[1])
data = json.loads(path.read_text())
for component in data.get("components", []):
    service = component.get("service")
    if isinstance(service, dict) and service.get("name") == "apexsky_dma":
        service["target"] = ["x86_64-linux"]
path.write_text(json.dumps(data, indent=4) + "\n")
PY

"$PACKER" "$OUT/apex1.spk" "$OUT/apex1/manifest.json"
"$PACKER" "$OUT/apex1_sg.spk" "$OUT/apex1_sg/manifest.json"

printf '\nBuild outputs:\n'
stat -c '%s %n' "$OUT/apex1.spk" "$OUT/apex1_sg.spk" "$OUT/apexsky_overlay"

#!/usr/bin/env bash
# Update offsets before launching Apexsky; keep the last known-good profile on failure.
set -u

ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
PROFILE="$ROOT/apex1_common/resource/default/offsets-v3.0.2.23.ini"
ETAG="$PROFILE.etag"
UPDATER="$ROOT/tools/update_offsets.py"

if ! python3 "$UPDATER" \
    --profile "$PROFILE" \
    --etag-file "$ETAG" \
    --min-updates 100; then
    printf '%s\n' "offset update failed; continuing with the last known-good profile" >&2
fi

# Apexsky resolves a selector when offsets.ini is present; launch from the
# resource directory so the selector and selected profile are both visible.
if [[ "$#" -eq 0 ]]; then
    printf '%s\n' "usage: $0 -- <Apexsky command> [args...]" >&2
    exit 2
fi
if [[ "$1" == "--" ]]; then shift; fi
if [[ "$#" -eq 0 ]]; then
    printf '%s\n' "missing launch command" >&2
    exit 2
fi
cd "$ROOT/apex1_common/resource/default"
exec "$@"

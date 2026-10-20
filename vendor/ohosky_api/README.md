# Tracked OhOSky API dmalib surface

The repository builds against the sibling SDK at `../../ohosky_sdk/ohosky_api`, as declared by the workspace crates. The sibling SDK is outside this Git repository, so vendoring only this file cannot replace the Cargo dependency without also vendoring the complete SDK crate graph. `src/common/dmalib.rs` is therefore a tracked, reproducible snapshot of the SDK trait extension (`read_raw_with_dtb`, `read_raw_with_dtb_blocking`, `read_with_dtb`, and `supports_dtb_override`).

Snapshot source: `../../ohosky_sdk/ohosky_api/src/common/dmalib.rs`
Snapshot SHA-256: `7dfa2ebae9ad52379287db92dbd166793ca32e5c1e8ec8fd0d319ff30a42c849`

When updating the SDK dependency, copy the corresponding file from the sibling SDK into this path and update the recorded SHA-256. Do not change Cargo path dependencies unless the complete SDK layout is brought into this repository; this snapshot keeps the external DTB trait surface reviewable and reproducible without breaking the current Cargo layout.

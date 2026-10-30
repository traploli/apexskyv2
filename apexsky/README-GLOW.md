# Glow pipeline (end to end)

This document follows glow from a menu/configuration change to the memory writes
that the game renderer consumes. The implementation is in
[`apexsky_dma/src/workers/actions.rs`](apexsky_dma/src/workers/actions.rs), with
settings in [`apex1_common/src/config.rs`](apex1_common/src/config.rs), and the
loot producer in [`apexsky_dma/src/workers/items.rs`](apexsky_dma/src/workers/items.rs).

The current build has three related glow paths:

* **Player glow** selects enemy player entities and writes their per-entity
  highlight context.
* **Item/loot glow** periodically classifies loot and sends `(entity, context)`
  pairs to the action worker.
* **Weapon-model glow** assigns a context to the local player's view model.

All three paths depend on the global highlight context table being populated.
That table is written through the normal process address space and, when a
validated secondary page-table root is available, mirrored through that root as
well.

## Entry points and task wiring

`State::start_tasks` creates a Tokio `watch` channel for item glow and starts
both workers:

```text
State::start_tasks
  ├─ actions_loop(..., items_glow_rx)
  └─ items_loop(..., items_glow_tx)
```

See [`apexsky_dma/src/lib.rs`](apexsky_dma/src/lib.rs) around
`start_tasks`. `actions_loop` runs the game-state tick. On each active tick it
clones the current `Settings`, updates the entity maps, and, when the world and
local player are ready, runs the four glow-related futures together:

```text
task_inject_highlights   global context table
task_player_glow         player entity context bytes
task_items_glow          loot entity context IDs
task_vmodel_glow         local view-model context ID
```

The futures are joined with `tokio::join!`; a failure in one path is logged by
that path and does not automatically stop the action loop.

## Configuration and menu controls

The wire format is `Settings::glow_settings`, a `#[repr(C)]` `GlowSettings`
structure. The fields are defined in
[`apex1_common/src/config.rs`](apex1_common/src/config.rs):

| Field | Used by the active path |
| --- | --- |
| `player_glow` | Gates player target collection and player writes. |
| `item_glow` | Gates loot writes after `items_loop` publishes a new list. |
| `weapon_model_glow` | Gates the local view-model write. |
| `weapon_model_transparent` | Chooses outline-only or filled weapon context bits. |
| `player_filled_toggle` | Chooses filled or outline-only bits for active context 79. |
| `player_brightness` | Multiplies configured player RGB values. |
| `player_glow_inside_value` | Inside/fill function byte for player context bits. |
| `player_glow_outline_value` | Outline function byte for player context bits. |
| `player_glow_outline_size` | Outline radius byte for player context bits. |
| `player_glow_inside_opacity` | Low six opacity/state bits for player context bits. |
| `loot_filled_toggle`, `loot_filled` | Loot context construction. `loot_outline` is retained in the ABI but is not read by the current injector, which uses outline byte `125`. |
| `player_glow_armor_color`, `player_glow_love_user`, `player_glow_rainbow` | Used by the standalone `player_glow` selector, described below; the current write path does not call that function. |

The feature menu is [`menu/glow_features_menu.rs`](apexsky_dma/src/menu/glow_features_menu.rs).
It changes the live global config under `lock_config!()`:

* **1** toggles item glow.
* **2** toggles player glow.
* **3** toggles loot fill and sets `loot_filled` to `14` or `0`.
* **4** toggles player fill and sets `player_glow_inside_value` to `14` or `0`.
* **5** edits outline size as a `u8`.
* **6** edits brightness as a `u8`.
* **7** toggles armor-color selection.
* **8** toggles favorite-player selection.
* **9** toggles weapon-model glow.
* **10** toggles weapon-model transparency.
* **11** toggles the rainbow selector flag.

The color menu is [`menu/glow_color_menu.rs`](apexsky_dma/src/menu/glow_color_menu.rs).
It accepts three space-separated floats in the half-open range `[0, 1)` and
updates:

* `glow_*_not` for the enemy/not-visible color,
* `glow_*_viz` for the visible-player color, and
* `glow_*_knocked` for downed players.

The defaults are red for `not`, green for `viz`, and a pale yellow for
`knocked`. In the active player write path, context 79 uses the `not` color;
the visible and knocked colors are still written into their separate global
slots but are not selected by the current per-entity write.

## Tick order and readiness gates

The action worker first refreshes the game state and builds `shared_state`
maps. A player is inserted only when its EADP UID is nonzero and its team is in
the broad sanity range. `local_player_ptr` is set only if the local player made
it into that map.

Glow work is skipped unless `world_ready` is true. `task_inject_highlights`
also requires at least one of `player_glow`, `item_glow`, or
`weapon_model_glow`. If the highlight table pointer is unavailable during a
map transition, the injector returns without writing.

The diagnostic heartbeat in the action loop records the conditions that most
often explain “the menu is on but nothing changed”:

```text
world_ready, player_glow, player_ready, targets, lplayer,
players_map, aim_entities
```

`targets=0` means no player context write is attempted. It usually means that
the local player is not in the filtered player map, rather than a failed memory
write.

## Player target selection

When `player_glow` is enabled, `task_player_glow` builds its own write set from
`shared_state.aim_entities`. It deliberately does **not** use the aimbot's
`aim_select_rx`: aimbot targets are filtered by distance, FOV, and shadow-origin
quality, while glow needs a stable wallhack write set.

The active filter keeps entities that are:

1. player entities;
2. alive;
3. on a team in `0..=50`;
4. not the local player; and
5. not a teammate according to `teammate_check`.

Normal modes compare team IDs directly. TDM compares team parity. The map-test
team override is also honored. No distance or FOV test is applied here.

The older `collect_aim_targets`/`process_player` path still exists for the
aimbot and diagnostics. It applies distance, FOV, visibility, and other aim
rules, so it must not be treated as the player-glow write set.

## Per-entity player write

For every accepted player, the current write path uses one confirmed field:

```text
entity + 0x294  (u8 m_highlightCurrent)
```

The value is context **79**, `HIGHLIGHT_PLAYER_RAINBOW`. The name is historical:
the active path routes every accepted live player to this single context so the
renderer uses one empirically working player slot. It does not write the
unverified visibility, far-fade, focused, health, or generic-context fields.

Before writing, the task:

1. reads `entity + 0x294` through the primary mapping;
2. reads the same context byte and the entity team through the candidate shadow
   DTB, when one is available;
3. accepts the shadow mapping only when the context is plausible and both roots
   report the same valid team;
4. writes the byte through the confirmed shadow DTB when that check succeeds;
5. otherwise writes through the primary mapping only when the current byte is
   not already 79.

The read-before-write check prevents an unconditional write every tick. A
dual-root diagnostic is emitted once when the validated DTB changes, including
the before/after context values and whether the write used the primary or
shadow mapping.

### The `player_glow` helper

`player_glow` is a separate selector function in `actions.rs`. It maps a
`AimTargetInfo` to contexts for knocked, armor-colored, favorite, kill-leader,
filled, and rainbow behavior. It is useful documentation of the intended
context policy, but **the current `task_player_glow` implementation does not
call it**; it writes context 79 directly. Consequently, toggling armor color,
favorite-player glow, or the selector's rainbow flag does not change the
active per-entity context until the write path is changed to use that helper.

## Global highlight table injection

`inject_highlight` runs from `task_inject_highlights` on every eligible action
tick. It constructs 22 context payloads. Each payload consists of:

* four `u8` `HighlightBits` at slot `+0x00`;
* RGBA `[f32; 4]` at slot `+0x04`; and
* a slot stride of `0x34` bytes.

The table pointer is read from:

```text
module_base + G_OFFSETS.highlight_settings
```

The pointer must be in the canonical user address range
`0x0000_0000_0001_0000..0x0000_7fff_ffff_ffff`. A null, torn, or otherwise
implausible pointer aborts the pass before any write.

The 22 injected contexts are the nine loot colors plus death box (`31`–`40`,
with the source list selecting the specific subset), weapon model (`81`), and
the player contexts (`70`–`80`). The exact loot mapping is defined by the
`highlight_settings_inject` array; it intentionally uses
`HIGHLIGHT_LOOT_ENERGY` (`36`) for more than one loot category. The player
contexts are:

| Context | Meaning in the injected table |
| ---: | --- |
| 70 | Knocked, outline bits, `glow_*_knocked` color (injected, but not selected by the active player write set, which writes context 79 directly). |
| 71 | Visible-player outline, `glow_*_viz` color. |
| 72 | Not-visible filled player, `glow_*_not` color (available to the standalone selector). |
| 73 | Not-visible outline twin, `glow_*_not` color (available to the standalone selector). |
| 74–78 | Orange, white, blue, purple, and red armor-style colors. |
| 79 | Active player context: fill or outline bits, `glow_*_not` color. |
| 80 | Near-black player color. |

`HighlightBits::new` packs the inside function, outline function, radius, and
opacity/state flags into the four-byte function word. The current player bits
use `EntityVisible=true` and `AfterPostProcess=false`; those values are the
current measured configuration in the source. `player_filled_toggle` selects
between the fill and outline payload for context 79.

Colors are multiplied by `player_brightness` before being written. The weapon
context uses a frame-derived `rainbow_color(frame_count)`, so its table entry
can change as the action loop advances.

### Primary write and shadow mirror

The injector first writes each selected slot through the ordinary `IMemAccess`
mapping. It then reads the highlight-table pointer through the validated
secondary DTB. If that pointer is valid, the same slot payloads are mirrored
through the DTB-scoped memflow backend.

The mirror is content-aware:

* a table-address or payload change forces a mirror;
* every 16 injector calls it performs a periodic re-drive; and
* each slot is read first and written only when its function bits or color
  differ.

The shadow path requires `device.use_memflow_direct = true` and a configured
memflow backend. The shipped `IMemAccess` backends do not provide
DTB-scoped writes, so the primary pass remains the only write path when direct
memflow is unavailable.

The secondary DTB is accepted only after the CR3 resolver's shadow-origin probe
has confirmed it. The resolver caches and revalidates roots; when the root is
stale or unavailable, the mirror is skipped while the primary pass continues.

## Loot/item glow

`items_loop` wakes every 500 ms. Once the game is attached and the world is
ready, it snapshots `treasure_clues`, calls `process_loot` for each clue, and
publishes the resulting `(entity_handle, context_id)` vector on
`items_glow_tx`.

`process_loot` rejects null handles and clues farther than the configured aim
distance. It then maps the item ID to a color/context only when the corresponding
`Settings::loot` toggle is enabled. The mapping covers death boxes, armor,
healing items, ammunition, magazines, stocks, optics, hop-ups, grenades, and
weapons; the match arms in
[`workers/items.rs`](apexsky_dma/src/workers/items.rs) are the source of truth.

When `item_glow` is enabled and the item channel has changed, the action worker
writes each selected context as a `u32` at:

```text
loot_entity + 0x330  (m_highlightContextID)
```

The item path also waits for `highlight_injected`. That flag is set when
`inject_highlight` returns successfully; the injector can return early after a
guarded invalid-pointer check, so the flag means “an injection pass returned,”
not an independent guarantee that every table write landed. Use the table and
mirror diagnostics to confirm delivery.

## Weapon-model glow

When `weapon_model_glow` and `highlight_injected` are true, the action worker:

1. reads the local player's `cplayer_viewmodels` handle;
2. resolves that handle through the current entity list;
3. chooses a context from spectator state (`40` for spectators,
   `36` for allied spectators, otherwise `81`); and
4. writes the context as a `u32` at `view_model + 0x330`.

`weapon_model_transparent` controls the bits injected for context 81: true uses
outline-only/transparent bits, false uses the normal outline bits.

## What to inspect when glow is missing

Use the runtime's rolling log, then the glow diagnostics written by the action
worker:

| Diagnostic | What it answers |
| --- | --- |
| `log/glow_diag.txt` | Was the action loop running? Was the world/player ready? How many player targets existed? |
| `log/target_diag.txt` | Which aim rules rejected entities. Distance/FOV rejections do not apply to the active glow set, but this file helps identify stale entity data. |
| `log/ctx_defaults.txt` | One-shot dump of the untouched global highlight slots before the first injector overwrite. |
| `rolling.log.<date>` | Pointer validation, shadow-mirror transitions, table writes, and dual-root entity diagnostics. |

Interpret the common cases as follows:

* `player_glow=false`: no player collection and no player context writes.
* `world_ready=false`: all glow tasks are gated off.
* `targets=0` with `player_glow=true`: the local player or player map was not
  ready, so no entity write was attempted.
* `highlight table pointer is not a usable address`: the injector correctly
  skipped a transition where the game had not initialized the table.
* `highlight shadow mirror availability changed ... active=false`: primary
  writes may still work, but the renderer's shadow-mapped copy of the table is
  not being updated.
* A successful primary write with no visual change in a match usually indicates
  that the renderer is using the shadow-mapped table; verify direct memflow and
  the shadow-mirror log before changing offsets.

## Known implementation limits

* The active player path hardcodes context 79. The armor, favorite, knocked,
  and visible-player selector branches are not currently reachable from that
  path, although their global slots are still injected.
* The `player_glow_rainbow` name refers to the context ID and the helper's
  policy. The table's frame-based rainbow color is injected, but the active
  per-entity write remains context 79 and does not itself cycle contexts.
* Through-wall behavior depends on the game build's renderer, the selected
  function bits, and whether the global table was written through the mapping
  the renderer uses. A successful virtual write is not proof that the renderer
  consumed that copy; the primary/shadow diagnostics are required.
* The offsets are build-specific. In particular, `0x294` is the only
  per-entity highlight field currently used by the player path, while `0x330`
  is used by loot and view-model writes. Reusing offsets from a different game
  build can turn a cosmetic write into a gameplay-state or process-stability
  bug.

## Relevant source map

| Concern | Source |
| --- | --- |
| Task startup and channels | [`apexsky_dma/src/lib.rs`](apexsky_dma/src/lib.rs) |
| Action tick and all three write tasks | [`apexsky_dma/src/workers/actions.rs`](apexsky_dma/src/workers/actions.rs) |
| Player target filtering | `actions.rs`: `task_player_glow` and `collect_aim_targets` |
| Global table construction | `actions.rs`: `inject_highlight` |
| Highlight bit layout and context constants | [`apexsky_dma/src/apexdream/sdk/highlight.rs`](apexsky_dma/src/apexdream/sdk/highlight.rs) and [`apex1_common/src/data.rs`](apex1_common/src/data.rs) |
| Loot classification and 500 ms publisher | [`apexsky_dma/src/workers/items.rs`](apexsky_dma/src/workers/items.rs) |
| Feature menu | [`apexsky_dma/src/menu/glow_features_menu.rs`](apexsky_dma/src/menu/glow_features_menu.rs) |
| Color menu | [`apexsky_dma/src/menu/glow_color_menu.rs`](apexsky_dma/src/menu/glow_color_menu.rs) |
| CR3-scoped reads/writes | [`apexsky_dma/src/apexdream/base/cr3.rs`](apexsky_dma/src/apexdream/base/cr3.rs) |


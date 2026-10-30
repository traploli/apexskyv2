# Session notes — 2026-10-29 — CR3 shadow root, glow pipeline, evdev aim

Working tree: `/home/sad/Music/apexsky-next/apexsky` (our CR3 project — **not** Nika's; Nika's
blind-bruteforces, ours does not).
Game: Apex Legends **v3.0.3.65**, Windows 10 build 19045 guest under QEMU/KVM, memflow `kvm`
connector, `use_memflow_direct = true`.
Final build this session: `f1c12d2ceca7c433` (`dist-memflow-test-v11/apex1.spk`, 20:07).

---

## 0. Read this first — where the logs actually are

`apexsky_dma` logs to **`log/rolling.log.<date>`**, *not* `log/ohosky.log.<date>`.

```
grep -c "cr3:" log/ohosky.log.2026-10-29    →  0
grep -c "cr3:" log/rolling.log.2026-10-29   →  1,830,661
```

Several hours were lost this session to conclusions drawn from the wrong file ("the mirror never
ran", "bruteforce never ran" — both false). Check `rolling.log` first, always.

Diagnostic files written by the current build:

| file | contents |
|---|---|
| `log/rolling.log.<date>` | everything from `apexsky_dma` |
| `log/glow_diag.txt` | one line, overwritten: `targets`, `players_map`, `lplayer`, `lp_team` |
| `log/target_diag.txt` | appended: aim-filter accept/reject counts + per-player reject reasons |
| `log/ctx_defaults.txt` | one-shot: all 91 highlight context slots, flags decoded |
| `log/ctx_defaults-before-fix.txt` | an earlier capture, partly polluted by our own writes |
| `log/aim_diag.txt` | appended: input bitmap, button union, probe snapshot |

---

## 1. Ground truth established by measurement

These are facts, not inferences. Each was read out of the running game.

### 1.1 The kernel fast paths are dead on this setup

```
proc_info: Win32ProcessInfo { ... dtb1: 0x3025dc000, dtb2: 0xffffffffffffffff }
cr3: EAAC module not found in process — skipping EAAC array path
cr3: EPROCESS scan found no valid shadow DTB candidate — all offsets missed
```

`dtb2` is the invalid sentinel, the EAAC module isn't present, and the
`KPROCESS::UserDirectoryTableBase` candidate list misses. **Every shadow root must be found by
guest-physical sweep.** This is the root cause of all remaining intermittency.

### 1.2 The shadow-origin probe is healthy — `0x375c` is correct

Suspected stale three separate times, wrong every time. Live samples (`aim_diag.txt`), local player
descending from the dropship:

```
origin=[-10562.077, 45090.105, 24440.0]   cam=[-10561.377, 45150.406, 24501.002]
origin=[-11758.410, 22740.180, 13729.237] cam=[-11684.845, 22804.072, 13937.407]
origin=[-12594.570, 21638.050,  1721.496] cam=[-12594.747, 21637.010,  1777.496]
```

Both references live and tracking. `player_abs_origin_shadow_dup = 0x375c` is right.

### 1.3 Sweep cost is ~50 µs per candidate and does not compress

The PE check already short-circuits on `"MZ"`, so almost every page costs exactly one
DTB-scoped read:

| phase | pages | elapsed | µs/page |
|---|---|---|---|
| window | 16,384 | 0.83 s | 51 |
| band | 57,344 | 2.82 s | 49 |
| slice | 192,000 | 14.18 s | 74 |

### 1.4 Every root confirmed today

```
0x4dd4000  78MB   0x6c90000 108MB  0x6d71000 110MB  0x7eea000 126MB
0x8da2000 141MB   0x9443000 148MB  0xa774000 167MB  0xb114000 177MB
0xb12c000 177MB   0xb167000 177MB  0xb472000 180MB  0xb478000 180MB
0xc08e000 192MB   0xcd96000 205MB  0xd4b6000 212MB  0xd692000 214MB
0xee02000 238MB   0xee5d000 238MB  0xef34000 239MB  0xf108000 241MB
0xfc96000 252MB   0xfd36000 253MB  0xfef8000 255MB  0x11dec000 285MB
0x134cf000 323MB  0x14895000 344MB
outliers: 0x5592c000 1.4GB   0x6d0a4000 1.75GB   0x6d3b2000 1.75GB
```

**26 of 29 live between 78 MB and 344 MB.** Three roots repeated within the day
(`0x4dd4000`, `0xb114000`, `0xcd96000`), and two consecutive roots were `0xb114000` → `0xb12c000`
— 0x18000 apart, twenty-four pages.

Root lifetime: **15–68 s**, measured. Rotation is real; the UC figure of 12–20 s is the right order.

### 1.5 The global highlight context table (`log/ctx_defaults.txt`)

All 91 slots dumped inline before our first write. Two unambiguous patterns:

- **`EntityVisible` (bit 6) is set on every single slot the game ships.** All 90 non-zero slots.
  It is a required enable, not a "draw only when visible" gate.
- **`state` (low 6 bits of byte[3]) is 0 on every single slot.** None use 7.
- Nine slots set `AfterPostProcess` (bit 7), all with byte[3] = `0xC0`:
  `15, 39, 40, 41, 43, 48, 49, 51, 62`. Example: slot 48 = `0xC0407D7E`
  (inside 126, outline 125, radius 64).
- **Context 79's true default is `0x40ff9b7d`, colour `[0.34, 0.59, 0.86]`** — a blue non-player
  effect. 79 was never "the confirmed player context"; we are repurposing an unused slot.
  The code comment claiming otherwise is wrong and should be cleaned up.

### 1.6 The per-entity highlight offsets are wrong except one

One-shot discrete reads off a live target:

```
far_fade_0x254: 2.571419e-39    far_fade_0x25f: 7.916865e-39   ← both denormal garbage
vis_0x25c: 38 / 93             vis_0x267: 255                  ← neither is a 0..3 enum
current_0x294: 79               current_0x29f: 0
generic_0x2a0: [8, 28, 255, 8, 8, 28, 255, 8]
focused_0x2a8: 0
```

Heashey's map (UC 716406 #2252) and the +0xB-shifted variant are **both wrong on this build**.
`m_highlightCurrent = 0x294` is the only per-entity offset with runtime proof: our 79 persists
there and glow follows.

Note on the profile: `RecvTable.DT_BaseEntity HighlightSettings = 0x0000` with type
`DT_HighlightSettings`, so that sub-table's members (`m_highlightGenericContexts = 0x2a0`,
`m_highlightFocused = 0x2a8`) are entity-relative. They still do not match Heashey's
`0x295` / `0x29D`, and the profile lists only six highlight fields — no `m_highlightCurrent`,
no `m_highlightVisType`.

### 1.7 Offsets: the embedded profile is correct, the "newer" dump is not

The game is v3.0.3.65 and the embedded **v3.0.2.23** profile works on it (57 entities, local player
resolved). Of 10,285 values only 105 differ, and only 3 reach the 140 consumed fields:

- `checksum` — metadata
- `local_entity_handle` — old value demonstrably works
- `network_var_table_len` 300 → 350 — **harmful**

`network_var_table_len` is a **hash modulus**, not a bound:
`let mut name_index = (name.0 % entries.len() as u64) as usize;`. Setting 350 against a 300-entry
table sends every script-var lookup to the wrong bucket. Installing the "newer" profile produced
`players_map = 0` in a live match.

`dist-memflow-test-v11/Text File.txt` is **not** a drop-in profile — it fails to parse
(`Failed to read value 'InputSystem' in section 'Miscellaneous'`) and would panic at load.
`offsets.ini.disabled-v3.0.3.65`, `offsets-variantC.ini`, `offsets-variantD.ini` are parked and
inert.

Also note the runtime parses offsets through the shipped **`apex1_offsets_parser.wasm`**, not the
native parser. Verifying a profile with a Rust test that calls `export_offsets_from_str` proves
nothing about what the runtime will do.

### 1.8 Input: mouse buttons never reach the game's bitmap

`is_button_down` reads `CInputSystem::button_state`, a 128-bit bitmap in guest memory. Accumulated
over **every** tick of a full match:

```
bs[3] (codes 96..127, where MOUSE_LEFT=108 .. MOUSE_5=112) = 00000000
atk_ever = false     in_attack = 0 throughout
```

Decoded keyboard bits were correct (`11=A, 14=D, 15=E, 29=S, 33=W`), so the read and
`InputSystem = 0x36dad00` are right. **Mouse buttons simply never appear there.** The project's
`inputsystem.rs` defines only 33 constants with gaps at 37–106; the user's original
`triggerbot_key1 = 79` was bound to `KEY_LSHIFT`, not a mouse button.

---

## 2. What worked — fixed and verified

| # | Fix | Evidence |
|---|---|---|
| 1 | **Null table-pointer guard** (`is_plausible_user_va`, range not null check) | Was writing 44 primary + 22 shadow writes into the guard page every tick: `highlight_settings_ptr: "0x0", ctx79_addr: "0x100c"`. Instant crash, gone. |
| 2 | **Circular probe dependency** — `publish_shadow_probe` moved out of the `Ok(..)` arm of the shadow read | The probe required a working shadow root to validate a shadow root. It never published, so a decoy was committed every ~5 s. |
| 3 | **Validation storm throttled** — revalidate `last_good` at most every 100 ms | `streak: 14408, failing_for: 3008` → **4,790 validations/sec**, each a VM-exit, starving the reads validation depends on. Now `streak: 30`. Root survival 41 s → 102 s. |
| 4 | **Grace window serves the root instead of withholding it** — `Ok(last_good)` not `bail!`, window 3 s → 1 s | Flapping went from **4 blackouts in 3 s on the same DTB** to **2 in 3 min with a different DTB each time** (real rotation). |
| 5 | **Rejection blacklist expires after 60 s** (was permanent for the session); eviction drops the stalest entry, not the lowest address | `rejected: 26` while 26 of 29 roots sat inside the scanned band, and ten consecutive band scans found nothing across a 2-minute blackout. One bad probe verdict used to ban the real root forever. |
| 6 | **Search order: window before band** | Window 0.83 s and best hit rate (consecutive roots 24 pages apart) vs band 2.82 s. ~3 s off common-case re-acquisition. |
| 7 | **Band widened** `0x10000000` → `0x18000000` (268 → 402 MB) | `0x11dec000` (285 MB) and `0x14895000` (344 MB) were outside the old ceiling; `0xfef8000` cleared it by half a megabyte. |
| 8 | **Slice 192,000 → 49,152 pages** | 14.18 s per slice vs a 15–68 s root lifetime — every cycle arrived after the root moved. Now ~2.5 s. |
| 9 | **`BRUTEFORCE_YIELD_EVERY` 64 → 1024** | A slice took **13.49 s** at /64 vs **1.149 s** at /1024. The sleeps were the budget, not the reads. My own earlier "costs nothing" comment was wrong by 11.7×. |
| 10 | **Log spam** — fallthrough INFO moved inside the `compare_exchange` | 10,991 lines in 2.5 min, 1,428,406 across the day, most of a 447 MB `rolling.log`. Now once per actual sweep. |
| 11 | **Glow write set decoupled from the aim target list** | `target_diag.txt`: `accepted=0 rejected_players=60`, `counts: [("dist_not_normal", 53), ...]`, every one `alive=true dist=0.0 pos=[0.0, 0.0, 0.0]`. 53 living players deleted from the glow set because their shadow origin never resolved. Glow now filters on team/alive/not-self only. `targets` went to 47–56 of 54–59. |
| 12 | **22-slot shadow mirror restored** (I had cut it to 2) | The primary pass writes all 22 and lands on a decoyed page, so the renderer kept vanilla defaults for 20 contexts. Confirmed landing: `slots: 22, dtb: "0x4dd4000", elapsed_us: 554`. |
| 13 | **Three unverified per-entity writes removed** (`0x25c`, `0x29d`, `0x254`) | §1.6 — they land on unidentified memory. 52 targets × 3 writes × every tick. |
| 14 | **Read-compare-then-write** on both the mirror and the per-entity context (UC 716406 #1003) | Steady state is reads and zero writes. 22 slots is now cheaper than the 2-slot build that replaced it. |
| 15 | **Mirror uptime instrumented** | `highlight shadow mirror availability changed ... active: true/false` on every transition. Previously a one-shot per process, which is why the intermittency took so long to see. |

Mirror reduction also measurably helped survival at the time: 22 → 2 slots took the process from
dying within 16 ms of the first mirror to surviving 2 m 23 s — but the real causes were #1 and #13,
and #12 reverted the reduction once those were fixed.

---

## 3. What did not work — do not retry these

### 3.1 The highlight bit flags (four live attempts)

| byte[3] | flags | in the game's table? | result on screen |
|---|---|---|---|
| `0x47` | vis 1, app 0, state 7 | yes (31–40, 81) | **glow on all enemies**, occluded fade after 2–3 s |
| `0x87` | vis 0, app 1, state 7 | **no** | visible players only |
| `0xC7` | vis 1, app 1, state 7 | **no** | almost nothing |
| `0xC0` | vis 1, app 1, state 0 | yes — all 9 through-geometry slots | **nothing at all, for anyone** |

`0xC0` was the strongest case on paper: byte-for-byte the shipped through-geometry pattern, write
verified landing (`game_bits: 0xc0787d00`), all 56 enemies in the write set. It rendered nothing.

**Conclusion: setting `AfterPostProcess` on a slot is not sufficient to make it draw over
geometry.** Those nine slots also differ in their inside/outline **function ids** — slot 48 has
inside 126, ours has inside 0. A function id indexes the renderer's shader table, so asking for an
after-post-process draw with no inside pass plausibly leaves nothing to composite.

Current state: back to `(true, false)` = `0x47`, `player_glow_inside_opacity = 7`.

### 3.2 Reasoning errors worth recording

- **Circular ground truth.** The `slot baseline read` line reads the slot *through the shadow root
  inside the mirror loop*, i.e. after our own earlier pass already wrote it. It reported
  `game_vis: false, game_app: true` and that was our own value echoed back. The giveaway was in the
  same line: `outline: 125 radius: 120 inside: 0` is the settings file verbatim. Any "what does the
  game use?" read must happen **before** the first write, inline, not spawned (a spawned task races
  the write loop).
- **`[u8; 0x70]` wide reads produced no output.** Discrete small reads of the exact fields work
  everywhere else in this codebase; use those.
- **The +0xB offset shift** was inferred from `0x295`→`0x2a0` and `0x29D`→`0x2a8` agreeing. It
  looked like two independent confirmations. Measurement killed it (§1.6).
- **The distance-filter theory for "6 specific players"** was partly a dropship artifact: eight
  players at `dist=0.1 pos=[-21018.0, 19829.8, 2399.2]` are all genuinely at the same coordinate
  inside the dropship. Correct rejection, not the bug — though #11 was still a real bug.

### 3.3 Process exits with no crash signature

Several loader exits logged **zero** `panicked`, `SIGSEGV`, `fatal`, `abort`, or
`Searching for process` lines, and no write failures. Most were the user closing the loader to pick
up a new build. Unexplained silent exits have not been reproduced since #1 and #13 landed, but the
crash is **not** formally closed.

---

## 4. Open issues

### 4.1 PRIMARY — close players' glow fades 2–3 s after they break line of sight

**Symptom:** an enemy at close range glows while visible. They move into cover, visibility is lost,
and the glow fades out after roughly 2–3 seconds. Far players are unaffected. This is the original
reported bug and it is **still open**.

Ruled out by measurement:

- **not the write set** — `targets` = 47–56 of 54–59; every living enemy receives context 79, zero
  `dist_*` rejections in-match
- **not the per-entity context** — `current_0x294: 79` reads back on live targets
- **not the global slot bits** — four byte[3] variants tried, §3.1; the only one that renders at all
  is the original
- **not root loss** — roots live 15–68 s, nothing there produces a 2–3 s decay
- **not the per-entity offsets** — `0x25c` holds 93, `0x254` holds a denormal, `0x267`/`0x25f`/
  `0x29f` are no better (§1.6)

A 2–3 second fade on loss of visibility is a **fade**, and the only fade fields anyone has named
are `m_highlightFadeDuration` / `m_highlightFadeParity` (profile: `0x2ac` / `0x2b4` in
`DT_HighlightSettings`) and Heashey's `m_highlightFarFade`. None of those offsets has been validated
on this build.

**Next thing to try (not yet attempted):** stop rewriting slot 79's bits. Point entities at one of
the nine shipped through-geometry contexts — slot 48 ships `0xC0407D7E` with the game's own working
function ids — and write **only its colour**, never its bits. That is the one approach that requires
no bit guessing, since the renderer's own configuration does the work. Cost: outline size and inside
function become the game's rather than the user's, which is a reasonable trade for a working
baseline to tune from.

Second candidate: validate `m_highlightFadeDuration` by the same discrete-read method used in §1.6
before writing anything to it.

### 4.2 Glow is still not working on all players

Even with the write set at 47–56 of 54–59 and the 22-slot mirror restored, coverage is incomplete.
This may be the same root cause as 4.1 or a separate one — **it has not been isolated**. The
instrument to use is `glow_diag.txt` (`targets` vs `players_map`) cross-referenced against
`target_diag.txt` reject reasons, and the glow filter does **not** yet have the reason-string
diagnostic that the aim filter has. Adding it is the next diagnostic step.

### 4.3 evdev aim is NOT fully implemented

**Status: not working end to end. Treat as unimplemented.**

- `use_evdev = true`, `event_mouse_path = "/dev/input/by-id/usb-Razer_Razer_DeathAdder_Elite-event-mouse"`
- `EvdevAimActuator::connect` opens the device node read+write — evdev is a pure **output**
  actuator and works as such
- hotkeys are read from the **guest's** `CInputSystem::button_state` bitmap, and §1.8 proves mouse
  buttons never appear there. `is_button_down(MOUSE_RIGHT)` can therefore never return true, no
  matter what the key is bound to
- current bindings: `aimbot_key1 = 109` (MOUSE_RIGHT), `aimbot_key2 = 112` (MOUSE_5),
  `triggerbot_key1 = 108` (MOUSE_LEFT) — all in the dead range

**Required fix:** read the hotkey host-side from the evdev device (the same node already opened for
output, or a second read-only grab) instead of out of guest memory. Until that exists, no
mouse-bound aim key can ever fire. A keyboard-bound key in the defined range (`KEY_A = 11` ..
`KEY_Z = 36`) should work today and has not been tested.

### 4.4 Shadow root availability

With #4, #5, #6, #7 in, flapping is fixed and uptime is measurable, but the root is still absent
for long stretches (a 2-minute blackout was observed before #5). Re-acquisition after a rotation
that lands outside the band costs a full progressive sweep at ~192 MB/cycle.

Ideas, cheapest first:

1. **Persist `VALID_CACHE` to disk** and seed it at startup. `VALID_CACHE` is already checked before
   bruteforce and is cheap. Three roots repeated within one day, so the hit rate is modest but
   non-zero and the cost is near nothing.
2. **Fix the EPROCESS path.** Guest is Windows 10 build **19045** (`nt_build_number: 4026550885`
   = `0xF0000000 + 19045`), where `KPROCESS::UserDirectoryTableBase` is at `0x388` — which *is* in
   `KNOWN_OFFSETS`. It still misses, so either KPTI is off in this guest (making the field 0 and the
   whole concept inapplicable) or the validation is rejecting it. Worth one measurement: log the raw
   qword at each candidate offset instead of only reporting "all offsets missed".
3. Widen the EPROCESS scan range (currently `0x260`–`0x3C0`).

### 4.5 Cosmetic / cleanup

- `player_brightness = 55` makes colour channels `[55.0, 0.0, 0.0]`. **Every colour the game ships
  sits inside 0.0–1.0.** May be intentional overbright; if red looks blown out, lower
  `player_brightness` toward 1–5 — it is not a code issue.
- The comment claiming context 79 is "the only player context empirically confirmed to draw through
  walls on v3.0.3.65" is **wrong** (§1.5) and should be corrected.
- `player_glow()` is dead code (`warning: function player_glow is never used`).
- Pre-existing test failure, unrelated: `system::test_kdb_backlight` panics with
  `org.freedesktop.DBus.Error.UnknownMethod` — no UPower keyboard backlight on this machine.
  Suite is **12 passed / 1 failed**.

---

## 5. Build and stage procedure

```sh
cd /home/sad/Music/apexsky-next/apexsky
cargo +nightly check -p apexsky_dma          # nightly is mandatory: -Z flags in cargo config
cargo +nightly test  -p apexsky_dma --release
cargo +nightly build -p apexsky_dma --release

DIST=dist-memflow-test-v11
SO=target/release/libapexsky_dma.so
cp -f "$DIST/apex1.spk" "$DIST/apex1.spk.bak-$(date +%H%M)"
cp -f "$SO" "$DIST/apex1/native/x86_64-linux/libapexsky_dma.so"
cp -f "$SO" "$DIST/libapexsky_dma.so"
../ohosky_sdk/bin/x86_64-unknown-linux-gnu/ohosky_mod_packer \
    "$DIST/apex1.spk" "$DIST/apex1/manifest.json"
```

Run: `cd dist-memflow-test-v11 && ./skyrun --load apex1.spk --load apex1_sg.spk kvm`

**Do not run `tools/build_release.sh` against the live dist** — it does `rm -rf "$OUT"`.

To confirm which build is actually loaded, the loader extracts the `.so` to
`native_home/com.chettoy.apex1_libapexsky_dma.so_<content-hash>`; compare that hash across launches
(it is not the plain sha256 of our `.so`, but a new build always yields a new hash):

```sh
pid=$(ps -eo pid=,comm= | awk '$2=="skyrun"{print $1}')
grep -ao "apexsky_dma.so_[0-9a-f]\{16\}" /proc/$pid/maps | head -1
```

---

## 6. Settings touched this session

`dist-memflow-test-v11/settings.toml`, backups as `settings.toml.bak-HHMM`:

```diff
 [device]
-use_evdev = false
-event_mouse_path = ""
+use_evdev = true
+event_mouse_path = "/dev/input/by-id/usb-Razer_Razer_DeathAdder_Elite-event-mouse"

 [settings.hotkey_settings]
-aimbot_key1 = 0          → 109   # MOUSE_RIGHT  — cannot fire, see 4.3
-aimbot_key2 = 0          → 112   # MOUSE_5      — cannot fire, see 4.3
-triggerbot_key1 = 79     → 108   # was KEY_LSHIFT; MOUSE_LEFT cannot fire, see 4.3
```

`player_glow_inside_opacity` was tried at `0` and reverted to `7` (§3.1).

Nothing has been committed to git — the working tree is not a git repository.

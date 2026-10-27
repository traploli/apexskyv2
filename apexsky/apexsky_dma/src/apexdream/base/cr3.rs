/*!
Secondary-CR3 ("shadow") resolution for entity fields the game now hides behind a pointer into
a region mapped under a different page-table root than the one the primary connector attaches
to.

# Background

Reverse-engineered from `Nika` (closed-source Apex DMA cheat; memflow + a C++ `Memory` class —
symbols `Memory::check_pml4e`, `Memory::check_proc`, `Memory::testDtb`,
`Memory::bruteforceDtb`/`bruteforceDtbOld`, `Memory::setDtb`, `lastCorrectDtbPhysAddr`,
`validDtbCache`, `findNewDtb`), then corrected directly by Nika's author mid-port:

* `entity + 0x170` (apexsky: [`crate::apex1_common::offsets`]'s `centity_origin`, surfaced here
  as [`crate::apexdream::data::GameData::entity_origin`]) is no longer the origin vector — it is
  `absOriginShadowPtr`, read under the *primary* DTB exactly like everything else.
* The real `Vector3D` only resolves at `*absOriginShadowPtr` under a *secondary* DTB — a
  different page-table root than the one the primary connector is attached to.
* The secondary DTB is validated the same way `Memory::check_proc` validates the primary one
  (`"MZ"` DOS header + PE32+ `ImageBase` cross-check), or — specifically for the local player,
  which carries its own always-primary-CR3-readable duplicate — by *distance* against
  `player_abs_origin_shadow_dup` (`localPlayerBaseAddress+0x375C`) or the camera position
  (`localPlayerBaseAddress+0x1F74`, apexsky: `cplayer_camerapos`). Distance, not an exact match:
  the dup and the camera are not guaranteed to be sampled on the same tick as the shadow read.
* Nika does not bruteforce the secondary CR3 in its current build: it asks the hypervisor
  directly (`virsh qemu-monitor-command "<domain>" --hmp "info registers" | grep -io
  "CR3=................"`) — "the correct CR3 inevitably ends up on the CPU." apexsky already
  owns a QMP connection for actuation ([`crate::actuator::actuator_qmp::QmpAimActuator`]), so
  this module talks QMP directly instead of shelling out to `virsh`.
*/

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use qapi::futures::{QapiService, QmpStreamTokio};

use ohosky_api::common::dmalib::{self as dmalib, IMemAccess};

use crate::apexdream::state::Api;

/// Real DTB-scoped physical/virtual reads via a memflow connector plugin (e.g.
/// `libmemflow_kvm.so` — the exact plugin `nika` itself ships alongside its binary),
/// independent of whichever `IMemAccess` backend apexsky's *primary* reads go through.
///
/// This is what actually closes the gap the default-unsupported `IMemAccess::read_raw_with_dtb`
/// leaves open: a secondary-CR3 read doesn't need an OS layer or a process attach, just a
/// [`memflow::mem::PhysicalMemory`] connector and an explicit DTB handed to [`VirtualDma`] — no
/// bruteforce, no second `MemAccess` backend to stand up.
mod memflow_backend {
    use std::str::FromStr;
    use std::sync::Mutex;

    use memflow::mem::MemoryView;
    use memflow::os::{Os, Process};
    use memflow::plugins::{ConnectorArgs, Inventory, IntoProcessInstanceArcBox};
    use memflow::types::Address;

    struct Loaded {
        dir: String,
        name: String,
        args: String,
        process_name: String,
        process: IntoProcessInstanceArcBox<'static>,
        primary_dtb: u64,
    }

    static PROCESS: Mutex<Option<Loaded>> = Mutex::new(None);

    fn config() -> (bool, String, String, String) {
        let state = crate::global_state::G_STATE.lock().unwrap();
        let dc = &state.config.device;
        (
            dc.use_memflow_direct,
            dc.memflow_plugin_dir.clone(),
            dc.memflow_connector_name.clone(),
            dc.memflow_connector_args.clone(),
        )
    }

    pub fn is_configured() -> bool {
        config().0
    }

    // Keep the process instance alive. Nika's relevant behavior is the Win32 process layer's
    // Process::set_dtb, not a fresh VirtualDma built around the connector for every read.
    #[allow(deprecated)]
    fn load(
        dir: &str,
        name: &str,
        args: &str,
    ) -> anyhow::Result<(IntoProcessInstanceArcBox<'static>, u64)> {
        let mut inventory = if dir.is_empty() {
            Inventory::scan()
        } else {
            Inventory::scan_path(dir).map_err(|e| {
                anyhow::anyhow!(
                    "memflow Inventory::scan_path({dir:?}) failed: {}",
                    e.as_str()
                )
            })?
        };

        let args = if args.is_empty() {
            None
        } else {
            Some(ConnectorArgs::from_str(args).map_err(|e| {
                anyhow::anyhow!("invalid memflow connector args {args:?}: {}", e.as_str())
            })?)
        };

        let connector = inventory
            .create_connector(name, None, args.as_ref())
            .map_err(|e| anyhow::anyhow!("memflow connector {name:?} failed to load: {}", e.as_str()))?;

        let mut os = inventory
            .create_os("win32", Some(connector), None)
            .map_err(|e| anyhow::anyhow!("memflow win32 OS failed to load: {}", e.as_str()))?;
        let info = os
            .process_info_by_name("r5apex_dx12.exe")
            .map_err(|e| anyhow::anyhow!("memflow could not find Apex process: {}", e.as_str()))?;
        let primary_dtb = info.dtb1.to_umem() as u64;
        let process = os
            .into_process_by_info(info)
            .map_err(|e| anyhow::anyhow!("memflow could not open Apex process: {}", e.as_str()))?;
        Ok((process, primary_dtb))
    }

    fn process_name() -> &'static str {
        "r5apex_dx12.exe"
    }

    fn ensure_loaded(guard: &mut Option<Loaded>) -> anyhow::Result<()> {
        let (enabled, dir, name, args) = config();
        if !enabled {
            anyhow::bail!("device.use_memflow_direct is false");
        }

        if guard.as_ref().is_some_and(|loaded| {
            loaded.dir == dir && loaded.name == name && loaded.args == args
        }) {
            return Ok(());
        }

        let (process, primary_dtb) = load(&dir, &name, &args)?;
        *guard = Some(Loaded {
            dir,
            name,
            args,
            process_name: process_name().to_string(),
            process,
            primary_dtb,
        });
        Ok(())
    }

    fn read_raw_blocking(dtb: u64, addr: u64, len: usize) -> anyhow::Result<Vec<u8>> {
        let mut guard = PROCESS.lock().unwrap();
        ensure_loaded(&mut guard)?;
        let loaded = guard.as_mut().expect("memflow process initialized");
        if loaded.process_name != process_name() {
            anyhow::bail!("memflow process name mismatch");
        }

        loaded
            .process
            .set_dtb(Address::from(dtb), Address::invalid())
            .map_err(|e| anyhow::anyhow!("memflow set secondary dtb {dtb:#x} failed: {}", e.as_str()))?;

        let mut buf = vec![0u8; len];
        let result = loaded
            .process
            .read_raw_into(Address::from(addr), &mut buf)
            .map_err(|e| anyhow::anyhow!("memflow read at {addr:#x} via dtb {dtb:#x} failed: {}", e.as_str()));
        let restore = loaded
            .process
            .set_dtb(Address::from(loaded.primary_dtb), Address::invalid())
            .map_err(|e| anyhow::anyhow!("memflow restore primary dtb failed: {}", e.as_str()));
        restore?;
        result?;
        Ok(buf)
    }

    pub async fn read_raw(dtb: u64, addr: u64, len: usize) -> anyhow::Result<Vec<u8>> {
        tokio::task::spawn_blocking(move || read_raw_blocking(dtb, addr, len))
            .await
            .map_err(|e| anyhow::anyhow!("memflow read task panicked: {e}"))?
    }

    /// Mirror of [`read_raw_blocking`] for writes. UC thread 716406 reports EAC shadow-mapping
    /// the per-entity highlight struct the same way it shadow-maps `absOrigin`: during a real
    /// match the address under the *primary* DTB is a decoy the renderer doesn't consult, while
    /// the real backing memory the engine actually reads sits behind the secondary DTB. A plain
    /// `IMemAccess::write` (primary DTB) can therefore "succeed" — no read-back error — while
    /// changing nothing the renderer sees, which is consistent with glow working on firing-range
    /// dummies (not shadow-mapped the same way; no match, no stakes) and failing or flickering on
    /// real players. `IMemAccess` has no `write_with_dtb` counterpart at all (checked: only
    /// `read_raw_with_dtb` exists), so this is memflow-direct only — same prerequisite as reads.
    fn write_raw_blocking(dtb: u64, addr: u64, data: Vec<u8>) -> anyhow::Result<()> {
        let mut guard = PROCESS.lock().unwrap();
        ensure_loaded(&mut guard)?;
        let loaded = guard.as_mut().expect("memflow process initialized");
        if loaded.process_name != process_name() {
            anyhow::bail!("memflow process name mismatch");
        }

        loaded
            .process
            .set_dtb(Address::from(dtb), Address::invalid())
            .map_err(|e| anyhow::anyhow!("memflow set secondary dtb {dtb:#x} failed: {}", e.as_str()))?;

        let result = loaded
            .process
            .write_raw(Address::from(addr), &data)
            .map_err(|e| anyhow::anyhow!("memflow write at {addr:#x} via dtb {dtb:#x} failed: {}", e.as_str()));
        let restore = loaded
            .process
            .set_dtb(Address::from(loaded.primary_dtb), Address::invalid())
            .map_err(|e| anyhow::anyhow!("memflow restore primary dtb failed: {}", e.as_str()));
        restore?;
        result?;
        Ok(())
    }

    pub async fn write_raw(dtb: u64, addr: u64, data: Vec<u8>) -> anyhow::Result<()> {
        tokio::task::spawn_blocking(move || write_raw_blocking(dtb, addr, data))
            .await
            .map_err(|e| anyhow::anyhow!("memflow write task panicked: {e}"))?
    }
}

/// Dispatches a DTB-scoped read to the real memflow connector when `device.use_memflow_direct`
/// is set, falling back to [`IMemAccess::read_raw_with_dtb`] (default-unsupported on both of
/// apexsky's current backends, but a real extension point for a future one) otherwise.
async fn dtb_read_raw(api: &Api, dtb: u64, addr: u64, len: usize) -> anyhow::Result<Vec<u8>> {
    if memflow_backend::is_configured() {
        memflow_backend::read_raw(dtb, addr, len).await
    } else {
        api.mem_access
            .read_raw_with_dtb(dtb, addr, len, dmalib::PRIO_LOW, api.req_id)
            .await
            .map(|bytes| bytes.to_vec())
    }
}

async fn dtb_read<T: zerocopy::FromBytes>(api: &Api, dtb: u64, addr: u64) -> anyhow::Result<T> {
    let bytes = dtb_read_raw(api, dtb, addr, size_of::<T>()).await?;
    T::read_from_bytes(&bytes).map_err(|_| anyhow::anyhow!("short read at {addr:#x}"))
}

/// Write counterpart to [`dtb_read_raw`]. No `IMemAccess::write_with_dtb` exists on either
/// backend apexsky ships today, so unlike reads this has no non-memflow fallback at all — it
/// simply errors, matching [`resolve_secondary_dtb`]'s own error for "this backend can't do
/// DTB overrides."
async fn dtb_write_raw(_api: &Api, dtb: u64, addr: u64, data: Vec<u8>) -> anyhow::Result<()> {
    if memflow_backend::is_configured() {
        memflow_backend::write_raw(dtb, addr, data).await
    } else {
        anyhow::bail!(
            "shadow-scoped writes need `device.use_memflow_direct = true` — no IMemAccess \
             backend here supports write_with_dtb"
        )
    }
}

async fn dtb_write<T: zerocopy::IntoBytes + zerocopy::Immutable>(
    api: &Api,
    dtb: u64,
    addr: u64,
    value: &T,
) -> anyhow::Result<()> {
    dtb_write_raw(api, dtb, addr, value.as_bytes().to_vec()).await
}

/// Guest-physical address mask Nika applies to a raw 16-hex-digit CR3 string pulled from
/// `info registers`: clears the low 12 bits (page offset) and anything above bit 51 (above the
/// architectural 52-bit `MAXPHYADDR` ceiling), leaving a page-aligned physical base.
const CR3_PAGE_MASK: u64 = 0xffffffffff000;

/// Upper bound on `e_lfanew` before it's trusted as an offset: an unvalidated CR3 guess can
/// make this read come back as attacker-irrelevant-but-still-untrusted memory, and without a
/// bound a huge `e_lfanew` would walk the next read arbitrarily far from `module_base`.
const MAX_E_LFANEW: u32 = 0x1000;

const PE_MZ_MAGIC: u16 = 0x5A4D;

/// Last secondary DTB known to validate, and everything seen valid at least once this session.
/// Mirrors Nika's `lastCorrectDtbPhysAddr` / `validDtbCache`: a shuffle that lands back on a
/// previously-seen value is re-confirmed with one read instead of re-querying QMP.
static LAST_GOOD: AtomicU64 = AtomicU64::new(0);
static VALID_CACHE: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// Drops `dtb` from both [`LAST_GOOD`] and [`VALID_CACHE`]. Clearing `LAST_GOOD` alone used to
/// leave the dead value in the cache: [`resolve_secondary_dtb`] would then re-walk to it on
/// every subsequent shuffle, spend a PE-header read confirming it's still dead, and only then
/// fall through to QMP — one wasted round trip per entity per rotation, stacking into exactly
/// the "players go missing during a CR3 shuffle" symptom this module exists to prevent.
fn invalidate_cached_dtb(dtb: u64) {
    if dtb != 0 {
        LAST_GOOD.compare_exchange(dtb, 0, Ordering::AcqRel, Ordering::Acquire).ok();
        VALID_CACHE.lock().retain(|&candidate| candidate != dtb);
    }
}

/// Parses `"...CR3=XXXXXXXXXXXXXXXX..."` (exactly what x86-64 `info registers` prints) and
/// page-aligns it. Returns `None` on anything shorter or non-hex, which also covers the
/// `grep`-miss fallback string Nika substitutes ("0000000000000000" is 16 zero digits — valid
/// hex, but masks to physical address `0`, which [`validate_dtb_by_pe_header`] will reject).
fn parse_cr3_from_hmp_registers(output: &str) -> Option<u64> {
    let idx = output.find("CR3=")?;
    let hex = output.get(idx + 4..idx + 4 + 16)?;
    if hex.len() != 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(hex, 16).ok().map(|v| v & CR3_PAGE_MASK)
}

/// Runs `human-monitor-command "info registers"` over a fresh QMP connection and extracts
/// `CR3=`. One-shot by design, the same cost model as Nika's `exec_cmd("virsh ...")`: CR3 only
/// needs re-asking when a validation fails, not on every entity read, so a one-shot connection
/// is simpler and safer than holding a second long-lived client on the same QMP socket as
/// [`crate::actuator::actuator_qmp::QmpAimActuator`].
async fn query_cr3_via_qmp(socket_addr: &str) -> anyhow::Result<u64> {
    #[cfg(unix)]
    let stream = QmpStreamTokio::open_uds(socket_addr).await?;
    #[cfg(not(unix))]
    let stream = QmpStreamTokio::open_tcp(socket_addr).await?;
    let stream = stream.negotiate().await?;
    let (qmp, _handle): (QapiService<_>, _) = stream.spawn_tokio();

    let output = qmp
        .execute(qapi::qmp::human_monitor_command {
            command_line: "info registers".to_string(),
            cpu_index: None,
        })
        .await?;

    parse_cr3_from_hmp_registers(&output)
        .ok_or_else(|| anyhow::anyhow!("no CR3= found in `info registers` output"))
}

/// The same `"MZ"` + 64-bit `ImageBase` cross-check `Memory::check_proc` runs against the
/// primary DTB, run here against a secondary-DTB candidate: DOS header magic at offset 0,
/// `e_lfanew` (`IMAGE_DOS_HEADER::e_lfanew`) at `+0x3C`, and the PE32+
/// `OptionalHeader::ImageBase` at `e_lfanew + 0x30` (4-byte `Signature` + 20-byte
/// `IMAGE_FILE_HEADER` + the 24-byte fixed prefix of `IMAGE_OPTIONAL_HEADER64` ahead of
/// `ImageBase`). Any read failure (including the backend not supporting DTB override at all)
/// counts as "does not validate" rather than propagating an error — the caller only needs a
/// yes/no.
async fn validate_dtb_by_pe_header(api: &Api, dtb: u64, module_base: u64) -> bool {
    if dtb == 0 {
        return false;
    }

    let Ok(mz) = dtb_read::<u16>(api, dtb, module_base).await else {
        return false;
    };
    if mz != PE_MZ_MAGIC {
        return false;
    }

    let Some(e_lfanew_addr) = module_base.checked_add(0x3C) else {
        return false;
    };
    let Ok(e_lfanew) = dtb_read::<u32>(api, dtb, e_lfanew_addr).await else {
        return false;
    };
    if e_lfanew == 0 || e_lfanew > MAX_E_LFANEW {
        return false;
    }

    let Some(image_base_addr) = module_base
        .checked_add(e_lfanew as u64)
        .and_then(|addr| addr.checked_add(0x30))
    else {
        return false;
    };
    let Ok(image_base) = dtb_read::<u64>(api, dtb, image_base_addr).await else {
        return false;
    };
    image_base == module_base
}

/// Largest plausible Source-engine world coordinate on any Apex map (Hammer units; King's
/// Canyon/World's Edge playable bounds top out well under this). A freshly spawned/respawned
/// entity's shadow pointer can validate (PE header check only proves the *DTB* is current, not
/// that *this specific* shadow-region page has been migrated yet) while still dereferencing to
/// leftover physical memory that hasn't been dragged into the shadow mapping — reported on
/// UnknownCheats thread 716406 (posts #2455/#2457) as "bone matrix garbage until next [CR3]
/// rotation" on NPC respawn. That garbage is finite, so `is_finite()` alone lets it through;
/// bound the magnitude too.
const MAX_PLAUSIBLE_COORD: f32 = 131_072.0;

/// Squared-distance sanity check for a shadow read that can't be cross-checked by PE header —
/// e.g. the local player's own `player_abs_origin_shadow_dup` or camera position, which can
/// legitimately differ from the shadow copy by up to a tick of movement. Never compare these
/// for exact equality.
pub fn validate_vec3(value: [f32; 3]) -> anyhow::Result<[f32; 3]> {
    if !value.iter().all(|component| component.is_finite()) {
        anyhow::bail!("non-finite shadow origin: {value:?}");
    }
    if value
        .iter()
        .any(|component| component.abs() > MAX_PLAUSIBLE_COORD)
    {
        anyhow::bail!("implausible shadow origin (not yet migrated into shadow region?): {value:?}");
    }
    Ok(value)
}

pub fn validate_by_distance(candidate: [f32; 3], reference: [f32; 3], max_dist: f32) -> bool {
    if !max_dist.is_finite() || max_dist < 0.0 {
        return false;
    }
    let Ok(candidate) = validate_vec3(candidate) else {
        return false;
    };
    let Ok(reference) = validate_vec3(reference) else {
        return false;
    };
    let dx = candidate[0] - reference[0];
    let dy = candidate[1] - reference[1];
    let dz = candidate[2] - reference[2];
    (dx * dx + dy * dy + dz * dz).is_finite() && (dx * dx + dy * dy + dz * dz) <= max_dist * max_dist
}

/// Read an absOrigin shadow pointer and reject malformed vectors before callers publish them.
pub async fn read_shadow_origin(
    api: &Api,
    shadow_ptr_addr: u64,
    module_base: u64,
) -> anyhow::Result<[f32; 3]> {
    validate_vec3(read_shadow::<[f32; 3]>(api, shadow_ptr_addr, module_base).await?)
}

/// Resolves a secondary DTB that currently validates against `module_base`'s PE header, or
/// `Err` if: this [`IMemAccess`] backend can't override DTB per-request, QMP isn't configured
/// (`device.use_qemu_qmp`), or no candidate — cached or freshly queried — validates.
pub async fn resolve_secondary_dtb(api: &Api, module_base: u64) -> anyhow::Result<u64> {
    if !memflow_backend::is_configured() && !api.mem_access.supports_dtb_override() {
        anyhow::bail!(
            "secondary CR3 reads need `device.use_memflow_direct = true`, or an IMemAccess \
             backend that supports read_with_dtb"
        );
    }

    let last_good = LAST_GOOD.load(Ordering::Acquire);
    if last_good != 0 && validate_dtb_by_pe_header(api, last_good, module_base).await {
        return Ok(last_good);
    }
    // A failed validation is evidence that the cached translation context rotated.
    // Do not keep retrying the same stale candidate during every entity update.
    if last_good != 0 {
        LAST_GOOD.store(0, Ordering::Release);
    }

    let cached: Vec<u64> = VALID_CACHE.lock().clone();
    for candidate in cached {
        if candidate != last_good && validate_dtb_by_pe_header(api, candidate, module_base).await
        {
            LAST_GOOD.store(candidate, Ordering::Release);
            return Ok(candidate);
        }
    }

    let qmp_addr = {
        let state = crate::global_state::G_STATE
            .lock()
            .map_err(|_| anyhow::anyhow!("G_STATE mutex poisoned"))?;
        if !state.config.device.use_qemu_qmp {
            anyhow::bail!("secondary CR3 resolution needs `device.use_qemu_qmp = true`");
        }
        state.config.device.qemu_qmp_addr.clone()
    };

    let candidate = query_cr3_via_qmp(&qmp_addr).await?;
    if !validate_dtb_by_pe_header(api, candidate, module_base).await {
        anyhow::bail!("QMP-reported CR3 {candidate:#x} did not validate against the module header");
    }

    LAST_GOOD.store(candidate, Ordering::Release);
    VALID_CACHE.lock().push(candidate);
    Ok(candidate)
}

/// Dereferences a shadow pointer (already read under the primary DTB) as `T`, under a freshly
/// resolved secondary DTB. On any failure — backend can't override DTB, QMP unavailable,
/// validation failed, or the shadow dereference itself failed — returns `Err`; callers must
/// leave the previous value in place rather than write a half-read/garbage result, matching
/// Nika's fallback-to-last-good behaviour instead of zeroing the field.
pub async fn read_shadow<T: zerocopy::FromBytes>(
    api: &Api,
    shadow_ptr_addr: u64,
    module_base: u64,
) -> anyhow::Result<T> {
    if shadow_ptr_addr == 0 {
        anyhow::bail!("null shadow pointer");
    }
    let dtb = resolve_secondary_dtb(api, module_base).await?;
    match dtb_read(api, dtb, shadow_ptr_addr).await {
        Ok(value) => Ok(value),
        Err(first_error) => {
            // A valid PE-header check can still race a rotating shadow mapping. The
            // dereference failure is the stronger signal: discard that candidate and
            // reacquire once before publishing failure. This is the missing retry edge
            // in the old implementation and is what prevents a stale DTB from starving
            // players that move behind cover.
            invalidate_cached_dtb(dtb);
            let refreshed = resolve_secondary_dtb(api, module_base).await.map_err(|refresh_error| {
                anyhow::anyhow!(
                    "shadow read failed via dtb {dtb:#x}: {first_error}; refresh failed: {refresh_error}"
                )
            })?;
            dtb_read(api, refreshed, shadow_ptr_addr)
                .await
                .map_err(|retry_error| anyhow::anyhow!(
                    "shadow read failed via dtb {dtb:#x}: {first_error}; retry via {refreshed:#x} failed: {retry_error}"
                ))
        }
    }
}

/// Writes `value` at `addr` under the secondary DTB instead of the primary one. Unlike
/// [`read_shadow`], `addr` is used directly rather than dereferenced — the per-entity highlight
/// struct isn't reached through a separate shadow pointer the way `absOrigin` is; the hypothesis
/// (UC thread 716406, late-Sept/Oct 2026 posts discussing "CR3 Shadow Region stuff" in the same
/// breath as "annoying for glow esp") is that the *same virtual address* the primary connector
/// already has (`target_ptr + offset`) is backed by different physical pages depending on which
/// page-table root resolves it — a primary-DTB write lands on a decoy the renderer never reads,
/// while the renderer's actual backing memory sits behind the secondary DTB, same as origin.
/// Requires `device.use_memflow_direct = true`; there is no non-memflow fallback for writes.
pub async fn write_shadow<T: zerocopy::IntoBytes + zerocopy::Immutable>(
    api: &Api,
    addr: u64,
    module_base: u64,
    value: &T,
) -> anyhow::Result<()> {
    if addr == 0 {
        anyhow::bail!("null shadow write target");
    }
    let dtb = resolve_secondary_dtb(api, module_base).await?;
    match dtb_write(api, dtb, addr, value).await {
        Ok(()) => Ok(()),
        Err(first_error) => {
            // Same reasoning as read_shadow's retry: a validated DTB can still rotate out from
            // under us between resolve and write. One re-resolve-and-retry before giving up.
            invalidate_cached_dtb(dtb);
            let refreshed = resolve_secondary_dtb(api, module_base).await.map_err(|refresh_error| {
                anyhow::anyhow!(
                    "shadow write failed via dtb {dtb:#x}: {first_error}; refresh failed: {refresh_error}"
                )
            })?;
            dtb_write(api, refreshed, addr, value)
                .await
                .map_err(|retry_error| anyhow::anyhow!(
                    "shadow write failed via dtb {dtb:#x}: {first_error}; retry via {refreshed:#x} failed: {retry_error}"
                ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_page_aligns_cr3() {
        assert_eq!(
            parse_cr3_from_hmp_registers("CR0=80050033 CR2=0000000000000000 CR3=000000007a40e123 CR4=00370ef8"),
            Some(0x7a40e000),
        );
    }

    #[test]
    fn rejects_short_or_non_hex() {
        assert_eq!(parse_cr3_from_hmp_registers("CR3=deadbeef"), None);
        assert_eq!(parse_cr3_from_hmp_registers("no cr3 here"), None);
        assert_eq!(
            parse_cr3_from_hmp_registers("CR3=zzzzzzzzzzzzzzzz"),
            None
        );
    }

    #[test]
    fn grep_miss_fallback_masks_to_null_physaddr() {
        // Nika's shell-command fallback string when `grep -io` finds nothing.
        assert_eq!(
            parse_cr3_from_hmp_registers("CR3=0000000000000000"),
            Some(0),
        );
    }

    #[test]
    fn rejects_non_finite_vectors_and_distance_inputs() {
        assert!(validate_vec3([1.0, 2.0, 3.0]).is_ok());
        assert!(validate_vec3([f32::NAN, 2.0, 3.0]).is_err());
        assert!(!validate_by_distance([1.0, 2.0, 3.0], [1.0, f32::INFINITY, 3.0], 8.0));
        assert!(!validate_by_distance([1.0, 2.0, 3.0], [1.0, 2.0, 3.0], f32::NAN));
    }

    #[test]
    fn rejects_finite_but_implausible_unmigrated_garbage() {
        // Still-finite leftover physical memory from a not-yet-shadow-migrated respawn
        // (UC thread 716406 #2455/#2457) must not pass just because it's not NaN/inf.
        assert!(validate_vec3([1_000_000.0, 0.0, 0.0]).is_err());
        assert!(validate_vec3([0.0, -500_000.0, 0.0]).is_err());
        assert!(validate_vec3([131_072.0 + 1.0, 0.0, 0.0]).is_err());
        assert!(validate_vec3([131_072.0, -131_072.0, 0.0]).is_ok());
    }

    #[test]
    fn distance_check_is_not_exact_equality() {
        let shadow = [100.0, 200.0, 50.0];
        let dup_slightly_stale = [100.4, 199.8, 50.1];
        assert!(validate_by_distance(shadow, dup_slightly_stale, 8.0));
        assert!(!validate_by_distance(shadow, [500.0, 200.0, 50.0], 8.0));
    }
}

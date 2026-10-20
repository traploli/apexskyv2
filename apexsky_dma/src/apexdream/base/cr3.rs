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

    use memflow::architecture::x86::x64;
    use memflow::mem::{MemoryView, VirtualDma};
    use memflow::plugins::{ConnectorArgs, ConnectorInstanceArcBox, Inventory};
    use memflow::types::Address;

    struct Loaded {
        dir: String,
        name: String,
        args: String,
        connector: ConnectorInstanceArcBox<'static>,
    }

    static CONNECTOR: Mutex<Option<Loaded>> = Mutex::new(None);

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

    // *memflow 0.2.4 deprecated `create_connector` in favor of `instantiate_connector`, but kept
    // it working — only the receiver's mutability changed (it now tracks the library handle it
    // instantiates from), so this stays on the stable 0.2 name rather than chase a 0.2.4-only
    // rename.*
    #[allow(deprecated)]
    fn load(dir: &str, name: &str, args: &str) -> anyhow::Result<ConnectorInstanceArcBox<'static>> {
        let mut inventory = if dir.is_empty() {
            Inventory::scan()
        } else {
            Inventory::scan_path(dir).map_err(|e| {
                anyhow::anyhow!("memflow Inventory::scan_path({dir:?}) failed: {}", e.as_str())
            })?
        };

        let args = if args.is_empty() {
            None
        } else {
            Some(ConnectorArgs::from_str(args).map_err(|e| {
                anyhow::anyhow!("invalid memflow connector args {args:?}: {}", e.as_str())
            })?)
        };

        inventory
            .create_connector(name, None, args.as_ref())
            .map_err(|e| anyhow::anyhow!("memflow connector {name:?} failed to load: {}", e.as_str()))
    }

    /// Returns a clone of the cached connector, (re)loading it if the config changed or nothing
    /// has been loaded yet. Connectors are cheap to clone (an `Arc`-backed handle), so every
    /// caller gets its own handle without contending on a shared one.
    fn connector() -> anyhow::Result<ConnectorInstanceArcBox<'static>> {
        let (enabled, dir, name, args) = config();
        if !enabled {
            anyhow::bail!("device.use_memflow_direct is false");
        }

        let mut guard = CONNECTOR.lock().unwrap();
        if let Some(loaded) = guard.as_ref() {
            if loaded.dir == dir && loaded.name == name && loaded.args == args {
                return Ok(loaded.connector.clone());
            }
        }

        let connector = load(&dir, &name, &args)?;
        let out = connector.clone();
        *guard = Some(Loaded {
            dir,
            name,
            args,
            connector,
        });
        Ok(out)
    }

    /// Reads `len` bytes at `addr`, translated through `dtb` — no process attach, no OS layer,
    /// just `PhysicalMemory` + an explicit x86-64 4-level page-table walk. *`VirtualDma` is
    /// rebuilt fresh per call: it owns a small `Bump` arena scoped to a single read, so reusing
    /// one across calls buys nothing and rebuilding keeps concurrent callers from sharing any
    /// mutable state.*
    fn read_raw_blocking(dtb: u64, addr: u64, len: usize) -> anyhow::Result<Vec<u8>> {
        let connector = connector()?;
        let translator = x64::new_translator(Address::from(dtb));
        let mut virt_mem = VirtualDma::new(connector, x64::ARCH, translator);

        let mut buf = vec![0u8; len];
        virt_mem
            .read_raw_into(Address::from(addr), &mut buf)
            .map_err(|e| {
                anyhow::anyhow!(
                    "memflow read at {addr:#x} via dtb {dtb:#x} failed: {}",
                    e.as_str()
                )
            })?;
        Ok(buf)
    }

    pub async fn read_raw(dtb: u64, addr: u64, len: usize) -> anyhow::Result<Vec<u8>> {
        tokio::task::spawn_blocking(move || read_raw_blocking(dtb, addr, len))
            .await
            .map_err(|e| anyhow::anyhow!("memflow read task panicked: {e}"))?
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

    let Ok(e_lfanew) = dtb_read::<u32>(api, dtb, module_base + 0x3C).await else {
        return false;
    };
    if e_lfanew == 0 || e_lfanew > MAX_E_LFANEW {
        return false;
    }

    let Ok(image_base) = dtb_read::<u64>(api, dtb, module_base + e_lfanew as u64 + 0x30).await
    else {
        return false;
    };
    image_base == module_base
}

/// Squared-distance sanity check for a shadow read that can't be cross-checked by PE header —
/// e.g. the local player's own `player_abs_origin_shadow_dup` or camera position, which can
/// legitimately differ from the shadow copy by up to a tick of movement. Never compare these
/// for exact equality.
pub fn validate_by_distance(candidate: [f32; 3], reference: [f32; 3], max_dist: f32) -> bool {
    let dx = candidate[0] - reference[0];
    let dy = candidate[1] - reference[1];
    let dz = candidate[2] - reference[2];
    (dx * dx + dy * dy + dz * dz) <= max_dist * max_dist
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
    dtb_read(api, dtb, shadow_ptr_addr).await
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
    fn distance_check_is_not_exact_equality() {
        let shadow = [100.0, 200.0, 50.0];
        let dup_slightly_stale = [100.4, 199.8, 50.1];
        assert!(validate_by_distance(shadow, dup_slightly_stale, 8.0));
        assert!(!validate_by_distance(shadow, [500.0, 200.0, 50.0], 8.0));
    }
}

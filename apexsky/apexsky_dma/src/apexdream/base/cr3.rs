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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;

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

    use memflow::mem::{MemoryView, PhysicalMemory};
    use memflow::os::{Os, Process};
    use memflow::plugins::{ConnectorArgs, ConnectorInstanceArcBox, Inventory, IntoProcessInstanceArcBox};
    use memflow::types::{Address, PhysicalAddress};

    struct Loaded {
        dir: String,
        name: String,
        args: String,
        process_name: String,
        process: IntoProcessInstanceArcBox<'static>,
        /// Cloned physical-memory connector, kept for EPROCESS page-table walks.
        ///
        /// The Win32 OS process layer scopes `read_raw_into` to user-space VAs, so reading a
        /// kernel-space address like EPROCESS (0xffff...) through the process handle fails
        /// silently with a translation error. The physical connector bypasses that restriction:
        /// we do the VA→PA walk ourselves (primary DTB as root), then read the physical cell
        /// directly — no kernel-VA gating, no Win32 layer involved.
        phys_connector: ConnectorInstanceArcBox<'static>,
        primary_dtb: u64,
        /// `ProcessInfo::dtb2` — the process's second page-table root
        /// (`KPROCESS::UserDirectoryTableBase`) as reported by memflow's Win32 OS layer.
        /// Zero when the OS layer did not resolve one. Validated before use.
        secondary_dtb: u64,
        /// Virtual address of the game's EPROCESS structure in kernel VA-space, as reported by
        /// memflow's Win32 OS layer (`ProcessInfo::address`). Used by
        /// [`find_userdir_dtb_from_eprocess`] to read `KPROCESS::UserDirectoryTableBase` — the
        /// KPTI shadow page-table root — without a full guest-physical bruteforce. This is Nika's
        /// "cached DTB straight from PID": read a kernel struct field, not a physical-range scan.
        eprocess_addr: u64,
    }

    /// Walks one level of a 4-level x86-64 page table using the physical connector. Returns the
    /// guest-physical address that `va` maps to under `dtb`, or `None` on any missing/invalid
    /// entry. Handles 1 GiB (PDPTE.PS) and 2 MiB (PDE.PS) huge pages.
    ///
    /// *Watch for A/D bit noise in bits 5–6 of PTEs — clear them before using the base address
    /// or the alignment check will fail on accessed/dirty pages.*
    fn page_walk_to_phys(
        phys: &mut ConnectorInstanceArcBox<'static>,
        dtb: u64,
        va: u64,
    ) -> Option<u64> {
        const PRESENT: u64 = 1;
        const HUGE: u64 = 1 << 7;
        const ADDR_MASK_4K: u64 = 0x000F_FFFF_FFFF_F000;
        const PAGE_OFFSET_4K: u64 = 0xFFF;
        const PAGE_OFFSET_2M: u64 = 0x1F_FFFF;
        const PAGE_OFFSET_1G: u64 = 0x3FFF_FFFF;

        let pml4_idx = (va >> 39) & 0x1FF;
        let pdpt_idx = (va >> 30) & 0x1FF;
        let pd_idx   = (va >> 21) & 0x1FF;
        let pt_idx   = (va >> 12) & 0x1FF;

        let mut pml4e = 0u64;
        phys.phys_read_into(PhysicalAddress::from(dtb + pml4_idx * 8), &mut pml4e).ok()?;
        if pml4e & PRESENT == 0 { return None; }
        let pdpt_base = pml4e & ADDR_MASK_4K;

        let mut pdpte = 0u64;
        phys.phys_read_into(PhysicalAddress::from(pdpt_base + pdpt_idx * 8), &mut pdpte).ok()?;
        if pdpte & PRESENT == 0 { return None; }
        if pdpte & HUGE != 0 {
            // 1 GiB page
            let base = pdpte & 0x000F_FFFF_C000_0000;
            return Some(base | (va & PAGE_OFFSET_1G));
        }
        let pd_base = pdpte & ADDR_MASK_4K;

        let mut pde = 0u64;
        phys.phys_read_into(PhysicalAddress::from(pd_base + pd_idx * 8), &mut pde).ok()?;
        if pde & PRESENT == 0 { return None; }
        if pde & HUGE != 0 {
            // 2 MiB page
            let base = pde & 0x000F_FFFF_FFE0_0000;
            return Some(base | (va & PAGE_OFFSET_2M));
        }
        let pt_base = pde & ADDR_MASK_4K;

        let mut pte = 0u64;
        phys.phys_read_into(PhysicalAddress::from(pt_base + pt_idx * 8), &mut pte).ok()?;
        if pte & PRESENT == 0 { return None; }
        let page_base = pte & ADDR_MASK_4K;
        Some(page_base | (va & PAGE_OFFSET_4K))
    }

    fn phys_read_u64(phys: &mut ConnectorInstanceArcBox<'static>, phys_addr: u64) -> Option<u64> {
        let mut val = 0u64;
        phys.phys_read_into(PhysicalAddress::from(phys_addr), &mut val).ok()?;
        Some(val)
    }

    fn phys_read_u16(phys: &mut ConnectorInstanceArcBox<'static>, phys_addr: u64) -> Option<u16> {
        let mut val = 0u16;
        phys.phys_read_into(PhysicalAddress::from(phys_addr), &mut val).ok()?;
        Some(val)
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

    /// The process's second page-table root exactly as memflow's Win32 OS layer reported it
    /// (`ProcessInfo::dtb2`), validated against `module_base`'s PE header before it is trusted.
    ///
    /// This is the cheapest possible shadow-DTB source: memflow already parsed it out of the
    /// kernel structures at attach time, so there is nothing to scan and nothing to sweep. It
    /// is tried ahead of [`find_userdir_dtb_from_eprocess_blocking`] (which hand-reads candidate
    /// EPROCESS offsets) and far ahead of [`bruteforce_secondary_dtb`] (which walks up to
    /// 0x800000000 of guest-physical RAM and is the reason the TUI reports
    /// "this can take a while").
    ///
    /// Returns `None` when memflow reported no second root (`0`), when it merely echoes the
    /// primary root, or when it fails PE validation — in which case the existing scan and
    /// bruteforce fallbacks still run, so this is strictly additive.
    pub fn dtb2_shadow_candidate(module_base: u64) -> Option<u64> {
        let (secondary, primary) = {
            let guard = PROCESS.lock().unwrap();
            let loaded = guard.as_ref()?;
            (loaded.secondary_dtb, loaded.primary_dtb)
        };
        // memflow reports "this process has no second page-table root" as `Address::invalid()`,
        // which arrives here as `u64::MAX` — not as 0. Masking it produces 0x7ffffffffffff000,
        // which is neither zero nor equal to the primary root, so it sailed through the guards
        // below and spent a full PE-header validation (four DTB-scoped reads) page-walking a
        // guaranteed-garbage root on every single call. Observed live: `dtb2: 0xffffffffffffffff`
        // on every `ProcessInfo page-table roots` line.
        if secondary == u64::MAX {
            tracing::debug!("cr3: memflow reports no second page-table root for this process");
            return None;
        }
        // Mask PCID (CR3[11:0]) and the no-flush bit 63 before treating this as an address.
        // Windows runs CR4.PCIDE=1, so a raw page-table root read out of a kernel struct can
        // carry a PCID in its low bits and/or bit 63 set. Rejecting on "not page aligned"
        // throws away perfectly good roots; masking is what the hardware does.
        let secondary = secondary & !0xFFFu64 & !(1u64 << 63);
        if secondary == 0 || secondary == primary {
            return None;
        }
        if check_mz_under_phys_dtb(secondary, module_base) {
            tracing::info!(
                dtb = %format!("{secondary:#x}"),
                "cr3: using ProcessInfo::dtb2 as shadow DTB (no scan, no bruteforce)"
            );
            return Some(secondary);
        }
        tracing::debug!(
            dtb = %format!("{secondary:#x}"),
            "cr3: ProcessInfo::dtb2 did not validate against PE header"
        );
        None
    }

    /// Scans the game's EPROCESS for `KPROCESS::UserDirectoryTableBase` — the KPTI shadow
    /// page-table root — by reading candidate 8-byte-aligned slots in the range
    /// `[EPROCESS_USERDIR_SCAN_START, EPROCESS_USERDIR_SCAN_END)` under the primary DTB and
    /// validating each page-aligned nonzero candidate against `module_base`'s PE header. This is
    /// Nika's "cached DTB straight from PID": the field lives in a fixed kernel struct, not
    /// somewhere in 32 GiB of guest-physical memory.
    ///
    /// On a CR3 rotation the Windows kernel updates `UserDirectoryTableBase` in EPROCESS before
    /// scheduling the process back, so re-reading this struct is sufficient to track rotations —
    /// no re-scan of guest-physical RAM needed.
    ///
    /// Worst-case cost: [`EPROCESS_USERDIR_SCAN_SLOTS`] DTB-scoped reads (≤ ~50), each a single
    /// 8-byte physical read. Compare to [`bruteforce_secondary_dtb`]'s ≤ 8 million candidates.
    pub fn find_userdir_dtb_from_eprocess_blocking(
        eprocess_addr: u64,
        primary_dtb: u64,
        module_base: u64,
    ) -> Option<u64> {
        // Page-size alignment requirement for a valid DTB candidate.
        const PAGE_SZ: u64 = 0x1000;
        // Known offsets of KPROCESS::UserDirectoryTableBase across shipped Windows 10/11 builds.
        // Tried first; EPROCESS scan below covers builds not in this list.
        const KNOWN_OFFSETS: &[u64] = &[
            0x278, // Win10 1607–1809
            0x280, // Win10 1903–21H2, Win11 21H2
            0x388, // Win11 22H2–23H2 (offset shifted with new KPROCESS fields)
            0x390, // Win11 24H2 candidates; verify against your build's ntoskrnl symbols
        ];
        for &off in KNOWN_OFFSETS {
            let Some(candidate_va) = eprocess_addr.checked_add(off) else {
                continue;
            };
            // Physical walk under primary DTB: bypasses the Win32 process layer's user-space VA
            // restriction — EPROCESS sits in kernel space (0xffff...) and a process handle's
            // read_raw_into() silently fails there. The physical connector has no such limit.
            let candidate = {
                let mut guard = PROCESS.lock().unwrap();
                let Some(loaded) = guard.as_mut() else { return None };
                let Some(phys_addr) = page_walk_to_phys(&mut loaded.phys_connector, primary_dtb, candidate_va) else { continue };
                phys_read_u64(&mut loaded.phys_connector, phys_addr)
            };
            let Some(candidate) = candidate else { continue };
            // PCID / bit-63 mask — see dtb2_shadow_candidate. A UserDirectoryTableBase read
            // straight out of KPROCESS routinely has low bits set on a PCIDE system, and the
            // old `% PAGE_SZ != 0` rejection silently discarded every such build. This is the
            // single most likely reason this scan reported "all offsets missed".
            let candidate = candidate & !0xFFFu64 & !(1u64 << 63);
            if candidate == 0 || candidate == primary_dtb {
                continue;
            }
            // Validate: does this DTB map module_base with a valid PE32+ header?
            if check_mz_under_phys_dtb(candidate, module_base) {
                return Some(candidate);
            }
        }
        // KNOWN_OFFSETS miss — scan the plausible EPROCESS range for any page-aligned candidate.
        // Upper limit chosen to cover all known builds with headroom; exits early on first hit.
        const SCAN_START: u64 = 0x260;
        const SCAN_END: u64 = 0x3C0;
        let mut off = SCAN_START;
        while off < SCAN_END {
            if KNOWN_OFFSETS.contains(&off) {
                off += 8;
                continue;
            }
            let candidate_va = match eprocess_addr.checked_add(off) {
                Some(a) => a,
                None => break,
            };
            off += 8;
            let candidate = {
                let mut guard = PROCESS.lock().unwrap();
                let Some(loaded) = guard.as_mut() else { return None };
                let Some(phys_addr) = page_walk_to_phys(&mut loaded.phys_connector, primary_dtb, candidate_va) else { continue };
                phys_read_u64(&mut loaded.phys_connector, phys_addr)
            };
            let candidate = match candidate.map(|v| v & !0xFFFu64 & !(1u64 << 63)) {
                Some(v) if v != 0 && v != primary_dtb => v,
                _ => continue,
            };
            if check_mz_under_phys_dtb(candidate, module_base) {
                return Some(candidate);
            }
        }
        None
    }

    /// Walks `module_base` through `candidate_dtb` using the physical connector and checks for
    /// MZ magic. Pure physical reads — no process handle, no kernel-VA restriction.
    fn check_mz_under_phys_dtb(candidate_dtb: u64, module_base: u64) -> bool {
        let mut guard = PROCESS.lock().unwrap();
        let Some(loaded) = guard.as_mut() else { return false };
        let Some(phys_addr) = page_walk_to_phys(&mut loaded.phys_connector, candidate_dtb, module_base) else {
            return false;
        };
        phys_read_u16(&mut loaded.phys_connector, phys_addr).map_or(false, |mz| mz == 0x5A4D)
    }

    // ── EAAC shadow-DTB array ─────────────────────────────────────────────────────────────────
    //
    // EasyAntiCheat maintains an array of 128 encrypted shadow-DTB candidates in its mapped
    // image at a known offset.  Reading and decoding this array is faster than bruteforcing
    // the 32 GiB guest-physical range (one 1 KiB read vs. up to 8 million 4 KiB reads) and
    // more reliable than the EPROCESS path (no kernel-VA walk required — the EAAC module base
    // is in the game's user-space).
    //
    // Offset empirically found and shared on UC thread 716406 for the current EAC version;
    // update EAAC_SHADOW_ARRAY_OFFSET if a future EAC update shifts the table.

    /// Byte offset inside the EAAC module image where the 128-entry encrypted shadow-DTB
    /// table begins.  Each entry is a `u64` encoded with `decode_shadow_entry`.
    const EAAC_SHADOW_ARRAY_OFFSET: u64 = 0x64A8;
    const EAAC_ENTRY_COUNT: usize = 128;

    /// Decode one encrypted entry from the EAAC shadow-DTB table.
    ///
    /// Returns `None` when:
    /// * the entry's tag (`enc & 0x7FFF`) does not match any known decode key (incl. the
    ///   `0x3C89` case whose XOR constant is not yet publicly confirmed), or
    /// * the decoded value is zero or not 4 KiB aligned (not a valid physical page root).
    fn decode_shadow_entry(enc: u64) -> Option<u64> {
        // Step 1 — right-rotate the raw encoded value by 3 bits.
        let v = enc.rotate_right(3);

        // Step 2 — tag-keyed XOR / byteswap.  Tag is the lower 15 bits of the *original* enc.
        let tag = (enc & 0x7FFF) as u16;
        let v: u64 = match tag {
            0x5128 => {
                let x = v ^ 0x0891_C4F0_484E_2A25_u64;
                // byteswap the upper 32 bits in-place (lower 32 stay)
                let lo = x & 0xFFFF_FFFF;
                let hi = (x >> 32) as u32;
                lo | ((hi.swap_bytes() as u64) << 32)
            }
            0x545F => v ^ 0xF10C_DFF4_F96D_EA8B_u64,
            0x0BFE => v ^ 0xDFCD_EB54_228F_717F_u64,
            // 0x3C89: byteswap64(v ^ 0x3...) — XOR constant not yet confirmed; skip.
            _ => return None,
        };

        // Step 3 — validity: non-zero and 4 KiB aligned (required for a physical PML4 root).
        if v == 0 || (v & 0xFFF) != 0 {
            return None;
        }
        Some(v)
    }

    /// Common EAC module names as they appear in the game process module list.
    /// Tried in order; the first match wins.
    const EAAC_MODULE_NAMES: &[&str] = &[
        "EasyAntiCheat_EOS.sys",
        "EasyAntiCheat_x64.sys",
        "EasyAntiCheat.sys",
        "EasyAntiCheat_EOS.dll",
        "EasyAntiCheat.dll",
    ];

    /// Return the mapped base VA of any EasyAntiCheat module loaded in the Apex process,
    /// or `None` if none is found.  Called with `PROCESS` already locked (caller holds the
    /// `MutexGuard` and passes `loaded` directly to avoid a second lock attempt).
    fn find_eaac_base_in_loaded(loaded: &mut Loaded) -> Option<u64> {
        for &name in EAAC_MODULE_NAMES {
            if let Ok(info) = loaded.process.module_by_name(name) {
                let base = info.base.to_umem() as u64;
                if base != 0 {
                    tracing::debug!(name, base = %format!("{base:#x}"), "cr3: found EAAC module");
                    return Some(base);
                }
            }
        }
        // Fallback: iterate all modules looking for any name that starts with "EasyAntiCheat"
        use memflow::os::Process as _;
        let mut found: Option<u64> = None;
        loaded.process.module_list_callback(
            None,
            (&mut |info: memflow::os::ModuleInfo| {
                let name_s = info.name.to_string();
                if name_s.to_ascii_lowercase().starts_with("easyantichat")
                    || name_s.to_ascii_lowercase().starts_with("easyanticheat")
                {
                    let base = info.base.to_umem() as u64;
                    if base != 0 {
                        found = Some(base);
                        return false; // stop iteration
                    }
                }
                true
            }).into(),
        );
        found
    }

    /// Read and decode the 128-entry EAAC shadow-DTB array from the EasyAntiCheat module,
    /// returning the first decoded candidate that validates against `module_base`'s PE header.
    ///
    /// Much faster than either the EPROCESS walk or the guest-physical bruteforce:
    /// one 1 KiB read + at most 128 physical page-table walks.
    pub fn find_eaac_shadow_dtb_blocking(module_base: u64) -> Option<u64> {
        // A missing EAAC module cannot appear mid-session; don't re-check (or re-log) per tick.
        if backoff_active(&EAAC_RETRY_AFTER_MS) {
            return None;
        }
        // Phase 1 — read the raw array (PROCESS lock held only during this phase).
        let candidates: Vec<u64> = {
            let mut guard = PROCESS.lock().unwrap();
            let Some(loaded) = guard.as_mut() else { return None };

            let eaac_base = match find_eaac_base_in_loaded(loaded) {
                Some(b) => b,
                None => {
                    arm_backoff(&EAAC_RETRY_AFTER_MS);
                    tracing::warn!(
                        retry_in_ms = FASTPATH_BACKOFF_MS,
                        "cr3: EAAC module not found in process — skipping EAAC array path"
                    );
                    return None;
                }
            };

            // Read under primary DTB (EAAC is in user-space, primary DTB maps it).
            let array_va = eaac_base + EAAC_SHADOW_ARRAY_OFFSET;
            if loaded.process
                .set_dtb(Address::from(loaded.primary_dtb), Address::invalid())
                .is_err()
            {
                return None;
            }
            let mut buf = [0u8; EAAC_ENTRY_COUNT * 8];
            if loaded.process
                .read_raw_into(Address::from(array_va), &mut buf)
                .is_err()
            {
                tracing::warn!(
                    array_va = %format!("{array_va:#x}"),
                    eaac_base = %format!("{eaac_base:#x}"),
                    "cr3: EAAC array read failed"
                );
                return None;
            }

            buf.chunks_exact(8)
                .filter_map(|chunk| {
                    let enc = u64::from_le_bytes(chunk.try_into().unwrap());
                    decode_shadow_entry(enc)
                })
                .collect()
            // lock released here
        };

        tracing::debug!(
            count = candidates.len(),
            "cr3: EAAC array decoded {} valid candidates",
            candidates.len()
        );

        // Phase 2 — validate each decoded candidate against the PE header.
        // check_mz_under_phys_dtb re-acquires the PROCESS lock; do NOT hold it here.
        for candidate in &candidates {
            if check_mz_under_phys_dtb(*candidate, module_base) {
                tracing::info!(
                    dtb = %format!("{candidate:#x}"),
                    "cr3: EAAC array — found valid shadow DTB"
                );
                return Some(*candidate);
            }
        }

        tracing::warn!(
            count = candidates.len(),
            "cr3: EAAC array — no candidate validated against PE header"
        );
        None
    }

    /// Async wrapper around [`find_eaac_shadow_dtb_blocking`].
    pub async fn find_eaac_shadow_dtb(module_base: u64) -> Option<u64> {
        tokio::task::spawn_blocking(move || {
            let mut guard = PROCESS.lock().unwrap();
            if guard.is_none() {
                match ensure_loaded(&mut guard) {
                    Ok(()) => {}
                    Err(e) => {
                        // ensure_loaded already logged the real failure once and armed a
                        // backoff; re-logging it per tick is what buried the TUI.
                        tracing::debug!(error = %e, "cr3: memflow not loaded for EAAC path");
                        return None;
                    }
                }
            }
            drop(guard); // release before calling find_eaac_shadow_dtb_blocking (it locks too)
            find_eaac_shadow_dtb_blocking(module_base)
        })
        .await
        .ok()
        .flatten()
    }

    // ── end EAAC shadow-DTB array ─────────────────────────────────────────────────────────────

    /// Async wrapper around [`find_userdir_dtb_from_eprocess_blocking`]. Returns `None` if
    /// memflow isn't configured or no candidate in EPROCESS validates against the PE header.
    ///
    /// Importantly, this now calls [`ensure_loaded`] internally so the memflow process is
    /// initialised on first call rather than waiting for a DTB-scoped read to trigger it.
    /// The old version read `PROCESS` first and returned `None` if unset — but `PROCESS` was
    /// only populated by `read_raw_blocking`, which in turn was only called after a DTB was
    /// already known, making `PROCESS` always `None` when this fast-path ran (chicken-and-egg).
    pub async fn find_userdir_dtb_from_eprocess(module_base: u64) -> Option<u64> {
        // Stable miss — stay out until the window expires (see FASTPATH_BACKOFF_MS).
        if backoff_active(&EPROCESS_RETRY_AFTER_MS) {
            return None;
        }
        tokio::task::spawn_blocking(move || {
            let (eprocess_addr, primary_dtb) = {
                let mut guard = PROCESS.lock().unwrap();
                // Eagerly load the memflow process if not yet done — previously this
                // returned None immediately when PROCESS was unset, so the EPROCESS fast
                // path never fired on first call.
                if guard.is_none() {
                    match ensure_loaded(&mut guard) {
                        Ok(()) => {
                            if let Some(ref l) = *guard {
                                tracing::info!(
                                    eprocess_addr = %format!("{:#x}", l.eprocess_addr),
                                    primary_dtb   = %format!("{:#x}", l.primary_dtb),
                                    "cr3: memflow secondary connector loaded (EPROCESS fast path)"
                                );
                            }
                        }
                        Err(ref e) => {
                            // Logged once by ensure_loaded with a backoff; keep this quiet.
                            tracing::debug!(
                                error = %e,
                                "cr3: memflow secondary connector FAILED to load — EPROCESS fast path unavailable"
                            );
                            return None;
                        }
                    }
                }
                guard
                    .as_ref()
                    .map(|l| (l.eprocess_addr, l.primary_dtb))?
            };
            if eprocess_addr == 0 {
                tracing::warn!("cr3: EPROCESS addr is zero — skipping EPROCESS DTB scan");
                return None;
            }
            // The candidate offset list is fixed and the kernel build doesn't change mid-run,
            // so a miss here is stable. Re-scanning per tick only costs reads and log spam.
            tracing::debug!(
                eprocess_addr = %format!("{eprocess_addr:#x}"),
                primary_dtb   = %format!("{primary_dtb:#x}"),
                module_base   = %format!("{module_base:#x}"),
                "cr3: scanning EPROCESS for UserDirectoryTableBase"
            );
            let result = find_userdir_dtb_from_eprocess_blocking(eprocess_addr, primary_dtb, module_base);
            match result {
                Some(dtb) => {
                    EPROCESS_RETRY_AFTER_MS.store(0, std::sync::atomic::Ordering::Relaxed);
                    tracing::info!(
                        dtb = %format!("{dtb:#x}"),
                        "cr3: EPROCESS scan found UserDirectoryTableBase (shadow DTB)"
                    );
                    Some(dtb)
                }
                None => {
                    arm_backoff(&EPROCESS_RETRY_AFTER_MS);
                    tracing::warn!(
                        eprocess_addr = %format!("{eprocess_addr:#x}"),
                        primary_dtb   = %format!("{primary_dtb:#x}"),
                        retry_in_ms   = FASTPATH_BACKOFF_MS,
                        "cr3: EPROCESS scan found no valid shadow DTB candidate — all offsets missed (falling back to CR3 bruteforce)"
                    );
                    None
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Scans `/proc` for a running `qemu-system-x86_64` process and returns its PID. Used when
    /// `memflow_connector_args` is empty so the KVM connector gets an explicit `pid=<N>` arg —
    /// the connector needs the host PID of the QEMU process that owns the VM's KVM memory slots;
    /// without it `create_connector("kvm", ...)` errors immediately.
    fn find_qemu_pid() -> Option<u32> {
        let dir = std::fs::read_dir("/proc").ok()?;
        for entry in dir.flatten() {
            let fname = entry.file_name();
            let fname_str = fname.to_string_lossy();
            if !fname_str.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let comm_path = entry.path().join("comm");
            let comm = std::fs::read_to_string(&comm_path).unwrap_or_default();
            // Match "qemu-system" prefix — covers qemu-system-x86_64 and similar.
            // comm is typically limited to 15 chars by the kernel, so the full name may be
            // truncated to "qemu-system-x86" — still unambiguous.
            if comm.trim().starts_with("qemu-system") {
                if let Ok(pid) = fname_str.parse::<u32>() {
                    return Some(pid);
                }
            }
        }
        None
    }

    // Keep the process instance alive. Nika's relevant behavior is the Win32 process layer's
    // Process::set_dtb, not a fresh VirtualDma built around the connector for every read.
    #[allow(deprecated)]
    fn load(
        dir: &str,
        name: &str,
        args: &str,
    ) -> anyhow::Result<(IntoProcessInstanceArcBox<'static>, ConnectorInstanceArcBox<'static>, u64, u64, u64)> {
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

        // When no explicit connector args are configured, auto-detect the QEMU host PID so the
        // KVM connector knows which VM's memory to attach to. Without pid=<N> the connector
        // cannot open the right KVM memory slot and will return an error immediately.
        let effective_args: String = if args.is_empty() {
            match find_qemu_pid() {
                Some(pid) => format!("pid={pid}"),
                None => String::new(),
            }
        } else {
            args.to_owned()
        };

        // memflow 0.2's `ConnectorArgs::from_str` splits on ':' and treats everything before
        // the first colon as the connector TARGET, not as key=value pairs. So "pid=6020"
        // parses as target="pid=6020" with an EMPTY extra_args map. The KVM connector reads
        // its PID from extra_args["pid"], finds nothing, and fails with ArgValidation
        // ("the argument could not be validated"). A leading ':' means "no target, the rest
        // are extra args", which is the form the connector actually accepts.
        let effective_args: String = if !effective_args.is_empty()
            && !effective_args.contains(':')
            && effective_args.contains('=')
        {
            format!(":{effective_args}")
        } else {
            effective_args
        };

        let parsed_args = if effective_args.is_empty() {
            None
        } else {
            Some(ConnectorArgs::from_str(&effective_args).map_err(|e| {
                anyhow::anyhow!(
                    "invalid memflow connector args {effective_args:?}: {}",
                    e.as_str()
                )
            })?)
        };

        let connector = inventory
            .create_connector(name, None, parsed_args.as_ref())
            .map_err(|e| anyhow::anyhow!("memflow connector {name:?} (args={effective_args:?}) failed to load: {}", e.as_str()))?;

        // Clone BEFORE consuming into the OS layer. The clone gives us a separate physical-memory
        // handle for EPROCESS page-table walks. The Win32 process layer's MemoryView restricts
        // reads to user-space VAs; EPROCESS is a kernel VA (0xffff...) and silently returns a
        // translation error there. The physical connector has no such VA gating.
        let phys_connector = connector.clone();

        let mut os = inventory
            .create_os("win32", Some(connector), None)
            .map_err(|e| anyhow::anyhow!("memflow win32 OS failed to load: {}", e.as_str()))?;
        let info = os
            .process_info_by_name("r5apex_dx12.exe")
            .map_err(|e| anyhow::anyhow!("memflow could not find Apex process: {}", e.as_str()))?;
        let primary_dtb = info.dtb1.to_umem() as u64;
        // memflow's Win32 OS parser already resolves BOTH page-table roots for the process:
        // `dtb1` is the one we drive reads with, and `dtb2` is `UserDirectoryTableBase` — the
        // KPTI/second root. That is the very value `find_userdir_dtb_from_eprocess_blocking`
        // hand-scans the EPROCESS struct for, and that `bruteforce_secondary_dtb` sweeps up to
        // 0x800000000 of guest-physical RAM looking for. It was being thrown away here.
        //
        // Taking it straight off ProcessInfo is "read the DTB from the process" — no QMP, no
        // blind sweep. Kept raw; the caller validates it against the PE header before use,
        // because memflow reports 0 (or a stale root) when the process has no second root.
        let secondary_dtb = info.dtb2.to_umem() as u64;
        // memflow's Win32 OS parser stores the EPROCESS virtual address in ProcessInfo::address.
        // We use it to read KPROCESS::UserDirectoryTableBase (the KPTI shadow DTB) directly from
        // the kernel struct at attach time — Nika's "cached DTB straight from PID" approach.
        let eprocess_addr = info.address.to_umem() as u64;
        tracing::info!(
            dtb1 = %format!("{primary_dtb:#x}"),
            dtb2 = %format!("{secondary_dtb:#x}"),
            eprocess_addr = %format!("{eprocess_addr:#x}"),
            "cr3: memflow ProcessInfo page-table roots"
        );
        let process = os
            .into_process_by_info(info)
            .map_err(|e| anyhow::anyhow!("memflow could not open Apex process: {}", e.as_str()))?;
        Ok((process, phys_connector, primary_dtb, secondary_dtb, eprocess_addr))
    }

    fn process_name() -> &'static str {
        "r5apex_dx12.exe"
    }

    /// Wall-clock ms after which a failed secondary-connector load may be retried.
    ///
    /// `load()` performs a full `Inventory::scan_path` (directory walk + dlopen of every
    /// plugin) on every call. Because a failure was never cached, both callers re-ran that
    /// whole sequence on every actions tick: the TUI was buried under two identical WARN
    /// lines ~7ms apart and the actions loop was starved badly enough that it never advanced
    /// past tick 0. Back off instead, and log the failure once per window.
    static LOAD_RETRY_AFTER_MS: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    const LOAD_RETRY_BACKOFF_MS: u64 = 30_000;

    fn wall_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// True while a recent load failure is still inside its backoff window. Callers use this
    /// to stay silent rather than re-logging an outcome that cannot have changed.
    pub(super) fn in_load_backoff() -> bool {
        wall_ms() < LOAD_RETRY_AFTER_MS.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Backoff windows for the two optional shadow-DTB fast paths.
    ///
    /// Both are optimisations over the CR3 bruteforce, which already resolves the shadow DTB
    /// on this build. When a fast path cannot work here (EAAC module absent; no EPROCESS
    /// offset matches this kernel) it fails identically on every tick — so retrying and
    /// logging at tick rate only burns CPU and buries the TUI. Arm a window on failure and
    /// stay silent inside it.
    static EAAC_RETRY_AFTER_MS: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    static EPROCESS_RETRY_AFTER_MS: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    const FASTPATH_BACKOFF_MS: u64 = 60_000;

    fn backoff_active(slot: &std::sync::atomic::AtomicU64) -> bool {
        wall_ms() < slot.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn arm_backoff(slot: &std::sync::atomic::AtomicU64) {
        slot.store(
            wall_ms() + FASTPATH_BACKOFF_MS,
            std::sync::atomic::Ordering::Relaxed,
        );
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

        // Cheap bail while backing off — skips the expensive inventory scan entirely.
        if in_load_backoff() {
            anyhow::bail!("secondary connector load backing off after recent failure");
        }

        let (process, phys_connector, primary_dtb, secondary_dtb, eprocess_addr) = match load(&dir, &name, &args) {
            Ok(v) => {
                LOAD_RETRY_AFTER_MS.store(0, std::sync::atomic::Ordering::Relaxed);
                v
            }
            Err(e) => {
                LOAD_RETRY_AFTER_MS
                    .store(wall_ms() + LOAD_RETRY_BACKOFF_MS, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    error = %e,
                    retry_in_ms = LOAD_RETRY_BACKOFF_MS,
                    "cr3: secondary connector load failed; backing off"
                );
                return Err(e);
            }
        };
        *guard = Some(Loaded {
            dir,
            name,
            args,
            process_name: process_name().to_string(),
            process,
            phys_connector,
            primary_dtb,
            secondary_dtb,
            eprocess_addr,
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

/// Upper bound on `e_lfanew` before it's trusted as an offset: an unvalidated CR3 guess can
/// make this read come back as attacker-irrelevant-but-still-untrusted memory, and without a
/// bound a huge `e_lfanew` would walk the next read arbitrarily far from `module_base`.
const MAX_E_LFANEW: u32 = 0x1000;

const PE_MZ_MAGIC: u16 = 0x5A4D;
/// "PE\0\0" — the NT header signature at `module_base + e_lfanew`.
const PE_NT_SIGNATURE: u32 = 0x0000_4550;

/// Last secondary DTB known to validate, and everything seen valid at least once this session.
/// Mirrors Nika's `lastCorrectDtbPhysAddr` / `validDtbCache`: a shuffle that lands back on a
/// previously-seen value is re-confirmed with one read instead of re-querying QMP.
static LAST_GOOD: AtomicU64 = AtomicU64::new(0);
static VALID_CACHE: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// Milliseconds since UNIX epoch when [`LAST_GOOD`] was last confirmed by a live EPROCESS
/// read. Used for proactive rotation detection: PE-header check alone does NOT catch a shadow
/// PML4 rotation — the old PML4 continues to map `module_base` (MZ still valid) while entity
/// position pages migrate to the new shadow PML4. Without proactive re-confirmation we serve
/// stale entity positions for the entire rotation window (~30s).
///
/// Every [`PROACTIVE_RECONFIRM_INTERVAL_MS`] we spawn a background EPROCESS re-read. If it
/// returns a different DTB, we swap immediately so the next entity tick uses the fresh one.
static LAST_GOOD_CONFIRMED_AT_MS: AtomicU64 = AtomicU64::new(0);

/// How often (ms) to proactively re-read EPROCESS::UserDirectoryTableBase to catch shadow PML4
/// rotations.
///
/// Sized against the *fast* end of the reported rotation interval, not the slow end. UC 716406
/// #2455 (lulyou) reports "every 30-90 seconds", but #2461 (xelbow) corrects it: "the real cr3
/// changes a lot faster than that, more like every 12-20 seconds". An 18s interval sits inside
/// that window, so a root could rotate and go stale before the re-check was even due — the
/// re-check was structurally incapable of pre-empting the stale phase it exists to prevent.
const PROACTIVE_RECONFIRM_INTERVAL_MS: u64 = 8_000;

/// Single-flight guard around the slow QMP-then-bruteforce resolution path in
/// [`resolve_secondary_dtb`]. Without this, every live entity gets its own concurrent
/// `tokio::spawn`ed write in `actions.rs`, each independently calling `resolve_secondary_dtb` —
/// with no cached DTB valid (QMP down, nothing in [`VALID_CACHE`] yet), every single one of those
/// concurrent tasks would race into its own full QMP attempt and, on failure, its own full
/// [`bruteforce_secondary_dtb`] scan, all at once. Observed in practice: with several players on
/// screen this spiked the process to several thousand OS threads and crashed the game — N
/// parallel multi-million-candidate scans hammering the same connector and the guest's physical
/// memory simultaneously, instead of one.
///
/// This is an `AtomicBool`, not a lock callers wait on — [`resolve_secondary_dtb`] never blocks
/// the caller on resolution (see its doc comment for why blocking is itself what crashed the
/// game, independent of the N-way-parallel bug above). This flag only gates "is a background
/// resolution task already running," so a second caller that also misses the cache doesn't spawn
/// a redundant second one.
static RESOLVE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Consecutive validation failures against the cached [`LAST_GOOD`] DTB, kept only for logging.
/// The decision to discard is made on elapsed time (see [`VALIDATION_FAIL_SINCE_MS`]), not on
/// this count.
static VALIDATION_FAIL_STREAK: AtomicU64 = AtomicU64::new(0);

/// Wall-clock ms of the *first* failure in the current unbroken run of failures against
/// [`LAST_GOOD`], or 0 when the last validation succeeded.
///
/// This replaces a count-based guard (`VALIDATION_FAIL_STREAK_LIMIT = 4`) that a high call rate
/// defeated outright. [`resolve_secondary_dtb`] is reached from nine entity types on every scan
/// tick; measured live at ~1000 calls/sec (400 fall-through log lines spanning 0.404s). Four
/// consecutive failures at that rate is **four milliseconds** of tolerance, so the instant a
/// genuine root rotated — or a concurrent sweep saturated the connector enough to fail one read —
/// the cached root was discarded and another sweep started. A duration is immune to call rate:
/// transient contention clears well inside the grace window, and a real rotation fails
/// continuously and is discarded promptly.
static VALIDATION_FAIL_SINCE_MS: AtomicU64 = AtomicU64::new(0);

/// How long validation must fail *continuously* before a root that worked at least once is
/// discarded.
///
/// Was 750 ms, which is *shorter than the read starvation a sweep causes* — this module's own
/// notes put that at ~700 ms. So the sequence was self-sustaining: a sweep saturates the
/// connector, a concurrent validation read fails for the whole 750 ms, a perfectly good root is
/// declared rotated, and the next sweep starts. Observed live as `0x52c30000` confirmed at
/// 05:24:25.126 and discarded at 05:24:25.986 — 823 ms, when the real rotation interval is
/// 12-20 s (UC 716406 #2461). The root was almost certainly fine and we threw it away.
///
/// 3 s was comfortably longer than a sweep's contention window and still well inside the rotation
/// interval, so a genuinely rotated root is dropped promptly while a contended one survives.
///
/// Cut to 1 s now that the window serves `Ok(last_good)` rather than bailing. Those two changes
/// pull in opposite directions and that is deliberate: serving the cached root means a flicker no
/// longer darkens the mirror at all, so the window no longer needs to be generous to protect
/// uptime — and because writes now continue through the root for the duration, a shorter window
/// bounds how long we can be writing through a root that really has rotated away. The revalidation
/// throttle also removed the starvation this was originally sized against: validations dropped
/// from 4 790/s to 10/s, so a failure is far more likely to be real than contended.
const VALIDATION_FAIL_GRACE_MS: u64 = 1_000;

/// Wall-clock ms of the last call that found nothing resolvable. Callers that miss within
/// [`UNRESOLVED_BACKOFF_MS`] of a previous miss return immediately without re-walking the whole
/// candidate chain.
///
/// Without this, ~1000 calls/sec each ran the full chain — a `dtb2` read, an EAAC module lookup,
/// an EPROCESS scan over every candidate offset, and a `VALID_CACHE` walk — to arrive at the same
/// answer the previous caller got a microsecond earlier, for as long as the root stayed
/// unresolved. That is what produced 120,860 EAAC lookups, 120,831 EPROCESS scans and a 218 MB
/// log file in a single session. The chain can only change on a rotation, so answering from a
/// recent miss costs nothing and removes essentially all of it.
static LAST_UNRESOLVED_AT_MS: AtomicU64 = AtomicU64::new(0);
const UNRESOLVED_BACKOFF_MS: u64 = 50;

/// Wall-clock ms at which the last bruteforce sweep finished. A sweep costs ~700ms of
/// VM-exit-heavy probing, so even with the failure streak above we refuse to start another
/// within [`BRUTEFORCE_COOLDOWN_MS`] — a real rotation is ~30s apart, far outside this window.
static LAST_BRUTEFORCE_DONE_MS: AtomicU64 = AtomicU64::new(0);
const BRUTEFORCE_COOLDOWN_MS: u64 = 5_000;

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

/// Roots that passed [`validate_dtb_by_pe_header`] and then failed
/// [`validate_dtb_by_shadow_origin`] — proven not to be the shadow root, however convincing their
/// PE header looked.
///
/// This exists because the sweep is deterministic: it walks guest-physical memory from 0 upward
/// and returns the *first* candidate that validates, so without a memory of what it already
/// disproved it re-finds the same impostor on every pass, forever. Observed live as `0x1f4000`
/// being "found" 6,016 times across one session and `0xb349000` once per sweep after that, each
/// time committed, each time dropped seconds later by a failed re-validation, each time
/// triggering the next sweep.
///
/// UC thread 716406 #2470 (bluewindow) names the reason that loop never self-corrects: "EA likes
/// to fill it with random [...] to mess with bruteforce mass-scanners but none of the positions
/// are valid" — the low-memory hits are planted bait, not accidents, so there are many of them
/// and they are stable across sweeps. Remembering a rejection is what lets the sweep step *past*
/// the bait instead of stopping on it every time.
static REJECTED_DTBS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// Cap on [`REJECTED_DTBS`]. A rotation every 12-20s over a long session can only disprove so
/// many roots, and an unbounded list scanned per candidate would itself become the bottleneck.
/// On overflow the oldest rejections are dropped — the sweep may rediscover an ancient impostor
/// once, which costs one shadow-probe read to re-reject.
const REJECTED_DTBS_MAX: usize = 4096;

/// [`REJECTED_DTBS`] is kept sorted so this is a binary search, not a linear scan.
///
/// It matters because this runs once per swept candidate — up to [`BRUTEFORCE_SLICE_PAGES`]
/// (192,000) times per slice. A linear `contains` over a full 4,096-entry list would be ~786M
/// comparisons per slice, turning the guard that exists to make the sweep cheaper into the most
/// expensive thing in it. Twelve comparisons per candidate is noise against a DTB-scoped read.
/// How long a rejection is honoured before the candidate becomes eligible again.
///
/// A rejection used to be permanent for the session, and that is unsafe in a way the live data
/// exposed. `reject_dtb` fires on a single negative verdict from the shadow-origin probe, and that
/// probe can be momentarily wrong through no fault of the candidate: the reference values are read
/// one tick apart, so a fast-moving local player (zipline, redeploy, skydive) can exceed the
/// distance tolerance, and a read through a perfectly good root can fail under connector
/// contention. One unlucky verdict and the real root was excluded from every future scan.
///
/// Observed: `rejected: 26` entries, while 26 of the 29 roots confirmed all day live inside the
/// 33-402 MB band the scan covers — and yet ten consecutive band scans found nothing across a
/// two-minute blackout. A band that cannot find a root sitting inside it is a band whose
/// candidates are being filtered out before they are tested.
///
/// 60 s is longer than the observed 15-40 s rotation, so a genuine decoy is still skipped for the
/// lifetime of the root it was competing with, and nothing is excluded forever.
const REJECTED_DTB_TTL_MS: u64 = 60_000;

fn is_rejected_dtb(dtb: u64) -> bool {
    let now = now_ms();
    let rejected = REJECTED_DTBS.lock();
    match rejected.binary_search_by_key(&dtb, |entry| entry.0) {
        Ok(index) => now.saturating_sub(rejected[index].1) < REJECTED_DTB_TTL_MS,
        Err(_) => false,
    }
}

fn reject_dtb(dtb: u64) {
    if dtb == 0 {
        return;
    }
    let now = now_ms();
    let mut rejected = REJECTED_DTBS.lock();
    match rejected.binary_search_by_key(&dtb, |entry| entry.0) {
        // Already present: refresh the stamp so a repeatedly-disproved decoy stays excluded.
        Ok(index) => rejected[index].1 = now,
        Err(insert_at) => {
            if rejected.len() >= REJECTED_DTBS_MAX {
                // Drop the stalest entry rather than the numerically lowest — with a TTL, age is
                // what makes an entry worthless, and `remove(0)` was evicting whichever address
                // happened to sort first.
                if let Some((oldest, _)) = rejected
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, entry)| entry.1)
                    .map(|(index, entry)| (index, entry.1))
                {
                    rejected.remove(oldest);
                }
                // The removal may have shifted indices; recompute rather than reuse a stale one.
                let Err(insert_at) = rejected.binary_search_by_key(&dtb, |entry| entry.0) else {
                    return;
                };
                rejected.insert(insert_at, (dtb, now));
                return;
            }
            rejected.insert(insert_at, (dtb, now));
        }
    }
}

/// Everything needed to prove a candidate root is *the* shadow root, published once per scan tick
/// by the local player's own update (see `state::entities::player`).
///
/// `shadow_ptr` is the local player's `absOriginShadowPtr` (`entity + 0x170`), read under the
/// primary DTB. `origin` is `player_abs_origin_shadow_dup` (`localPlayerBase + 0x375C`), the
/// local-player-only duplicate of the shadow-resolved origin, also readable under the primary
/// DTB. A candidate root is the real one if and only if dereferencing `shadow_ptr` under it lands
/// within a tick of travel of `origin`.
#[derive(Copy, Clone)]
struct ShadowProbe {
    shadow_ptr: u64,
    origin: [f32; 3],
    /// Second, independent reference: the local player's camera position
    /// (`localPlayerBase + 0x1F74`, apexsky `cplayer_camerapos`), the other validation target
    /// ApexCV names in UC 716406 #2572.
    ///
    /// This exists because `player_abs_origin_shadow_dup` is a **hardcoded `0x375c`** in
    /// `apex1_offsets_parser` rather than something the offsets profile resolves, taken from a
    /// post written several game updates ago — and the shipped profiles map `0x375c` to
    /// `m_flLastLandFromHeight`, which is not an origin. If that offset has moved, `origin` is a
    /// float that has nothing to do with position, and a probe built on it alone would reject the
    /// *correct* root and blacklist it. `cplayer_camerapos` is resolved from the profile
    /// (`CPlayer!camera_origin=0x1f74`), so the two references fail independently.
    camera: Option<[f32; 3]>,
    at_ms: u64,
}

static SHADOW_PROBE: Mutex<Option<ShadowProbe>> = Mutex::new(None);

/// How stale a published [`ShadowProbe`] may be before it stops being usable as a verdict. The
/// local player moves, so an old `origin` compared against a fresh shadow read fails on travel
/// alone and would reject a perfectly good root. Beyond this age the probe is ignored and
/// validation falls back to the PE header.
const SHADOW_PROBE_MAX_AGE_MS: u64 = 1_500;

/// One tick of maximum player travel plus slack. The dup and the shadow copy are not guaranteed
/// to be sampled on the same tick, so this is a distance test and never an equality test — the
/// same tolerance `state::entities::player` already uses for its own cross-check.
const SHADOW_PROBE_MAX_DRIFT: f32 = 512.0;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Publishes the local player's shadow-origin probe for [`validate_dtb_by_shadow_origin`].
///
/// Called from the local player's own state update, which is the only place both halves are known
/// at once. A zero pointer or an all-zero origin is not a probe (the player has no position yet),
/// and publishing one would make every candidate "validate" against the origin.
pub fn publish_shadow_probe(shadow_ptr: u64, origin: [f32; 3], camera: [f32; 3]) {
    if shadow_ptr == 0 {
        return;
    }
    let origin_ok = origin != [0.0; 3] && validate_vec3(origin).is_ok();
    let camera = if camera != [0.0; 3] && validate_vec3(camera).is_ok() {
        Some(camera)
    } else {
        None
    };
    // At least one usable reference, or this is not a probe.
    if !origin_ok && camera.is_none() {
        return;
    }
    *SHADOW_PROBE.lock() = Some(ShadowProbe {
        shadow_ptr,
        origin: if origin_ok { origin } else { [0.0; 3] },
        camera,
        at_ms: now_ms(),
    });
}

/// Camera-position tolerance. The camera sits at eye height above the origin — roughly 70 Hammer
/// units of view offset — so it needs the drift allowance *plus* that standing offset, generously
/// bounded. Still vastly tighter than "any page that happens to decode as three finite floats",
/// which is all a wrong root can offer.
const SHADOW_PROBE_CAMERA_DRIFT: f32 = 768.0;

/// The validation ApexCV describes for the *secondary* root, as opposed to the PE-header check
/// that confirms the *primary* one (UC thread 716406, post #2572):
///
/// > "the same way you confirm the normal CR3 against "MZ" you can confirm secondary CR3 using
/// > your own absOrigin against either: localPlayerBaseAddress+0x375C which is a duplicate of
/// > absOrigin for local player only. or agaisnt your cameraPosition
/// > (localPlayerBaseAddress+0x1F74). obviously validate a distance instead of an absolute match."
///
/// This is the only test that can distinguish the real shadow root from an impostor, because it
/// is the only one that reads *live game data* instead of image bytes. Every PE-header check is a
/// property of the executable's own contents, and both of the things that fool us have those
/// contents verbatim: EA's planted decoy page tables (716406 #2470) and the file-cache copy of
/// the exe, which UC 778297 #6 warns is byte-identical and "never fetched by the game". A root
/// that maps the image but not the live shadow region passes all five header checks and then
/// fails on the first real read — which is precisely the loop this module has been stuck in.
///
/// Returns `None` when no fresh probe is published (pre-match, between rounds, or before the
/// local player's first update), meaning "cannot judge" rather than "invalid" — callers must fall
/// back to the header check rather than treat an unjudgeable candidate as bad.
async fn validate_dtb_by_shadow_origin(api: &Api, dtb: u64) -> Option<bool> {
    if dtb == 0 {
        return Some(false);
    }
    let probe = (*SHADOW_PROBE.lock())?;
    if now_ms().saturating_sub(probe.at_ms) > SHADOW_PROBE_MAX_AGE_MS {
        return None;
    }
    let Ok(origin) = dtb_read::<[f32; 3]>(api, dtb, probe.shadow_ptr).await else {
        // A read failure under this root is not a verdict: the sweep itself produces transient
        // read failures by saturating the connector. Let the header check decide.
        return None;
    };
    // Accept a match against EITHER reference. They are derived from different offsets — one
    // hardcoded, one profile-resolved — so requiring both would make the stale one veto the
    // sound one. Requiring either means a single surviving reference still disqualifies every
    // decoy, because a wrong root has no way to produce the player's actual coordinates.
    let has_origin = probe.origin != [0.0; 3];
    let matches_origin =
        has_origin && validate_by_distance(origin, probe.origin, SHADOW_PROBE_MAX_DRIFT);
    let matches_camera = probe
        .camera
        .is_some_and(|cam| validate_by_distance(origin, cam, SHADOW_PROBE_CAMERA_DRIFT));

    // Require every reference we actually have to agree, not just one of them.
    //
    // "either reference matches" was too weak and it confirmed a root that died 0.86 s later —
    // far too fast to be the 12-20 s rotation, so it was never the right root. A single distance
    // test is cheap to satisfy by accident: if one reference is itself garbage close to the world
    // origin, then any candidate root whose read lands on small or zeroed values falls inside the
    // tolerance and "matches". Two independent references, derived from two different offsets,
    // have to be satisfied at once — and a wrong root has no way to produce the player's real
    // coordinates twice over.
    //
    // With only one reference available this is unchanged, which is the honest degradation: it is
    // the best test obtainable at that moment, and `write_at_dtb` still refuses any root that was
    // never confirmed at all.
    let verdict = match (has_origin, probe.camera.is_some()) {
        (true, true) => matches_origin && matches_camera,
        (true, false) => matches_origin,
        (false, true) => matches_camera,
        (false, false) => return None,
    };
    Some(verdict)
}

/// Snapshot of the live probe for diagnostics. Returns `(shadow_ptr, origin, camera, age_ms)`.
///
/// Exists because the probe's reference values have been the one unverified link in this chain
/// all along: `player_abs_origin_shadow_dup` is a hardcoded `0x375c` literal in
/// `apex1_offsets_parser`, taken from a forum post rather than resolved from the offsets profile,
/// and nothing has ever printed what it actually reads. Guessing about it has cost several builds.
pub fn shadow_probe_snapshot() -> Option<(u64, [f32; 3], Option<[f32; 3]>, u64)> {
    let probe = (*SHADOW_PROBE.lock())?;
    Some((
        probe.shadow_ptr,
        probe.origin,
        probe.camera,
        now_ms().saturating_sub(probe.at_ms),
    ))
}

/// The single qualification gate for a secondary-DTB candidate: cheap header pre-filter, then the
/// decisive live-data check.
///
/// Order matters for cost. The header check is four small reads and rejects the overwhelming
/// majority of sweep candidates, so it runs first; the shadow probe is only spent on candidates
/// that already look like page-table roots mapping the image.
///
/// Order also matters for correctness. The header check can only ever say "this root maps the
/// image"; passing it is necessary and nowhere near sufficient. Only the shadow probe can say
/// "this root maps the data the game is actually reading", so when it returns a verdict that
/// verdict wins outright, and a candidate it rejects is remembered in [`REJECTED_DTBS`] so the
/// sweep never stops on it again.
async fn validate_dtb(api: &Api, dtb: u64, module_base: u64) -> bool {
    validate_dtb_inner(api, dtb, module_base, false).await
}

/// The last root to earn a positive [`validate_dtb_by_shadow_origin`] verdict — proven against
/// live game data, not merely against the image's PE header.
///
/// [`write_at_dtb`] refuses any other root. A write through a root that only satisfied the header
/// check lands wherever that bogus translation happens to resolve, which is how a decoy root
/// turned the 22-slot highlight mirror into 22 writes into unrelated guest physical memory every
/// mirror pass. Reads through an unconfirmed root return garbage and the caller discards it;
/// writes through one corrupt the guest. The two do not deserve the same gate.
static PROBE_CONFIRMED_DTB: AtomicU64 = AtomicU64::new(0);

/// `strict` refuses to qualify a candidate the shadow probe cannot judge.
///
/// The lenient form is correct for the three fast paths, whose candidates come out of kernel
/// structures — `ProcessInfo::dtb2`, the EAAC array, `KPROCESS::UserDirectoryTableBase`. Those are
/// claims by the OS about this process, so a PE header match is reasonable corroboration and
/// "no probe available yet" should not block an early-match startup.
///
/// The strict form is mandatory for the sweep, whose candidates are *guesses*. Letting an
/// unjudgeable guess through means committing whatever page EA planted: UC 716406 #2470
/// (bluewindow) — "EA likes to fill it with random [...] to mess with bruteforce mass-scanners
/// but none of the positions are valid". Observed live, this is exactly what happened — with no
/// probe published the sweep marched 0x0 → 0x2950000 → 0x2e3b000 → 0x455a000 → 0x6cb4000 → ...
/// committing a different wrong root every five seconds, each one then driving entity reads and
/// highlight writes.
async fn validate_dtb_inner(api: &Api, dtb: u64, module_base: u64, strict: bool) -> bool {
    if dtb == 0 || is_rejected_dtb(dtb) {
        return false;
    }
    if !validate_dtb_by_pe_header(api, dtb, module_base).await {
        return false;
    }
    match validate_dtb_by_shadow_origin(api, dtb).await {
        Some(true) => {
            PROBE_CONFIRMED_DTB.store(dtb, Ordering::Release);
            true
        }
        Some(false) => {
            // The probe only earns the right to *blacklist* once it has confirmed something.
            //
            // A blacklist entry is permanent for the session, so a probe built on a stale offset
            // would use its first few verdicts to poison the one root that actually works and
            // then never find another. Requiring one prior confirmation makes that impossible:
            // until the probe has demonstrably recognised a real root, a negative verdict only
            // declines this candidate and nothing is remembered.
            if PROBE_CONFIRMED_DTB.load(Ordering::Acquire) != 0 {
                reject_dtb(dtb);
                tracing::debug!(
                    dtb = %format!("{dtb:#x}"),
                    "cr3: candidate passed PE header but failed the shadow-origin probe — rejected"
                );
            } else {
                tracing::debug!(
                    dtb = %format!("{dtb:#x}"),
                    "cr3: candidate failed an unproven shadow-origin probe — declined, not \
                     blacklisted"
                );
            }
            false
        }
        // Unjudgeable. A kernel-sourced candidate may proceed on the header alone; a guess may
        // not, and is not blacklisted either — it was never disproved, only unproven.
        None => !strict,
    }
}

/// Whether `dtb` is the root that last satisfied the live shadow-origin probe.
pub fn is_probe_confirmed(dtb: u64) -> bool {
    dtb != 0 && PROBE_CONFIRMED_DTB.load(Ordering::Acquire) == dtb
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

    let Some(nt) = module_base.checked_add(e_lfanew as u64) else {
        return false;
    };

    // PE\0\0 at e_lfanew. We were reading e_lfanew purely as a bounds check and never
    // confirming the signature it points at.
    let Ok(pe_sig) = dtb_read::<u32>(api, dtb, nt).await else {
        return false;
    };
    if pe_sig != PE_NT_SIGNATURE {
        return false;
    }

    let Some(image_base_addr) = nt.checked_add(0x30) else {
        return false;
    };
    let Ok(image_base) = dtb_read::<u64>(api, dtb, image_base_addr).await else {
        return false;
    };
    if image_base != module_base {
        return false;
    }

    // Second VA, per UC 778297 #6: a DTB that maps the *file-cache copy* of the executable
    // rather than the live mapped image still passes every header check above — the cached
    // page is byte-identical. Those pages are never fetched by the game, so a DTB validated
    // on the header alone can be a false positive that then fails on real entity reads.
    // Translating a second, distant VA (the entry point, which lives in .text) only succeeds
    // on a root that maps the whole image.
    let Some(entry_rva_addr) = nt.checked_add(0x28) else {
        return false;
    };
    let Ok(entry_rva) = dtb_read::<u32>(api, dtb, entry_rva_addr).await else {
        return false;
    };
    if entry_rva == 0 {
        return false;
    }
    let Some(entry_va) = module_base.checked_add(entry_rva as u64) else {
        return false;
    };
    dtb_read::<u32>(api, dtb, entry_va).await.is_ok()
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

/// Page size used both as the physical page-table-root alignment (a CR3 must be page-aligned)
/// and as the bruteforce scan stride below.
const PAGE_SIZE: u64 = 0x1000;

/// Upper bound for [`bruteforce_secondary_dtb`]'s linear scan: generous headroom (2x) over this
/// deployment's guest RAM (Celestial VM: `-m size=16777216k` = 16 GiB, from the qemu commandline)
/// without needing a settings.toml knob. A secondary DTB is a physical page-table root, so it
/// must land inside guest-physical RAM; scanning past the actual RAM size just wastes time on
/// reads that will never hit a mapped page, which [`validate_dtb_by_pe_header`] already treats
/// as "does not validate" rather than erroring.
const BRUTEFORCE_MAX_PHYS: u64 = 32 * 1024 * 1024 * 1024;

/// Last-resort secondary-DTB discovery when QMP is unavailable, misconfigured, or returns a
/// value that doesn't validate (e.g. a stale socket permission on `device.qemu_qmp_addr` — this
/// is the recoverable case, not a reason to give up on occluded glow entirely). UC thread 716406
/// posts #1765/#1767 (ApexCV): "to get the base address you can read SectionBaseAddress at
/// EPROCESS+0x520 ... after this you can bruteforce CR3 by testing the first two bytes for 'MZ'"
/// — page-aligned candidate, `set_dtb`, check `e_magic == 0x5A4D` at the module base, then cross-
/// check `ImageBase` at `e_lfanew + 0x30` — exactly what [`validate_dtb_by_pe_header`] already
/// implements, just driven over the whole guest-physical range instead of one QMP-reported
/// candidate. Also independent of CR3 shuffling (#1207, same author: "or if it's CR3 related i
/// would not know because i just brute force so it always works").
///
/// Slow by construction — up to [`BRUTEFORCE_MAX_PHYS`]/[`PAGE_SIZE`] candidate reads in the
/// worst case — so callers should only reach this once per CR3 rotation (the same cadence the
/// QMP path and [`VALID_CACHE`] already assume), never per-entity-per-tick. [`RESOLVE_LOCK`]
/// already guarantees only one of these runs at a time; the throttle below is a second,
/// independent guard — even strictly sequential, a `set_dtb`-scoped physical read on a KVM
/// connector typically traps the guest (VM-exit) per call, so hammering it at max possible rate
/// for millions of iterations can still degrade or destabilize the VM on its own, or read as
/// exactly the kind of anomalous memory-scan burst an anti-cheat heuristic watches for.
/// Probes between yields.
///
/// This is the knob that decides instantaneous pressure on the guest, and it has to be traded off
/// against a hard deadline rather than minimised. Each probe is a DTB-scoped read that traps the
/// guest, so a high value is a VM-exit burst on a second connector while the first serves the game
/// loop — UC 716406 #2467: "bruteforcing is very fragile and can lead to random bsods if you're
/// not careful". But the root rotates every 12-20 s (#2461), so a sweep that cannot finish a slice
/// inside that window never catches anything at all, no matter how gentle it is.
///
/// 64 was tried and it loses that race outright. Measured on the same hardware, same slice size:
///   1024 probes/yield → 192 000 pages in 1.149 s  (found `0xb172000`)
///     64 probes/yield → 192 000 pages in 13.486 s (found nothing)
/// The earlier note here claimed "the sleep dominates either way, so 64 costs nothing"; that was
/// wrong by 11.7x. At 192 000 pages a slice there are 3000 sleeps at /64 versus 187 at /1024, and
/// a `tokio::time::sleep` of one millisecond costs at least a millisecond of timer granularity
/// plus a scheduler round trip each time. The sleeps *are* the budget.
///
/// Live consequence of getting this wrong, from the run that produced this comment: zero
/// `highlight table mirrored through shadow DTB` lines in an entire match, because no sweep slice
/// ever completed inside a rotation. No shadow root means no mirror, which means the global
/// highlight table is only ever written through the primary (decoyed) root — so far rasterized
/// players glow and close occluded ones do not. That is the exact symptom this module exists to
/// remove, reintroduced by this one constant.
const BRUTEFORCE_YIELD_EVERY: u64 = 1024;
const BRUTEFORCE_YIELD_FOR: std::time::Duration = std::time::Duration::from_millis(1);

/// Guest-physical offset the next sweep resumes from, so consecutive sweeps make forward progress
/// instead of re-walking the same prefix.
///
/// Every sweep used to restart at 0. Since the scan is deterministic and EA plants decoy
/// page-table pages low in guest-physical memory (UC 716406 #2470), restarting meant every sweep
/// re-found the same bait: `0x1f4000` was "found" 6,016 times in one session without the scan
/// ever reaching the address space above it. With [`REJECTED_DTBS`] filtering known impostors and
/// this cursor carrying the position forward, the sweep behaves like one progressive scan across
/// however many invocations it takes, and wraps to 0 only after covering the range.
static BRUTEFORCE_CURSOR: AtomicU64 = AtomicU64::new(0);

/// Candidate pages examined per invocation before yielding back with "not yet".
///
/// A full 32 GiB sweep is 8.4M probes and each one traps the guest, so running it to completion
/// in a single pass is the ~2.7s connector-saturating burst that UC 716406 #2467 describes:
/// "bruteforcing is very fragile and can lead to random bsods if you're not careful and having to
/// do it continously is aids". Slicing it bounds the burst, and the cursor means the slices
/// compose into a complete scan.
///
/// The slice size is set by a deadline, not by a throughput target. Timed from the run that
/// produced this comment, a candidate costs ~50 us — that is one `set_dtb` + read round trip, and
/// it does not shrink, because the PE check already short-circuits on "MZ" so almost every page
/// pays for exactly one read:
///
///   band    57 344 pages  09:22:08.710 -> 09:22:11.529   2.82 s   49 us/page
///   window  16 384 pages  09:22:11.529 -> 09:22:12.361   0.83 s   51 us/page
///   slice  192 000 pages  09:22:12.361 -> 09:22:26.537  14.18 s   74 us/page
///
/// A full band + window + slice cycle at 192 000 was ~17.8 s against a root that rotates every
/// 12-20 s (UC 716406 #2461), so every cycle arrived after the root had already moved. Seven
/// consecutive cycles found nothing while marching 0x5592d000 -> 0x13ff2d000, five gigabytes of
/// ground covered and not one hit.
///
/// 49 152 pages is ~2.5 s, putting the whole cycle near 6 s — comfortably inside one rotation
/// window. Linear coverage per cycle drops from 786 MB to 192 MB, which costs nothing real:
/// confirmed roots have turned up at 0x5592c000 (1.4 GB) and 0x21d98b000 (8.9 GB), so sweeping
/// linearly was never going to reach them anyway. The band and the window around the last
/// confirmed root are what actually land hits, and both only get a chance if the cycle they sit
/// in can finish before the target moves.
const BRUTEFORCE_SLICE_PAGES: u64 = 49_152;

async fn bruteforce_secondary_dtb(api: &Api, module_base: u64) -> anyhow::Result<u64> {
    // Refuse to sweep at all without a live probe to judge candidates with.
    //
    // A sweep that cannot disprove a candidate is strictly harmful: it cannot find the real root
    // (nothing distinguishes it) but it will happily commit the first planted decoy it meets, and
    // every entity read and highlight write then goes through that. Waiting costs nothing — the
    // probe appears on the local player's first update — and the alternative is corrupting guest
    // memory at a fresh random address every cooldown period.
    {
        let probe = *SHADOW_PROBE.lock();
        let fresh = probe
            .map(|p| now_ms().saturating_sub(p.at_ms) <= SHADOW_PROBE_MAX_AGE_MS)
            .unwrap_or(false);
        if !fresh {
            anyhow::bail!(
                "refusing secondary CR3 sweep: no fresh shadow-origin probe to validate \
                 candidates against"
            );
        }
    }

    // Phase 0 — the neighbourhood of the last root the probe confirmed.
    //
    // This runs FIRST now, ahead of the band, because it is both the cheapest phase and the one
    // with by far the best hit rate. Two consecutive roots from the same session were
    // `0xb114000` and `0xb12c000` — 0x18000 apart, twenty-four pages. Rotation frequently hands
    // the allocator a neighbouring frame, so the 64 MB window around the previous root covers the
    // common case in 0.83 s where the band needs 2.82 s to cover its range.
    //
    // Ordering is the whole point. Measured per phase at ~50 us a candidate:
    //
    //   window   16 384 pages   0.83 s
    //   band     57 344 pages   2.82 s
    //   slice    49 152 pages   2.50 s
    //
    // Running band-then-window spent 2.82 s on the less likely range before touching the most
    // likely one, on every single rotation. Since glow correctness is a direct function of how
    // much of the match has a live root — measured at under half the wall clock — three seconds
    // off the common-case re-acquisition is the single highest-value change available here.
    //
    // `PROBE_CONFIRMED_DTB` is deliberately never cleared, so it stays usable as a search hint
    // after the root it names has been dropped as rotated.
    let hint = PROBE_CONFIRMED_DTB.load(Ordering::Acquire);
    if hint != 0 {
        let from = hint.saturating_sub(BRUTEFORCE_WINDOW_BYTES) & !(PAGE_SIZE - 1);
        let pages = (BRUTEFORCE_WINDOW_BYTES * 2) / PAGE_SIZE;
        tracing::warn!(
            hint = %format!("{hint:#x}"),
            from = %format!("{from:#x}"),
            pages,
            "{}",
            "secondary CR3 windowed scan around last confirmed root"
        );
        if let Some(found) = scan_phys_range(api, module_base, from, pages).await {
            tracing::info!(
                candidate = %format!("{found:#x}"),
                "{}",
                "secondary CR3 bruteforce found a match"
            );
            return Ok(found);
        }
    }

    // Phase 1 — the band every root observed in practice has lived in.
    //
    // This phase exists because of a race, not just a cost. The shadow root rotates every 12-20 s
    // (UC 716406 #2461), so a scan is only useful if it can cover the ground where the root
    // actually is *within one rotation window*. A 192,000-page slice took 14.18 s, the same order
    // as the rotation period — it kept arriving at each address after the root had already moved.
    //
    // The answer is a narrower range, not a faster one. Every root confirmed across the whole of
    // this session, in order of appearance:
    //
    //   0x2176000  34 MB    0x216f000  35 MB    0x6d71000 110 MB    0xb114000 177 MB
    //   0xb12c000 177 MB    0xef34000 239 MB    0xf108000 241 MB    0x11dec000 285 MB
    //   0x5592c000  1.4 GB
    //
    // Eight of nine fall between 34 MB and 285 MB. The old top end of 0x10000000 (268 MB) cut
    // across that cluster and excluded 0x11dec000, so the band could not find a root that was
    // sitting inside the pattern the band exists to exploit. 0x18000000 (402 MB) covers all eight
    // with margin, for 90,112 pages — about 4.5 s, which still fits a rotation window now that
    // the window phase runs ahead of it and usually returns first.
    {
        let pages = (BRUTEFORCE_BAND_END - BRUTEFORCE_BAND_START) / PAGE_SIZE;
        tracing::warn!(
            from = %format!("{BRUTEFORCE_BAND_START:#x}"),
            to = %format!("{BRUTEFORCE_BAND_END:#x}"),
            pages,
            "{}",
            "secondary CR3 priority-band scan"
        );
        if let Some(found) =
            scan_phys_range(api, module_base, BRUTEFORCE_BAND_START, pages).await
        {
            tracing::info!(
                candidate = %format!("{found:#x}"),
                "{}",
                "secondary CR3 bruteforce found a match"
            );
            return Ok(found);
        }
    }

    // Phase 2 — resume the full progressive sweep where the last slice stopped.
    let start = BRUTEFORCE_CURSOR.load(Ordering::Acquire);
    tracing::warn!(
        start = %format!("{start:#x}"),
        max = %format!("{BRUTEFORCE_MAX_PHYS:#x}"),
        rejected = REJECTED_DTBS.lock().len(),
        "{}",
        "secondary CR3 sweep slice started"
    );
    match scan_phys_range(api, module_base, start, BRUTEFORCE_SLICE_PAGES).await {
        Some(found) => {
            BRUTEFORCE_CURSOR.store(found.wrapping_add(PAGE_SIZE), Ordering::Release);
            tracing::info!(
                candidate = %format!("{found:#x}"),
                "{}",
                "secondary CR3 bruteforce found a match"
            );
            Ok(found)
        }
        None => {
            let end = start.wrapping_add(BRUTEFORCE_SLICE_PAGES * PAGE_SIZE);
            BRUTEFORCE_CURSOR.store(
                if end >= BRUTEFORCE_MAX_PHYS { 0 } else { end },
                Ordering::Release,
            );
            anyhow::bail!(
                "secondary CR3 sweep slice covered {} pages up to {end:#x} with no match",
                BRUTEFORCE_SLICE_PAGES
            )
        }
    }
}

/// Guest-physical band that every secondary root observed in practice has fallen inside.
///
/// Derived from ten confirmed roots in one session — 0x2950000, 0x2e3b000, 0x455a000, 0x6cb4000,
/// 0x6dfd000, 0x7041000, 0x75a2000, 0xb172000, 0xb349000, 0xbc87000 — widened to the next round
/// boundaries either side. This is a *priority* range, not a limit: the progressive full sweep
/// still runs behind it, so a root outside the band is found eventually rather than never.
const BRUTEFORCE_BAND_START: u64 = 32 * 1024 * 1024;
const BRUTEFORCE_BAND_END: u64 = 384 * 1024 * 1024;

/// How far either side of the last confirmed root [`bruteforce_secondary_dtb`] looks first.
///
/// 32 MiB each way is 16,384 probes — a few hundred milliseconds at the current yield rate, and
/// wide enough to cover the spread between consecutive roots observed in practice.
const BRUTEFORCE_WINDOW_BYTES: u64 = 32 * 1024 * 1024;

/// Scans `pages` page-aligned candidates from `from`, wrapping at [`BRUTEFORCE_MAX_PHYS`], and
/// returns the first that passes strict validation.
///
/// Yields every [`BRUTEFORCE_YIELD_EVERY`] probes. The sleep is what keeps this from being a
/// sustained trap storm on the guest, so it is inside the loop and not hoisted out of it.
async fn scan_phys_range(api: &Api, module_base: u64, from: u64, pages: u64) -> Option<u64> {
    let mut candidate = from & !(PAGE_SIZE - 1);
    let mut since_yield = 0u64;
    for _ in 0..pages {
        if candidate >= BRUTEFORCE_MAX_PHYS {
            candidate = 0;
        }
        // The blacklist check is free relative to a DTB-scoped read, so it goes first: the whole
        // point is to not spend probes re-proving that a known decoy is a decoy.
        if !is_rejected_dtb(candidate)
            && validate_dtb_inner(api, candidate, module_base, true).await
        {
            return Some(candidate);
        }
        since_yield += 1;
        if since_yield >= BRUTEFORCE_YIELD_EVERY {
            since_yield = 0;
            tokio::time::sleep(BRUTEFORCE_YIELD_FOR).await;
        }
        candidate += PAGE_SIZE;
    }
    None
}

/// The actual slow resolution: a straight [`bruteforce_secondary_dtb`] scan of guest-physical
/// memory for the PE-header match. The old QMP-first path (a second QMP client on the actuator's
/// socket running `human-monitor-command "info registers"`) was dropped: that `info registers`
/// HMP call stalls the guest vCPUs long enough to crash the game, and the bruteforce scan already
/// resolves the same secondary DTB without ever touching the monitor. Only ever run from inside
/// the detached background task [`resolve_secondary_dtb`] spawns — never awaited directly by an
/// entity-processing call site. Takes an owned `Api` (it derives `Clone`) because it has to
/// outlive the caller's stack frame once spawned.
async fn resolve_via_bruteforce(api: &Api, module_base: u64) -> anyhow::Result<u64> {
    bruteforce_secondary_dtb(api, module_base).await
}

/// Resolves a secondary DTB that currently validates against `module_base`'s PE header, or
/// `Err` if: this [`IMemAccess`] backend can't override DTB per-request, or no cached candidate
/// validates right now. This is the fast-path-only contract it always had before QMP/bruteforce
/// existed — callers (every `read_shadow`/`write_shadow` call site already does this) treat `Err`
/// as "leave the previous value in place" or "fall back to a primary-DTB write", never as fatal.
///
/// Critically, this function **never blocks on QMP or the bruteforce scan**. `read_shadow_origin`
/// runs from nine different entity types — player, loot, deathbox, NPC, waypoint, vehicle,
/// projectile, animating, base — on every scan tick, completely independent of glow settings or
/// player-ready gating. Blocking the caller here (an earlier version of this function used a
/// `tokio::sync::Mutex` single-flight lock) serializes the *entire entity list* behind one
/// multi-minute bruteforce the moment any single entity's cache misses — that global stall, not
/// the scan itself, is what crashed the game. Instead: if nothing cached validates, this ensures
/// at most one background resolution task is running (QMP, then bruteforce on failure — see
/// [`resolve_via_bruteforce`]) via [`RESOLVE_IN_FLIGHT`], fires it with `tokio::spawn` if
/// one isn't already running, and returns `Err` immediately either way. The background task
/// populates [`LAST_GOOD`]/[`VALID_CACHE`] on success, so once it lands, every subsequent call —
/// same tick or ticks later — hits the fast cached path above instead of missing again.
pub async fn resolve_secondary_dtb(api: &Api, module_base: u64) -> anyhow::Result<u64> {
    if !memflow_backend::is_configured() && !api.mem_access.supports_dtb_override() {
        tracing::warn!(
            memflow_configured = memflow_backend::is_configured(),
            dtb_override_supported = api.mem_access.supports_dtb_override(),
            "cr3: resolve_secondary_dtb bailing — no DTB-capable backend"
        );
        anyhow::bail!(
            "secondary CR3 reads need `device.use_memflow_direct = true`, or an IMemAccess \
             backend that supports read_with_dtb"
        );
    }

    // Revalidate the cached root at most every REVALIDATE_INTERVAL_MS, and serve it unchecked in
    // between.
    //
    // This function is called per entity per actions tick, so without a throttle every caller
    // pays for a full `validate_dtb` — and each of those is a DTB-scoped read that traps the
    // guest. Measured in the run that produced this comment:
    //
    //   cr3: shadow DTB failed validation for the full grace window — treating as rotated,
    //        dtb: 0x5592c000, streak: 14408, failing_for: 3008
    //
    // 14 408 consecutive validations inside 3.008 s is 4 790 calls/sec, each one at least one
    // (and for a candidate that gets past "MZ", up to five) `set_dtb` + read round trips. That is
    // a sustained VM-exit storm aimed at the same connector the game loop is reading through, and
    // it is self-defeating: the storm starves the very reads the validation depends on, so
    // validation keeps failing, which keeps the storm running until the 3 s grace window expires
    // and a root that had been mirroring highlights 41 s earlier gets thrown away as "rotated".
    //
    // Note where the old code had no brake at all. The fully-unresolved path stamps
    // `LAST_UNRESOLVED_AT_MS` (so `UNRESOLVED_BACKOFF_MS` collapses those callers), but the
    // "cached root failed, still inside the grace window" bail below stamps nothing — that path
    // ran flat out, once per caller, for the whole 3 s.
    //
    // 100 ms caps it at ten validations/sec. The cost of being wrong for up to 100 ms is reads
    // through a stale root returning garbage that callers already discard, and `write_at_dtb`
    // still refuses to write through any root the live probe has not confirmed.
    static REVALIDATED_AT_MS: AtomicU64 = AtomicU64::new(0);
    const REVALIDATE_INTERVAL_MS: u64 = 100;

    let last_good = LAST_GOOD.load(Ordering::Acquire);
    if last_good != 0 {
        let checked_at = REVALIDATED_AT_MS.load(Ordering::Acquire);
        if checked_at != 0 && now_ms().saturating_sub(checked_at) < REVALIDATE_INTERVAL_MS {
            return Ok(last_good);
        }
    }
    REVALIDATED_AT_MS.store(now_ms(), Ordering::Release);
    if last_good != 0 && validate_dtb(api, last_good, module_base).await {
        VALIDATION_FAIL_STREAK.store(0, Ordering::Release);
        VALIDATION_FAIL_SINCE_MS.store(0, Ordering::Release);
        LAST_UNRESOLVED_AT_MS.store(0, Ordering::Release);
        // PE-header still valid. BUT: the old shadow PML4 keeps module_base mapped even after
        // Windows rotates the shadow DTB (~every 30s). Entity position pages migrate under the
        // NEW shadow PML4 immediately while MZ stays readable through the OLD one — so we serve
        // stale positions for the full rotation window unless we proactively re-read EPROCESS.
        //
        // If memflow is available and we have not confirmed this DTB from EPROCESS within
        // PROACTIVE_RECONFIRM_INTERVAL_MS, spawn a background EPROCESS re-check. The caller
        // keeps using last_good this tick; if the re-check finds a new DTB it will be live for
        // the very next call (sub-second latency vs the ~30s stale window we had before).
        let confirmed_at = LAST_GOOD_CONFIRMED_AT_MS.load(Ordering::Acquire);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let due_for_reconfirm = confirmed_at > 0
            && now_ms.saturating_sub(confirmed_at) > PROACTIVE_RECONFIRM_INTERVAL_MS;
        if memflow_backend::is_configured()
            && due_for_reconfirm
            && RESOLVE_IN_FLIGHT
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            // Bump the timestamp now so racing callers don't also try to spawn.
            LAST_GOOD_CONFIRMED_AT_MS.store(now_ms, Ordering::Release);
            let module_base_cap = module_base;
            let api = api.clone();
            tokio::spawn(async move {
                let api = &api;
                // Try EAAC array first (fastest — one 1 KiB read); fall back to EPROCESS
                // if the module isn't found or no entry validates.
                // Rotation re-check uses the same chain: ProcessInfo::dtb2 first, since the
                // kernel updates that root on rotation too and re-reading it is free.
                let new_dtb = memflow_backend::dtb2_shadow_candidate(module_base_cap);
                let new_dtb = if new_dtb.is_none() {
                    memflow_backend::find_eaac_shadow_dtb(module_base_cap).await
                } else {
                    new_dtb
                };
                let new_dtb = if new_dtb.is_none() {
                    memflow_backend::find_userdir_dtb_from_eprocess(module_base_cap).await
                } else {
                    new_dtb
                };
                // Same reasoning as the main chain: the fast paths self-qualify on the PE header
                // only, so a rotation "detected" here could just as easily be a decoy. Prove it
                // against live shadow data before swapping a working root out for it.
                let new_dtb = match new_dtb {
                    Some(candidate) if validate_dtb(api, candidate, module_base_cap).await => {
                        Some(candidate)
                    }
                    _ => None,
                };
                if let Some(new_dtb) = new_dtb {
                    let old = LAST_GOOD.load(Ordering::Acquire);
                    if new_dtb != old {
                        tracing::info!(
                            old_dtb = %format!("{old:#x}"),
                            new_dtb = %format!("{new_dtb:#x}"),
                            "cr3: proactive re-check detected shadow PML4 rotation — \
                             updating LAST_GOOD to new shadow DTB"
                        );
                        VALID_CACHE.lock().retain(|&c| c != old);
                        let _ = LAST_GOOD.compare_exchange(
                            old,
                            new_dtb,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        let mut cache = VALID_CACHE.lock();
                        if !cache.contains(&new_dtb) {
                            cache.push(new_dtb);
                        }
                    } else {
                        tracing::debug!(
                            dtb = %format!("{new_dtb:#x}"),
                            "cr3: proactive re-check — DTB unchanged, still current"
                        );
                    }
                    let confirmed_now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    LAST_GOOD_CONFIRMED_AT_MS.store(confirmed_now, Ordering::Release);
                }
                RESOLVE_IN_FLIGHT.store(false, Ordering::Release);
            });
        }
        return Ok(last_good);
    }
    // A failed validation is *weak* evidence that the translation context rotated — it is
    // also what a transient read failure looks like, and transient read failures are exactly
    // what a running bruteforce produces: it hammers the KVM connector (a VM-exit per probe)
    // for ~700ms straight, so a concurrent dtb_read from an entity tick can fail purely from
    // contention while the DTB is perfectly good.
    //
    // Clearing LAST_GOOD on a single failure turned that into a self-sustaining loop:
    //   bruteforce load -> concurrent validate fails -> LAST_GOOD=0 -> spawn bruteforce ...
    // observed live as the same candidate (0x2136000) being "found" over and over, ~13ms
    // after the previous sweep reported success. Require a streak of consecutive failures
    // before discarding a DTB that validated at least once, so genuine rotation (which fails
    // every time, forever) still invalidates promptly while contention noise does not.
    if last_good != 0 {
        let streak = VALIDATION_FAIL_STREAK.fetch_add(1, Ordering::AcqRel) + 1;
        let now = now_ms();
        // Stamp the start of this failure run on the first failure only; a successful validation
        // above clears it back to 0.
        let since = match VALIDATION_FAIL_SINCE_MS.compare_exchange(
            0,
            now,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => now,
            Err(existing) => existing,
        };
        let failing_for = now.saturating_sub(since);
        if failing_for < VALIDATION_FAIL_GRACE_MS {
            tracing::debug!(
                dtb = %format!("{last_good:#x}"),
                streak,
                failing_for,
                "cr3: shadow DTB validation failed; keeping cached value for now"
            );
            // Serve the cached root instead of bailing. The log line above has always said
            // "keeping cached value for now" and the code then returned Err, which is not keeping
            // it — it is withholding it from every caller for the whole grace window.
            //
            // Caught by the new uptime instrument on its first run:
            //
            //   23:43:57  dtb: 0x4dd4000  active: true
            //   23:44:35  dtb: 0x0        active: false
            //   23:44:35  dtb: 0x4dd4000  active: true     <- same root, 0 s later
            //   23:44:36  dtb: 0x0        active: false
            //
            // The same DTB going inactive and active inside one second is not rotation. It is this
            // bail: one transient validation miss darkened the highlight mirror until the window
            // expired, then the next successful validation brought it straight back. Every one of
            // those gaps is a stretch where the renderer falls back to the game's shipped slot 79
            // and the glow visibly decays — the "sometimes" the user has been reporting all along.
            //
            // Returning `Ok(last_good)` is what the grace window was for. A read through a root
            // that really has rotated fails and the caller discards it, which is the same outcome
            // as the Err we used to return, and the mirror keeps working across every flicker that
            // is not a real rotation.
            return Ok(last_good);
        }
        tracing::info!(
            dtb = %format!("{last_good:#x}"),
            streak,
            failing_for,
            "cr3: shadow DTB failed validation for the full grace window — treating as rotated"
        );
        LAST_GOOD.store(0, Ordering::Release);
        VALIDATION_FAIL_STREAK.store(0, Ordering::Release);
        VALIDATION_FAIL_SINCE_MS.store(0, Ordering::Release);
        REVALIDATED_AT_MS.store(0, Ordering::Release);
    }

    // Step 4: collapse the ~1000 calls/sec that arrive while nothing is resolvable. The candidate
    // chain below can only change on a rotation, so re-walking it per caller buys nothing and
    // costs an EAAC lookup plus a full EPROCESS scan every time.
    let unresolved_at = LAST_UNRESOLVED_AT_MS.load(Ordering::Acquire);
    if unresolved_at != 0 && now_ms().saturating_sub(unresolved_at) < UNRESOLVED_BACKOFF_MS {
        anyhow::bail!("secondary DTB unresolved; backing off");
    }

    // Fast path 1: read the EAAC module's encrypted 128-entry shadow-DTB array.
    // One 1 KiB read + at most 128 physical page-table walks — far cheaper than EPROCESS
    // scan (which needs a kernel-VA walk) and orders of magnitude cheaper than bruteforce.
    // Falls through to EPROCESS scan, then VALID_CACHE, then bruteforce on failure.
    if memflow_backend::is_configured() {
        // Fast path 0: the second page-table root memflow already parsed for this process
        // (ProcessInfo::dtb2). Costs one PE-header validation — no module read, no EPROCESS
        // walk, no guest-physical sweep. Everything below stays as the fallback chain.
        let resolved = memflow_backend::dtb2_shadow_candidate(module_base);

        let resolved = if resolved.is_none() {
            memflow_backend::find_eaac_shadow_dtb(module_base).await
        } else {
            resolved
        };

        // Fast path 2: KPROCESS::UserDirectoryTableBase from EPROCESS (Nika's method).
        // Runs only if EAAC returned nothing (module not found / no entry validated).
        let resolved = if resolved.is_none() {
            memflow_backend::find_userdir_dtb_from_eprocess(module_base).await
        } else {
            resolved
        };

        // Every one of the three fast paths qualifies its own candidate with the PE header only,
        // which cannot tell the shadow root from EA's planted decoys or the exe's file-cache copy.
        // Re-qualify here through the shadow probe before committing, and drop a candidate the
        // probe disproves instead of caching it and discovering it's dead a tick later.
        let resolved = match resolved {
            Some(candidate) if validate_dtb(api, candidate, module_base).await => Some(candidate),
            Some(candidate) => {
                tracing::debug!(
                    candidate = %format!("{candidate:#x}"),
                    "cr3: fast-path candidate did not survive shadow-origin validation"
                );
                None
            }
            None => None,
        };

        if let Some(found_dtb) = resolved {
            if found_dtb != last_good {
                tracing::info!(
                    found_dtb = %format!("{found_dtb:#x}"),
                    "secondary CR3 resolved via EAAC array or EPROCESS (fast path)"
                );
                LAST_GOOD.store(found_dtb, Ordering::Release);
                let mut cache = VALID_CACHE.lock();
                if !cache.contains(&found_dtb) {
                    cache.push(found_dtb);
                }
            }
            LAST_GOOD_CONFIRMED_AT_MS.store(now_ms(), Ordering::Release);
            VALIDATION_FAIL_SINCE_MS.store(0, Ordering::Release);
            LAST_UNRESOLVED_AT_MS.store(0, Ordering::Release);
            return Ok(found_dtb);
        }
    }

    let cached: Vec<u64> = VALID_CACHE.lock().clone();
    for candidate in cached {
        if candidate != last_good && validate_dtb(api, candidate, module_base).await {
            LAST_GOOD.store(candidate, Ordering::Release);
            VALIDATION_FAIL_SINCE_MS.store(0, Ordering::Release);
            LAST_UNRESOLVED_AT_MS.store(0, Ordering::Release);
            return Ok(candidate);
        }
    }

    // Refuse to restart a sweep that just finished — without this the streak guard above can
    // still be outrun when many entity call sites miss at once right after a sweep lands.
    let last_done = LAST_BRUTEFORCE_DONE_MS.load(Ordering::Acquire);
    if last_done != 0 && now_ms().saturating_sub(last_done) < BRUTEFORCE_COOLDOWN_MS {
        LAST_UNRESOLVED_AT_MS.store(now_ms(), Ordering::Release);
        anyhow::bail!("secondary DTB unresolved; bruteforce cooling down");
    }

    // Why are we here at all? Reaching this point means nothing in the chain above resolved.
    // If `entry_last_good` is 0 the cached DTB was already gone before this call (so the
    // streak guard never saw it), which is a different bug from the cache being invalidated
    // by failed validation. Logged once per spawn, to file.
    if RESOLVE_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        // Inside the compare_exchange, which is what "once per spawn" above actually requires.
        //
        // Sitting outside it, this INFO fired once per *caller* rather than once per sweep: 10 991
        // copies in a 2.5-minute run, and 1 428 406 across the day, which is most of how
        // rolling.log reached 447 MB. Formatting and writing that is work done on the same threads
        // that are supposed to be servicing memflow reads.
        tracing::info!(
            entry_last_good = %format!("{last_good:#x}"),
            fail_streak = VALIDATION_FAIL_STREAK.load(Ordering::Acquire),
            valid_cache = VALID_CACHE.lock().len(),
            module_base = %format!("{module_base:#x}"),
            "cr3: falling through to bruteforce"
        );
        let api = api.clone();
        tokio::spawn(async move {
            match resolve_via_bruteforce(&api, module_base).await {
                Ok(candidate) => {
                    // The sweep already qualified this through `validate_dtb`, so reaching here
                    // means it satisfied the shadow probe (or no probe was available to judge
                    // it). Commit it; if it was only header-validated it will be re-qualified on
                    // the next call, and rejected for good then if the probe disproves it.
                    tracing::info!(
                        candidate = %format!("{candidate:#x}"),
                        "cr3: committing swept secondary CR3"
                    );
                    LAST_GOOD.store(candidate, Ordering::Release);
                    VALIDATION_FAIL_STREAK.store(0, Ordering::Release);
                    VALIDATION_FAIL_SINCE_MS.store(0, Ordering::Release);
                    LAST_GOOD_CONFIRMED_AT_MS.store(now_ms(), Ordering::Release);
                    let mut cache = VALID_CACHE.lock();
                    if !cache.contains(&candidate) {
                        cache.push(candidate);
                    }
                }
                Err(resolve_err) => {
                    tracing::warn!(
                        %resolve_err,
                        "{}",
                        "background secondary CR3 resolution failed"
                    );
                }
            }
            LAST_BRUTEFORCE_DONE_MS.store(now_ms(), Ordering::Release);
            RESOLVE_IN_FLIGHT.store(false, Ordering::Release);
        });
    }

    LAST_UNRESOLVED_AT_MS.store(now_ms(), Ordering::Release);
    anyhow::bail!("no secondary DTB currently resolved; background resolution in progress")
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

/// [`write_shadow`] with the DTB resolution hoisted out of the call.
///
/// [`write_shadow`] re-enters [`resolve_secondary_dtb`] on every call, which costs five
/// DTB-scoped PE-header reads each time. That is the right trade for a handful of per-entity
/// writes, but the global highlight table is 22 slots wide and is re-driven every actions tick —
/// resolving per slot would be 110 validation reads per pass. Callers that write a whole batch
/// resolve once with [`resolve_secondary_dtb`] and then drive every write in the batch through
/// this.
///
/// Unlike [`write_shadow`] there is no re-resolve-and-retry on failure: the caller owns the DTB
/// for the batch and a mid-batch rotation is better handled by failing the batch and letting the
/// next tick re-resolve than by half-applying it under two different roots.
pub async fn write_at_dtb<T: zerocopy::IntoBytes + zerocopy::Immutable>(
    api: &Api,
    dtb: u64,
    addr: u64,
    value: &T,
) -> anyhow::Result<()> {
    if dtb == 0 {
        anyhow::bail!("write_at_dtb called with null dtb");
    }
    if addr == 0 {
        anyhow::bail!("write_at_dtb called with null target");
    }
    // A write through a root that was never proven against live game data does not fail safely:
    // the translation resolves to *some* physical page and we modify it. One decoy root turned
    // this into 22 writes into unrelated guest memory per mirror pass, every few seconds, which
    // crashed the game far faster than the stale glow it was meant to fix.
    if !is_probe_confirmed(dtb) {
        anyhow::bail!(
            "refusing shadow write through dtb {dtb:#x}: not confirmed by the live \
             shadow-origin probe"
        );
    }
    dtb_write(api, dtb, addr, value).await
}

/// Read counterpart to [`write_at_dtb`] — same contract, same reason for existing.
///
/// Used to compare a pointer read under the primary DTB against the same read under the shadow
/// DTB. When the two disagree the primary connector is holding a decoy copy of whatever that
/// pointer addresses, which is the difference between a write the renderer consults and a write
/// it never looks at.
pub async fn read_at_dtb<T: zerocopy::FromBytes>(
    api: &Api,
    dtb: u64,
    addr: u64,
) -> anyhow::Result<T> {
    if dtb == 0 {
        anyhow::bail!("read_at_dtb called with null dtb");
    }
    if addr == 0 {
        anyhow::bail!("read_at_dtb called with null target");
    }
    dtb_read(api, dtb, addr).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `REJECTED_DTBS` is process-global, so these two tests share it and must not run
    /// concurrently with each other.
    static REJECT_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn rejected_dtbs_stay_sorted_and_deduplicated() {
        let _guard = REJECT_TEST_LOCK.lock();
        REJECTED_DTBS.lock().clear();

        // Insert deliberately out of order, with a duplicate.
        for dtb in [0xb349000u64, 0x1f4000, 0x2297000, 0x1f4000, 0x1000] {
            reject_dtb(dtb);
        }
        // Zero is never a root and must never occupy a blacklist slot.
        reject_dtb(0);

        let rejected = REJECTED_DTBS.lock().clone();
        let addresses: Vec<u64> = rejected.iter().map(|entry| entry.0).collect();
        assert_eq!(addresses, vec![0x1000, 0x1f4000, 0x2297000, 0xb349000]);

        drop(rejected);
        assert!(is_rejected_dtb(0x1f4000));
        assert!(is_rejected_dtb(0xb349000));
        assert!(!is_rejected_dtb(0x1f5000));
        assert!(!is_rejected_dtb(0));

        REJECTED_DTBS.lock().clear();
    }

    /// The eviction path recomputes its insert index after `remove(0)` shifts the vector. Getting
    /// that wrong silently corrupts the ordering, which would make `binary_search` miss and the
    /// sweep start re-committing decoys again — the exact bug the blacklist exists to prevent.
    #[test]
    fn rejected_dtbs_stay_sorted_across_eviction() {
        let _guard = REJECT_TEST_LOCK.lock();
        REJECTED_DTBS.lock().clear();

        // Fill to capacity with ascending roots, leaving a gap at the bottom to insert into.
        for i in 0..REJECTED_DTBS_MAX as u64 {
            reject_dtb((i + 2) * PAGE_SIZE);
        }
        assert_eq!(REJECTED_DTBS.lock().len(), REJECTED_DTBS_MAX);

        // This must evict the lowest entry and still land in sorted position at the front.
        reject_dtb(PAGE_SIZE);

        let rejected = REJECTED_DTBS.lock().clone();
        assert_eq!(rejected.len(), REJECTED_DTBS_MAX);
        assert!(
            rejected.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "blacklist lost its ordering across eviction"
        );
        assert_eq!(rejected[0].0, PAGE_SIZE);

        drop(rejected);
        assert!(is_rejected_dtb(PAGE_SIZE));
        // The evicted entry is genuinely gone, not merely unreachable by binary search.
        assert!(!is_rejected_dtb(2 * PAGE_SIZE));

        REJECTED_DTBS.lock().clear();
    }

    /// A rejection must expire. It was permanent for the session, so one wrong verdict from the
    /// shadow-origin probe — which reads its two references a tick apart and can legitimately
    /// disagree while the local player is moving fast — excluded the real root from every
    /// subsequent scan. Live symptom: ten consecutive priority-band scans finding nothing across a
    /// two-minute blackout, while 26 of the 29 roots confirmed that day sat inside that band.
    #[test]
    fn rejected_dtbs_expire_after_their_ttl() {
        let _guard = REJECT_TEST_LOCK.lock();
        REJECTED_DTBS.lock().clear();

        reject_dtb(0x4dd4000);
        assert!(is_rejected_dtb(0x4dd4000), "a fresh rejection must be honoured");

        // Backdate the stamp past the TTL rather than sleeping for a minute.
        {
            let mut rejected = REJECTED_DTBS.lock();
            let stamp = &mut rejected[0].1;
            *stamp = stamp.saturating_sub(REJECTED_DTB_TTL_MS + 1);
        }
        assert!(
            !is_rejected_dtb(0x4dd4000),
            "an expired rejection must stop excluding the candidate"
        );

        // Re-rejecting refreshes the stamp instead of inserting a duplicate.
        reject_dtb(0x4dd4000);
        assert_eq!(REJECTED_DTBS.lock().len(), 1);
        assert!(is_rejected_dtb(0x4dd4000));

        REJECTED_DTBS.lock().clear();
    }

    #[test]
    fn shadow_probe_rejects_unusable_publications() {
        *SHADOW_PROBE.lock() = None;

        const Z: [f32; 3] = [0.0; 3];

        // A null shadow pointer is never a probe.
        publish_shadow_probe(0, [100.0, 200.0, 300.0], [100.0, 200.0, 370.0]);
        assert!(SHADOW_PROBE.lock().is_none());

        // Neither reference usable is "no position yet", not a probe: accepting it would let a
        // decoy root that reads zeros look like a perfect match.
        publish_shadow_probe(0x1234_5000, Z, Z);
        assert!(SHADOW_PROBE.lock().is_none());
        // Unmigrated-shadow-region garbage must not become a reference either.
        publish_shadow_probe(0x1234_5000, [f32::NAN, 0.0, 0.0], Z);
        assert!(SHADOW_PROBE.lock().is_none());
        publish_shadow_probe(0x1234_5000, [MAX_PLAUSIBLE_COORD * 2.0, 0.0, 0.0], Z);
        assert!(SHADOW_PROBE.lock().is_none());

        // Either reference alone is enough — that is the whole point of carrying two, since the
        // dup offset is hardcoded and may be stale while the camera offset is profile-resolved.
        publish_shadow_probe(0x1234_5000, [1024.0, -2048.0, 96.0], Z);
        let probe = SHADOW_PROBE.lock().expect("origin-only probe is accepted");
        assert_eq!(probe.origin, [1024.0, -2048.0, 96.0]);
        assert!(probe.camera.is_none());

        publish_shadow_probe(0x1234_5000, [f32::NAN, 0.0, 0.0], [10.0, 20.0, 30.0]);
        let probe = SHADOW_PROBE.lock().expect("camera-only probe is accepted");
        assert_eq!(probe.origin, Z, "unusable origin must not be kept as a reference");
        assert_eq!(probe.camera, Some([10.0, 20.0, 30.0]));

        publish_shadow_probe(0x1234_5000, [1024.0, -2048.0, 96.0], [1024.0, -2048.0, 166.0]);
        let probe = SHADOW_PROBE.lock().expect("plausible probe is accepted");
        assert_eq!(probe.shadow_ptr, 0x1234_5000);
        assert_eq!(probe.origin, [1024.0, -2048.0, 96.0]);
        assert_eq!(probe.camera, Some([1024.0, -2048.0, 166.0]));

        *SHADOW_PROBE.lock() = None;
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

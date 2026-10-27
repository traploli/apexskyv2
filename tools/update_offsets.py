#!/usr/bin/env python3
"""Fetch or import Apex Legends offsets and safely merge them into Apexsky's native INI.

The API's ETag is persisted next to the profile. A 304 leaves the profile
untouched. Unknown API fields are ignored; existing Apexsky fields absent
from the dump are deliberately preserved. Network updating is opt-in in the
launch/build wrappers; --reference imports a local dump selectively.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import ssl
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

DEFAULT_URL = "https://www.cheatoffsets.com/api/games/apex/current"
HEX = re.compile(r"^0x[0-9a-fA-F]+$")

# API names that do not use the native INI spelling.
MISC_KEYS = {
    "ClientState": "ClientState",
    "GlobalVars": "GlobalVars",
    "HighlightSettings": "HighlightSettings",
    "InputSystem": "InputSystem",
    "LevelName": "LevelName",
    "LocalEntityHandle": "LocalEntityHandle",
    "LocalPlayer": "LocalPlayer",
    "ModelNames": "ModelNames",
    "NameList": "NameList",
    "NetworkVarTablePtr": "NetworkVarTablePtr",
    "SignonState": "SignonState",
    "ViewAngles": "CPlayer!camera_angles",
    "ViewMatrix": "ViewMatrix",
    "ViewRender": "ViewRender",
    "WeaponNames": "WeaponNames",
    "WeaponSettingsMeta_base": "WeaponSettingsMeta_base",
    "camera_origin": "CPlayer!camera_origin",
    "cl_entitylist": "cl_entitylist",
    "lastVisibleTime_2": "CPlayer!lastVisibleTime",
    "m_flProjectileScale": "CWeaponX!m_flProjectileScale",
    "m_flProjectileSpeed": "CWeaponX!m_flProjectileSpeed",
}


def parse_ini(text: str):
    sections: dict[str, dict[str, int]] = {}
    section = ""
    for n, line in enumerate(text.splitlines()):
        m = re.match(r"^\[([^]]+)\]", line)
        if m:
            section = m.group(1)
            sections.setdefault(section, {})
            continue
        m = re.match(r"^([^=;\s][^=]*)=(.*)$", line)
        if m:
            sections.setdefault(section, {})[m.group(1)] = n
    return sections


def candidates(group: str, key: str):
    if group == "misc":
        k = MISC_KEYS.get(key)
        return [("Miscellaneous", k)] if k else []
    if group == "buttons":
        return [("Buttons", "in_break" if key == "break_" else "in_" + key)]
    if group == "NetworkedStringTables":
        return [("NetworkedStringTables", key)]
    if group == "convars":
        return [("ConVars", key)]
    if key.startswith("DT_"):
        return [("RecvTable." + key, key)]
    if group.startswith("C_"):
        # The API uses underscores for this nested datamap field, while the
        # native Apexsky profile uses the dotted INI spelling.
        if group == "C_BaseEntity" and key == "m_currentFrame_viewOffset":
            return [("DataMap.C_BaseEntity", "m_currentFrame.viewOffset")]
        return [("DataMap." + group, key), ("RecvTable." + group, key)]
    return [(group, key), ("DataMap." + group, key), ("RecvTable." + group, key)]


def api_values(payload: dict):
    flat = payload.get("offsets_flat")
    if not isinstance(flat, dict):
        raise ValueError("response has no offsets_flat object")
    nested = payload.get("offsets", {}).get("structs", {})
    values = []
    for dotted, raw in flat.items():
        if not isinstance(dotted, str) or not isinstance(raw, str) or not HEX.fullmatch(raw):
            continue
        if "." not in dotted:
            continue
        group, key = dotted.split(".", 1)
        values.append((group, key, raw.lower()))
    # Prefer the flat representation, but ensure it is actually present.
    if not values or not isinstance(nested, dict):
        raise ValueError("response contains no usable offsets")
    return values


def merge(profile: Path, payload: dict):
    original = profile.read_text()
    lines = original.splitlines()
    sections = parse_ini(original)
    updates = {}
    unmatched = []
    for group, key, value in api_values(payload):
        hit = next(((section, native_key) for section, native_key in candidates(group, key)
                     if native_key in sections.get(section, {})), None)
        if hit:
            updates[hit] = value
        else:
            unmatched.append(f"{group}.{key}")
    for (section, key), value in updates.items():
        line_no = sections[section][key]
        lines[line_no] = re.sub(r"(?<=^" + re.escape(key) + r")=.*$", "=" + value, lines[line_no])
    changed = "\n".join(lines) + ("\n" if original.endswith("\n") else "")
    return original, changed, updates, unmatched


REFERENCE_SECTIONS = {"Miscellaneous", "DataMap.C_BaseEntity"}
REFERENCE_KEYS = {
    "TimeDateStamp", "CheckSum", "GameVersion", "cl_entitylist",
    "LocalEntityHandle", "LocalPlayer", "GlobalVars", "NameList",
    "CPlayer!camera_origin", "CPlayer!camera_angles",
    "CBaseAnimating!m_pStudioHdr", "HighlightSettings", "NetworkVarTableLen",
    "m_currentFrame.viewOffset", "m_vecAbsVelocity", "m_vecAbsOrigin",
}


def merge_reference(profile: Path, reference: Path):
    """Import only safe, semantically matched keys from an offline dump."""
    original = profile.read_text()
    reference_text = reference.read_text()
    lines = original.splitlines()
    sections = parse_ini(original)
    source = parse_ini(reference_text)
    updates = {}
    for section in REFERENCE_SECTIONS:
        for key in REFERENCE_KEYS:
            source_line = source.get(section, {}).get(key)
            target_line = sections.get(section, {}).get(key)
            if source_line is None or target_line is None:
                continue
            raw = reference_text.splitlines()[source_line].split("=", 1)[1].strip()
            if key != "GameVersion" and not HEX.fullmatch(raw):
                continue
            updates[(section, key)] = raw
            lines[target_line] = re.sub(
                r"(?<=^" + re.escape(key) + r")=.*$", "=" + raw, lines[target_line]
            )
    if not updates:
        raise ValueError(f"reference contains no safe keys present in {profile}")
    changed = "\n".join(lines) + ("\n" if original.endswith("\n") else "")
    return original, changed, updates


def reference_metadata(reference: Path):
    text = reference.read_text()
    sections = parse_ini(text)
    result = {}
    for key in ("GameVersion", "TimeDateStamp", "CheckSum"):
        line = sections.get("Miscellaneous", {}).get(key)
        if line is not None:
            result[key] = text.splitlines()[line].split("=", 1)[1].strip()
    return result


def validate_candidate(text: str):
    sections = parse_ini(text)
    required = {
        "Miscellaneous": {
            "cl_entitylist", "LocalEntityHandle", "LocalPlayer", "GlobalVars",
            "InputSystem", "NameList", "ViewRender", "ViewMatrix", "ClientState",
            "SignonState", "LevelName", "HighlightSettings", "NetworkVarTablePtr",
        },
        "NetworkedStringTables": {"WeaponNames"},
        "Buttons": {"in_attack", "in_jump", "in_duck", "in_forward", "in_backward"},
    }
    missing = [f"[{section}] {key}" for section, keys in required.items()
               for key in keys if key not in sections.get(section, {})]
    if missing:
        raise ValueError("candidate is missing required native keys: " + ", ".join(missing))
    # Reject accidental non-offset values in the native sections touched by the API.
    for section, keys in sections.items():
        for key, line_no in keys.items():
            value = text.splitlines()[line_no].split("=", 1)[1].strip()
            if key in required.get(section, set()) and not HEX.fullmatch(value):
                raise ValueError(f"invalid value for [{section}] {key}: {value!r}")


def atomic_write(path: Path, text: str):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "w", newline="") as f:
            f.write(text)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def fetch(url: str, etag_path: Path):
    headers = {"Accept": "application/json", "User-Agent": "apexsky-offset-updater/1.0"}
    if etag_path.exists():
        etag = etag_path.read_text().strip()
        if etag:
            headers["If-None-Match"] = etag
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=15, context=ssl.create_default_context()) as response:
            body = response.read()
            return response.status, response.headers.get("ETag", ""), json.loads(body)
    except urllib.error.HTTPError as exc:
        if exc.code == 304:
            return 304, etag_path.read_text().strip() if etag_path.exists() else "", None
        raise RuntimeError(f"offset API returned HTTP {exc.code}") from exc


def update_once(args) -> bool:
    if args.reference:
        original, changed, updates = merge_reference(args.profile, args.reference)
        validate_candidate(changed)
        if changed != original:
            backup = args.profile.with_suffix(args.profile.suffix + ".pre-reference.bak")
            if not backup.exists():
                atomic_write(backup, original)
            atomic_write(args.profile, changed)
        metadata = reference_metadata(args.reference)
        print(
            f"imported {len(updates)} safe reference keys from {args.reference}; "
            f"latest={metadata.get('GameVersion', '<unknown>')} "
            f"checksum={metadata.get('CheckSum', '<unknown>')}"
        )
        return changed != original
    status, etag, payload = fetch(args.url, args.etag_file)
    if status == 304:
        print("not modified (304)")
        return False
    original, changed, updates, unmatched = merge(args.profile, payload)
    if len(updates) < args.min_updates:
        raise RuntimeError(f"refusing incomplete update: only {len(updates)} native keys matched")
    validate_candidate(changed)
    if changed != original:
        backup = args.profile.with_suffix(args.profile.suffix + ".pre-api.bak")
        if not backup.exists():
            atomic_write(backup, original)
        atomic_write(args.profile, changed)
    if etag:
        atomic_write(args.etag_file, etag + "\n")
    print(f"updated {len(updates)} native keys; unmatched API keys: {len(unmatched)}; etag={etag or '<none>'}")
    return True


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--url", default=DEFAULT_URL)
    p.add_argument("--profile", type=Path, required=True)
    p.add_argument("--etag-file", type=Path, required=True)
    p.add_argument("--reference", type=Path,
                   help="offline reference INI; skips the network updater")
    p.add_argument("--min-updates", type=int, default=100)
    p.add_argument("--interval", type=int, default=0, help="poll interval in seconds; 0 means once")
    args = p.parse_args()
    while True:
        try:
            update_once(args)
        except Exception as exc:
            print(f"update failed: {exc}")
            raise SystemExit(1)
        if not args.interval:
            return
        time.sleep(args.interval)


if __name__ == "__main__":
    main()

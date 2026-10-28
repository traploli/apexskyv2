use std::sync::LazyLock as Lazy;

use named_constants::named_constants;
use secrecy::SecretString;

/*
 * GameVersion=3.0.2.23
 */

pub static GAME_VER_DX12_PROCESS_NAME: Lazy<SecretString> =
    Lazy::new(|| SecretString::from(obfstr::obfstr!("r5apex_dx12.exe")));

// m_ammoPoolCount - 0x8
pub const OFFSET_YAW: u64 = 0x229c - 0x8;
//pub const OFFSET_GLOW_CONTEXT_ID: u64 = 0x29c;
// Through-wall glow used to live here as three hand-tracked literals (OFFSET_GLOW_VISIBLE_TYPE /
// _DISTANCE / _FIX @ 0x25c/0x254/0x274) that rotted because nothing refreshed them. They were
// forum reverse-guesses (the 0x26c -> 0x25c "drift" lineage off UC thread 716406) that the
// authoritative DT_BaseEntity RecvTable dump for GameVersion v3.0.3.65 does NOT contain — the
// writes landed in unnamed padding below m_wantsScopeHighlight, which is why no 1/2/5 value ever
// rendered. The real through-wall control is m_passThroughFlags @ 0x35c, now sourced from the INI
// pipeline as G_OFFSETS.entity_pass_through_flags so it auto-updates with the rest of G_OFFSETS.
// Far-fade is already handled by G_OFFSETS.entity_fade_dist (m_fadeDist @ 0x764); the old
// _DISTANCE/_FIX writes hit dead space and have been removed.
//pub const OFFSET_MODULE_BASE: u64 = 0x140000000;

#[allow(dead_code)]
#[named_constants]
#[derive(Copy, Clone, Default, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum WeaponId {
    R301 = 0,
    Sentinel = 2,
    Bow = 3,
    R2R5 = 4,
    Rampage = 7,
    Alternator = 86,
    Re45 = 87,
    Re45Crate = 88,
    ChargeRifle = 89,
    Devotion = 90,
    DevotionCrate = 91,
    Longbow = 92,
    Havoc = 93,
    HavocCrate = 94,
    Eva8 = 95,
    Eva8Crate = 96,
    Flatline = 97,
    G7Scout = 98,
    Hemlock = 99,
    HermlockCrate = 100,
    Kraber = 101,
    Lstar = 102,
    LstarCrate = 103,
    Mastiff = 104,
    MastiffCrate = 105,
    Mozambique = 106,
    MozambiqueLight = 107,
    MozambiqueEnergy = 108,
    MozambiqueSniper = 109,
    MozambiqueHeavy = 110,
    Prowler = 111,
    ProwlerCrate = 112,
    Peacekeeper = 113,
    PeacekeeperCrate = 114,
    R99 = 115,
    R99Crate = 116,
    P2020 = 117,
    Spitfire = 118,
    TripleTake = 119,
    Wingman = 121,
    WingmanCrate = 122,
    Volt = 123,
    _3030Repeater = 124,
    CarSmg = 125,
    Nemesis = 126,
    GrenadeFrag = 127,
    GrenadeArcStar = 128,
    GrenadeThermite = 129,
    Hands = 130,
    Epg = 184,
    EpgTethered = 185,
    ThrowingKnife = 186,
    Max,
}

#[allow(dead_code)]
#[named_constants]
#[derive(Copy, Clone, Default, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ItemId {
    ApexskyItemDeathBox = -1,
    None = 0,
    WeaponKraber = 1,
    WeaponMastiff = 2,
    WeaponLStar = 8,
    WeaponHavoc = 14,
    WeaponDevotion = 20,
    WeaponTripleTake = 27,
    WeaponFlatline = 32,
    WeaponHemlock = 37,
    WeaponG7Scout = 43,
    WeaponAlternator = 48,
    WeaponR99 = 53,
    WeaponProwler = 59,
    WeaponVolt = 65,
    WeaponLongbow = 70,
    WeaponChargeRifle = 75,
    WeaponSpitfire = 80,
    WeaponR301 = 85,
    WeaponEva8 = 90,
    WeaponPeacekeeper = 97,
    WeaponMozambique = 102,
    WeaponWingman = 118,
    WeaponP2020 = 124,
    WeaponRE45 = 133,
    WeaponSentinel = 139,
    WeaponBow = 146,
    Weapon3030Repeater = 152,
    WeaponNemesis = 157,
    LightAmmo = 162,
    EnergyAmmo = 163,
    ShotgunAmmo = 164,
    HeavyAmmo = 165,
    SniperAmmo = 166,
    WeaponRampage = 168,
    WeaponCARSMG = 173,
    Accelerant = 210,
    Phoenix = 211,
    HealthLarge = 212,
    HealthSmall = 213,
    ShieldBatteryLarge = 214,
    ShieldBatterySmall = 215,
    //ShieldUpgradeHead1,
    //ShieldUpgradeHead2,
    ShieldUpgradeHead3 = 220,
    ShieldUpgradeHead4 = 221,
    ArmorCore1 = 233,
    ArmorCore2 = 234,
    ArmorCore3 = 235,
    //ArmorCore4,
    ShieldDown1 = 238,
    ShieldDown2 = 239,
    ShieldDown3 = 240,
    //ShieldDown4,
    LightBackpack = 242,
    MedBackpack = 243,
    HeavyBackpack = 244,
    GoldBackpack = 245,
    ThrowingKnife = 246,
    GrenadeThermite = 247,
    GrenadeFrag = 248,
    GrenadeArcStar = 249,
    Optic1xHCOG = 250,
    Optic2xHCOG = 251,
    OpticHolo1x = 252,
    OpticHolo1x2x = 253,
    OpticThreat = 254,
    Optic3xHCOG = 255,
    Optic2x4x = 256,
    OpticSniper6x = 257,
    OpticSniper4x8x = 258,
    OpticSniperThreat = 259,
    Suppressor1 = 260,
    Suppressor2 = 261,
    Suppressor3 = 262,
    LaserSight1 = 264,
    LaserSight2 = 265,
    LaserSight3 = 266,
    LightAmmoMag1 = 267,
    LightAmmoMag2 = 268,
    LightAmmoMag3 = 269,
    LightAmmoMag4 = 270,
    HeavyAmmoMag1 = 271,
    HeavyAmmoMag2 = 272,
    HeavyAmmoMag3 = 273,
    HeavyAmmoMag4 = 274,
    EnergyAmmoMag1 = 275,
    EnergyAmmoMag2 = 276,
    EnergyAmmoMag3 = 277,
    EnergyAmmoMag4 = 278,
    SniperAmmoMag1 = 279,
    SniperAmmoMag2 = 280,
    SniperAmmoMag3 = 281,
    SniperAmmoMag4 = 282,
    ShotgunBolt1 = 283,
    ShotgunBolt2 = 284,
    ShotgunBolt3 = 285,
    ShotgunBolt4 = 286,
    StockRegular1 = 287,
    StockRegular2 = 288,
    StockRegular3 = 289,
    StockSniper1 = 290,
    StockSniper2 = 291,
    StockSniper3 = 292,
    SelectfireReceiver = 295,
    DisruptorRounds = 296,
    TurboCharger = 297,
    SkullPiecer = 298,
    HammerPoint = 307,
    BoostedLoader = 308,
    GunShieldGenerator = 309,
}

pub const HIGHLIGHT_LOOT_GOLD: u8 = 31;
pub const HIGHLIGHT_LOOT_RED: u8 = 32;
pub const HIGHLIGHT_LOOT_PURPLE: u8 = 33;
pub const HIGHLIGHT_LOOT_BLUE: u8 = 34;
pub const HIGHLIGHT_LOOT_WHITE: u8 = 35;
pub const HIGHLIGHT_LOOT_ENERGY: u8 = 36;
pub const HIGHLIGHT_LOOT_HEAVY: u8 = 37;
pub const HIGHLIGHT_LOOT_LIGHT: u8 = 38;
pub const HIGHLIGHT_LOOT_GREY: u8 = 39;
pub const HIGHLIGHT_DEATH_BOX: u8 = 40;
pub const HIGHLIGHT_PLAYER_KNOCKED: u8 = 70;
pub const HIGHLIGHT_PLAYER_VISIBLE: u8 = 71;
pub const HIGHLIGHT_PLAYER_NOTVIZ: u8 = 72;
/// Through-wall-capable twin of [`HIGHLIGHT_PLAYER_NOTVIZ`]. The latter is also reused (see
/// `player_filled_toggle` in `workers/actions.rs::player_glow`) as the generic "filled" style
/// for every target regardless of visibility, and a filled/inside-render highlight needs the
/// model mesh actually rasterized — which occlusion/frustum culling skips entirely for an
/// occluded entity. Confirmed on UC thread 716406 (post #847, 30 Jan 2026): "the invisible glow
/// doesnt even render through walls anymore" specifically on the filled variant. The default
/// not-visible case (no fill toggle) needs the outline-only style instead, same technique
/// `HIGHLIGHT_PLAYER_VISIBLE` already uses successfully — outline rendering is a dedicated
/// stencil-silhouette pass that bypasses normal occlusion, fill rendering is not.
pub const HIGHLIGHT_PLAYER_NOTVIZ_OUTLINE: u8 = 73;
pub const HIGHLIGHT_PLAYER_ORANGE: u8 = 74;
pub const HIGHLIGHT_PLAYER_WHITE: u8 = 75;
pub const HIGHLIGHT_PLAYER_BLUE: u8 = 76;
pub const HIGHLIGHT_PLAYER_PURPLE: u8 = 77;
pub const HIGHLIGHT_PLAYER_RED: u8 = 78;
pub const HIGHLIGHT_PLAYER_RAINBOW: u8 = 79;
pub const HIGHLIGHT_PLAYER_BLACK: u8 = 80;
pub const HIGHLIGHT_WEAPON_RAINBOW: u8 = 81;

#[derive(Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub struct LootModelsItem {
    pub int: i32,
    pub models: Vec<String>,
}

#[cfg(feature = "data")]
pub static ITEM_LIST: Lazy<std::collections::HashMap<i32, std::collections::HashSet<String>>> =
    Lazy::new(|| {
        include_flate::flate!(static ITEM_JSON: [u8] from "resource/default/item.json" with zstd);
        let data: Vec<LootModelsItem> = serde_json::from_slice(&ITEM_JSON).unwrap();
        data.into_iter()
            .map(|item| (item.int, std::collections::HashSet::from_iter(item.models)))
            .collect()
    });
#[cfg(feature = "data")]
pub static WEAPON_LIST: Lazy<Vec<String>> = Lazy::new(|| {
    include_flate::flate!(static WEAPON_JSON: [u8] from "resource/default/weapon.json" with zstd);
    serde_json::from_slice(&WEAPON_JSON).unwrap()
});

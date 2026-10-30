use anyhow::Context;
use apex1_common::pb::apexlegends::{AimKeyState, AimTargetInfo, SpectatorInfo, TreasureClue};
use apex1_common::utils::get_unix_timestamp_in_millis;
use apex1_common::{
    aimbot::{AimEntity, calc_angle, calc_fov},
    config::Settings,
    love_players::LoveStatus,
};
use ndarray::arr1;
use ohosky_api::common::dmalib::IMemAccess;
use ohosky_api::common::msg::ISharedWatchValue;
use ohosky_api::common::share::ISharableValue;
use std::{collections::HashMap, sync::Arc, time::Duration};
use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use zerocopy::IntoBytes;
use tokio::sync::watch;
use tokio::time::{Instant, sleep};
use tracing::{Instrument, info_span};

use crate::G_OFFSETS;
use crate::PRINT_LATENCY;
use crate::SharedStateType;
use crate::apexdream::{
    base::math,
    sdk::HighlightBits,
    state::GameState,
    state::entities::{BaseNPCEntity, DeathboxEntity, Entity, LootEntity},
};
use crate::game::{data::*, player::GamePlayer, player::QuickLooting};
use crate::global_state::G_STATE;
use crate::obfstr as s;
use crate::skyapi::{self, dmalib};
use crate::workers::items::LootInt;

const REQUEST_ID: usize = 0; //obfstr::random!(usize);

#[tracing::instrument(skip_all)]
pub async fn actions_loop(
    mut active: watch::Receiver<bool>,
    shared_state: SharedStateType,
    access_tx: crate::MemAccess,
    aim_key_tx: watch::Sender<AimKeyState>,
    aim_select_tx: watch::Sender<Vec<AimTargetInfo>>,
    update_time_tx: watch::Sender<f64>,
    mut aim_select_rx: watch::Receiver<Vec<AimTargetInfo>>,
    mut items_glow_rx: watch::Receiver<Vec<(u64, u8)>>,
) -> anyhow::Result<()> {
    tracing::debug!("{}", s!("task start"));

    let event_tick = skyapi::SharedWatchValue::new(&apex1_common::global::MSG.action_tick, "").0;
    let event_attached =
        skyapi::SharedWatchValue::new(&apex1_common::global::MSG.game_attached, "").0;
    let event_unattached =
        skyapi::SharedWatchValue::new(&apex1_common::global::MSG.game_unattached, "").0;
    fn usermod_send_event<T: ISharableValue>(tx: &impl ISharedWatchValue<T>, v: T) {
        if let Err(e) = tx.update(v) {
            tracing::error!(%e, "{}", s!("usermod_send_event"));
        };
    }

    let mut apexdream =
        crate::apexdream::Instance::new(G_STATE.lock().unwrap().config.settings.is_access_fast);
    let mut start_instant = Instant::now();
    let mut fps_checkpoint_instant = Instant::now();
    let mut last_checkpoint_frame: i32 = 0;
    let mut actions_tick: i64 = -1;
    let mut log_items: usize = 0;
    let mut log_players: usize = 0;
    let mut world_ready: bool;
    let mut player_ready: bool;

    while *active.borrow_and_update() {
        sleep(Duration::from_secs(2)).await;

        match access_tx.get_baseaddr(100).await? {
            Some(baseaddr) => {
                shared_state
                    .game_baseaddr
                    .store(baseaddr, Ordering::Release);
                usermod_send_event(&event_attached, ());
            }
            None => {
                shared_state.game_baseaddr.store(0, Ordering::Release);
                continue;
            }
        }

        while *active.borrow_and_update() {
            sleep(Duration::from_millis(2)).await; // don't change xD

            let loop_duration = start_instant.elapsed().as_millis().try_into()?;
            start_instant = Instant::now();

            let apex_base = match access_tx.get_baseaddr(100).await? {
                Some(addr) => {
                    shared_state.game_baseaddr.store(addr, Ordering::Release);
                    addr
                }
                None => {
                    shared_state.game_baseaddr.store(0, Ordering::Release);
                    usermod_send_event(&event_unattached, ());
                    tracing::warn!("{}", s!("Unattached to the game"));
                    break;
                }
            };
            let mem = &access_tx;

            /* Hot Variables Update Begin */

            actions_tick += 1;
            let verbose = actions_tick % 1_000 == 0;

            // Tick game state
            let (tick_duration, apex_state, apex_update_ctx, is_newly_connected) = {
                (
                    {
                        let mut api = crate::apexdream::api::Api {
                            apex_base: apex_base.into(),
                            mem_access: mem.clone(),
                            req_id: REQUEST_ID,
                        };
                        let tick_start = Instant::now();
                        apexdream.tick_state(&mut api).await;
                        tick_start.elapsed().as_millis().try_into()?
                    },
                    apexdream.get_state(),
                    apexdream.get_update_ctx(),
                    apexdream.is_newly_connected(),
                )
            };

            if verbose {
                tracing::trace!(loop_duration, tick_duration, ?apex_state.client, "{}", s!("game state update"));
            }

            // // Update spectator checker
            // if actions_tick % 15 == 0 && player_ready {
            //     let lplayer_ptr = apex_state.client.local_player_ptr;
            //     // Init spectator checker
            //     if prev_lplayer_ptr != lplayer_ptr {
            //         init_spec_checker(lplayer_ptr);
            //         prev_lplayer_ptr = lplayer_ptr;
            //     }
            //     // Update yaw to spec checker
            //     apex_state.players().for_each(|pl| {
            //         if pl.eadp_uid == 0 || pl.team_num < 0 || pl.team_num > 50 {
            //             return;
            //         }
            //         apexsky::tick_yaw(pl.entity_ptr.into_raw(), pl.yaw);
            //     });
            // }

            // Calc game FPS
            let game_fps_update = {
                let delta_frame = apex_state.client.framecount - last_checkpoint_frame;
                if delta_frame < 90 {
                    None
                } else if delta_frame > 120 {
                    last_checkpoint_frame = apex_state.client.framecount;
                    fps_checkpoint_instant = Instant::now();
                    None
                } else {
                    let duration = fps_checkpoint_instant.elapsed().as_secs_f32();
                    //trace!(delta_frame, duration, "{}", s!("calc game fps"));

                    last_checkpoint_frame = apex_state.client.framecount;
                    fps_checkpoint_instant = Instant::now();

                    Some(delta_frame as f32 / duration)
                }
            };

            // To hold only one lock at a time, clone G_STATE.config.settings
            let g_settings = G_STATE.lock().unwrap().config.settings.clone();
            // if verbose {
            //     trace!(?g_settings);
            // }

            // Update client state
            {
                // Accumulate on EVERY tick, not just on diagnostic ticks — that sampling gap is
                // exactly what made the last run's input data inconclusive.
                BUTTON_UNION.observe(
                    &apex_state.input_system.button_state,
                    apex_state.buttons.in_attack.down[0] != 0,
                    apex_state.buttons.in_zoom.down[0] != 0,
                );

                world_ready = apex_state.is_in_game() && apex_state.local_player().is_some();
                player_ready = world_ready
                    && shared_state
                        .players
                        .read()
                        .get(&shared_state.local_player_ptr.load(Ordering::Acquire))
                        .is_some();

                shared_state
                    .world_ready
                    .store(world_ready, Ordering::Release);
                shared_state
                    .frame_count
                    .store(apex_state.client.framecount, Ordering::Release);
                shared_state
                    .tick_num
                    .store(actions_tick.try_into().unwrap(), Ordering::Release);
                shared_state
                    .tick_duration
                    .store(tick_duration, Ordering::Release);
                shared_state
                    .actions_duration
                    .store(loop_duration, Ordering::Release);

                {
                    let mut lock = shared_state.game_fps.lock();
                    if let Some(fps_update) = game_fps_update {
                        tracing::trace!(fps_update);
                        *lock = fps_update;
                    } else if *lock < f32::EPSILON {
                        *lock = g_settings.game_fps;
                    }
                }
                *shared_state.view_matrix.lock() = apex_state.client.view_matrix;

                if PRINT_LATENCY {
                    println!(
                        "{}{:.1}",
                        s!("actions data latency "),
                        get_unix_timestamp_in_millis() as f64 - apex_state.time * 1000.0
                    );
                }

                if verbose {
                    if !world_ready {
                        let local_player = apex_state.local_player();
                        tracing::debug!(
                            signon_state = apex_state.client.signon_state,
                            levelname = apex_state.client.level_name,
                            local_ent = ?apex_state.client.local_entity,
                            ?local_player,
                        );
                        if local_player.is_none() {
                            tracing::debug!(entity_list = ?apex_state.entity_list);
                        }
                    }
                    tracing::trace!(?shared_state, localplayer = ?apex_state.local_player());
                }
            }

            // Unconditional heartbeat — written every 120 ticks regardless of game state.
            // If glow_diag.txt never appears, the inner while loop isn't executing at all.
            if actions_tick % 120 == 0 {
                // `targets` is the decisive one for "works in firing range, not in a match":
                // task_player_glow only writes to entities in this list, and
                // collect_aim_targets returns an EMPTY vec whenever the local player is
                // missing from the players map (same condition as player_ready=false).
                // targets=0 in a match means no glow write is even attempted — a completely
                // different failure from the writes landing on a shadow-mapped decoy page.
                // lplayer=0x0 means the local player was dropped by the players-map filter at
                // the `pl.eadp_uid == 0 || pl.team_num < 0 || pl.team_num > 10000` check, which
                // forces collect_aim_targets to early-return an empty vec (targets=0) and so no
                // glow write is attempted at all. Report the two fields that filter tests so we
                // can see WHICH condition rejects it rather than guessing.
                let (lp_uid, lp_team, lp_raw) = apex_state
                    .local_player()
                    .map(|p| (p.eadp_uid, p.team_num, p.get_entity_ptr()))
                    .unwrap_or((0, -1, 0));
                let _ = std::fs::write(
                    "/home/sad/Music/apexsky-next/apexsky/dist-memflow-test-v11/log/glow_diag.txt",
                    format!(
                        "tick={} world_ready={} player_glow={} player_ready={} targets={} lplayer={:#x}\n\
                         lp_raw={:#x} lp_eadp_uid={} lp_team={} players_map={} aim_entities={}\n",
                        actions_tick,
                        world_ready,
                        g_settings.glow_settings.player_glow,
                        player_ready,
                        aim_select_rx.borrow().len(),
                        shared_state.local_player_ptr.load(Ordering::Acquire),
                        lp_raw,
                        lp_uid,
                        lp_team,
                        shared_state.players.read().len(),
                        shared_state.aim_entities.read().len(),
                    ),
                );

                // Aim diagnostic — the same instrument-before-guessing approach that found the
                // glow chain, pointed at the input path.
                //
                // "no matter which key I bind, nothing happens" has exactly two shapes and this
                // tells them apart in one line. `is_button_down` does not read evdev at all; it
                // bit-tests a 128-bit bitmap copied out of the game's own CInputSystem at
                // `input_system + input_button_state`. So either:
                //
                //   bitmap is always 0000000000000000...  -> we are reading the wrong address,
                //       and no key code can ever work. `InputSystem=0x36dad00` is unverifiable
                //       against the confirmed dump, which contains no InputSystem key at all.
                //   bitmap has bits moving as you click  -> the read is fine and the configured
                //       code is simply the wrong bit; the raw value shows which bit your button
                //       actually sets, and the hotkey can be set to that index directly.
                //
                // `any_down` is the quick tell. `k1/k2/trig` are the configured codes and what
                // `is_button_down` currently answers for each.
                let bs = apex_state.input_system.button_state;
                let (seen, attack_ever, zoom_ever) = BUTTON_UNION.snapshot();
                let k1 = g_settings.hotkey_settings.aimbot_key1;
                let k2 = g_settings.hotkey_settings.aimbot_key2;
                let kt = g_settings.hotkey_settings.triggerbot_key1;
                let probe = crate::apexdream::base::cr3::shadow_probe_snapshot();
                let line = format!(
                    "tick={} ready={} bitmap={:08x}{:08x}{:08x}{:08x} any_down={} \
                     seen={:08x}{:08x}{:08x}{:08x} atk_ever={} zoom_ever={} \
                     k1={}/{} k2={}/{} trig={}/{} in_attack={} in_zoom={} targets={} \
                     evdev={} probe={:?}\n",
                    actions_tick,
                    world_ready,
                    bs[3],
                    bs[2],
                    bs[1],
                    bs[0],
                    apex_state.is_any_button_down(),
                    seen[3],
                    seen[2],
                    seen[1],
                    seen[0],
                    attack_ever,
                    zoom_ever,
                    k1,
                    apex_state.is_button_down(k1),
                    k2,
                    apex_state.is_button_down(k2),
                    kt,
                    apex_state.is_button_down(kt),
                    apex_state.buttons.in_attack.down[0],
                    apex_state.buttons.in_zoom.down[0],
                    aim_select_rx.borrow().len(),
                    G_STATE.lock().unwrap().config.device.use_evdev,
                    probe,
                );
                // Append, don't overwrite.
                //
                // The previous version rewrote this file every 120 ticks, so after a crash it
                // only ever held the post-mortem sample — `ready=false`, an empty entity map, a
                // zeroed bitmap — which says nothing about the in-game state we are trying to
                // capture. A rolling append keeps the last live samples readable after the
                // process is gone, which is the entire reason this instrument exists.
                use std::io::Write as _;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(
                    "/home/sad/Music/apexsky-next/apexsky/dist-memflow-test-v11/log/aim_diag.txt",
                ) {
                    let _ = f.write_all(line.as_bytes());
                }
            }

            // Send key status to aimbot worker
            aim_key_tx
                .send(AimKeyState {
                    aimbot_hotkey_1: if apex_state
                        .is_button_down(g_settings.hotkey_settings.aimbot_key1)
                    {
                        g_settings.hotkey_settings.aimbot_key1
                    } else {
                        0
                    },
                    aimbot_hotkey_2: if apex_state
                        .is_button_down(g_settings.hotkey_settings.aimbot_key2)
                    {
                        g_settings.hotkey_settings.aimbot_key2
                    } else {
                        0
                    },
                    attack_button: apex_state.buttons.in_attack.down[0],
                    zoom_button: apex_state.buttons.in_zoom.down[0],
                    triggerbot_hotkey: if apex_state
                        .is_button_down(g_settings.hotkey_settings.triggerbot_key1)
                    {
                        g_settings.hotkey_settings.triggerbot_key1
                    } else {
                        0
                    },
                    attack_state: match apex_state.buttons.in_attack.state.try_into() {
                        Ok(v) => v,
                        Err(_) => {
                            tracing::error!(?apex_state.buttons.in_attack.state,
                                            "{}", s!("err convert in_attack to i32"));
                            continue;
                        }
                    },
                    quick_looting_hotkey: if apex_state
                        .is_button_down(g_settings.hotkey_settings.quick_looting_key)
                    {
                        g_settings.hotkey_settings.quick_looting_key
                    } else {
                        0
                    },
                })
                .unwrap_or_else(|e| {
                    tracing::error!(%e, ?aim_key_tx, "{}", s!("send key status"));
                });

            // Update entities
            if world_ready {
                let mut players = HashMap::new();
                apex_state.players().for_each(|pl| {
                    // FIXME: skip wrong entity
                    if pl.eadp_uid == 0 || pl.team_num < 0 || pl.team_num > 10000 {
                        return;
                    }
                    let game_player = GamePlayer::new(
                        pl.clone(),
                        apex_state,
                        &mut G_STATE.lock().unwrap().config,
                    );
                    let entity_ptr = pl.entity_ptr.into_raw();
                    let player = Arc::new(game_player);
                    players.insert(entity_ptr, player);
                });

                let local_player_ptr: u64 = Some(apex_state.client.local_player_ptr)
                    .take_if(|ptr| players.contains_key(ptr))
                    .unwrap_or(0);
                let view_player_ptr: u64 = apex_state
                    .local_player()
                    .map(|entity| entity.get_entity_ptr())
                    .unwrap_or(0);

                if actions_tick % 2 != 0 {
                    // Keep the entity map coherent on the fast tick as well. The previous
                    // path updated `players` but left removed/replaced entries in
                    // `aim_entities`, causing stale visibility/context decisions after
                    // streaming transitions.
                    let mut aim_entities = shared_state.aim_entities.write();
                    aim_entities.retain(|ptr, _| players.contains_key(ptr));
                    for (k, v) in &players {
                        aim_entities.insert(*k, v.clone());
                    }
                    *shared_state.players.write() = players;
                } else {
                    let mut npcs: HashMap<u64, Arc<dyn AimEntity>> = HashMap::new();
                    apex_state
                        .entities_as::<BaseNPCEntity>()
                        .for_each(|entity| {
                            npcs.insert(entity.entity_ptr.into_raw(), Arc::new(entity.clone()));
                        });

                    let mut treasure_clues: HashMap<u64, TreasureClue> = HashMap::new();
                    if let Some(local_position) = players
                        .get(&local_player_ptr)
                        .map(|l| arr1(&l.get_entity().origin))
                    {
                        apex_state.entities_as::<LootEntity>().for_each(|entity| {
                            let distance = (arr1(&entity.origin) - &local_position)
                                .mapv(|x| x * x)
                                .sum()
                                .sqrt();
                            let clue = TreasureClue {
                                entity_handle: entity.entity_ptr.into_raw(),
                                item_id: entity.custom_script_int,
                                custom_item_id: (entity.custom_script_int as u64
                                    | ((entity.survival_property as u64) << 32)),
                                position: Some(entity.origin.into()),
                                distance,
                            };
                            treasure_clues.insert(clue.entity_handle, clue);
                        });
                        apex_state
                            .entities_as::<DeathboxEntity>()
                            .for_each(|entity| {
                                let distance = (arr1(&entity.origin) - &local_position)
                                    .mapv(|x| x * x)
                                    .sum()
                                    .sqrt();
                                let clue = TreasureClue {
                                    entity_handle: entity.entity_ptr.into_raw(),
                                    item_id: ItemId::ApexskyItemDeathBox.0,
                                    custom_item_id: 0,
                                    position: Some(entity.origin.into()),
                                    distance,
                                };
                                treasure_clues.insert(clue.entity_handle, clue);
                            });
                    }
                    let loot_count = treasure_clues.len();
                    tracing::trace!(loot_count, "{}", s!("loots updated"));

                    let mut aim_entities: HashMap<u64, Arc<dyn AimEntity>> = HashMap::new();
                    for (k, v) in &players {
                        aim_entities.insert(*k, v.clone());
                    }
                    for (k, v) in &npcs {
                        aim_entities.insert(*k, v.clone());
                    }
                    for (k, v) in &treasure_clues {
                        if v.distance > 40.0 * 3.0 {
                            continue;
                        }
                        aim_entities.insert(*k, Arc::new(QuickLooting(*v)));
                    }
                    let entity_count = aim_entities.len();
                    tracing::trace!(entity_count, "{}", s!("AimEntities updated"));

                    *shared_state.aim_entities.write() = aim_entities;
                    *shared_state.npcs.write() = npcs;
                    *shared_state.players.write() = players;
                    *shared_state.treasure_clues.write() = treasure_clues;
                }

                shared_state
                    .local_player_ptr
                    .store(local_player_ptr, Ordering::Release);
                shared_state
                    .view_player_ptr
                    .store(view_player_ptr, Ordering::Release);
            } else {
                shared_state.local_player_ptr.store(0, Ordering::Release);
                shared_state.view_player_ptr.store(0, Ordering::Release);
                shared_state.aim_entities.write().clear();
                shared_state.npcs.write().clear();
                shared_state.players.write().clear();
                shared_state.treasure_clues.write().clear();
            }

            if let Err(e) = update_time_tx.send(apex_state.time) {
                tracing::error!(%e, ?e, "{}", s!("send update time"));
            }

            // usermod_send_event(UserModEvent::ActionTick(ActionTickData {
            //     input_state: apex_state.input_system.button_state,
            // }));
            usermod_send_event(&event_tick, actions_tick.to_le_bytes().to_vec());

            // Log WeaponId
            if is_newly_connected {
                // The full weapon string table is several hundred entries — one INFO of it
                // scrolls the entire TUI away. Keep the payload at debug; the count is the
                // only part worth seeing at a glance.
                tracing::info!(
                    weapon_names = apex_state.string_tables.weapon_names.len(),
                    "{}",
                    s!("weapon table loaded")
                );
                tracing::debug!(?apex_state.string_tables.weapon_names);
                if apex_state.is_firing_range()
                    && apex_state.string_tables.weapon_names.len() != WEAPON_LIST.len()
                {
                    match (|| -> anyhow::Result<()> {
                        use std::fs;
                        use std::io::Write;

                        let weapons_json =
                            serde_json::to_string(&apex_state.string_tables.weapon_names)?;
                        let path = crate::DATA_DIR.join(s!("updated_weapon.json"));
                        let mut json_file = fs::OpenOptions::new()
                            .create(true)
                            .write(true)
                            .truncate(true)
                            .open(path)?;
                        write!(json_file, "{weapons_json}")?;
                        Ok(())
                    })() {
                        Ok(()) => {
                            tracing::info!("{}", s!("Exported to updated_weapon.json"));
                        }
                        Err(e) => {
                            tracing::warn!(%e, ?e);
                        }
                    }
                }
            }

            // Log Players
            if actions_tick % 40 == 0 && !apex_state.is_firing_range() {
                let players = shared_state.players.read().clone();
                let player_count = players.len();
                if player_count == 0 {
                    tracing::debug!("{}", s!("wait players"));
                } else if player_count > log_players {
                    let mut player_list: Vec<(u32, String)> = players
                        .values()
                        .map(|entity| {
                            (
                                entity.get_entity().index,
                                entity.get_buf().player_name.to_owned(),
                            )
                        })
                        .collect();
                    player_list.sort_by_key(|a| a.0);
                    // Same reasoning as the weapon table: a 60-entry list per update buries
                    // the menu. Count at info, roster at debug.
                    tracing::info!("{player_count}{}", s!(" players sorted"));
                    tracing::debug!(?player_list);
                    log_players = player_count;
                }
            }

            // Log ItemId
            if actions_tick % 40 == 0 && apex_state.is_firing_range() {
                let loot_count = shared_state.treasure_clues.read().len();
                if loot_count == 0 {
                    tracing::debug!("{}", s!("wait items"));
                } else if loot_count > log_items {
                    // create new item list
                    let item_namelist = apex_state
                        .entities_as::<LootEntity>()
                        .map(|entity| LootInt {
                            int: entity.custom_script_int,
                            model: entity.model_name.string.clone(),
                        })
                        .filter(|loot| !(loot.int == 0 && loot.model.is_empty()))
                        .collect::<HashSet<LootInt>>();
                    let mut item_namelist: Vec<LootInt> = item_namelist.into_iter().collect();
                    item_namelist.sort_by_key(|a| a.int);
                    tracing::info!("{loot_count}{}", s!(" items sorted"));
                    log_items = loot_count;

                    match crate::workers::items::export_new_items(item_namelist) {
                        Ok(()) => {
                            tracing::info!("{}", s!("Exported to updated_item.json"));
                        }
                        Err(e) => {
                            let treasure_clues = shared_state.treasure_clues.read().clone();
                            tracing::warn!(%e, ?treasure_clues, "{loot_count}{}", s!(" items sorted"))
                        }
                    }
                }
            }

            /* Hot Variables Update End */

            if actions_tick % 30_000 == 0 {
                actions_tick = 0;
            }
            if actions_tick % 2 != 0 {
                // at least 30ms // don't change xD
                continue;
            }

            /* Cold Variables Update Start */

            // // Debug reading game_state.ClientState
            // if actions_tick % 100 == 0 {
            //     tracing::warn!(?apex_state.client);
            // }

            tracing::trace_span!("Update state in global settings").in_scope(|| {
                let firing_range_mode = apex_state.is_firing_range();
                let tdm = apex_state
                    .gamemode()
                    .is_some_and(|gamemode| [s!("control"), s!("freedm")].contains(&gamemode));

                let mut update_menu = false;
                let g_state = &mut G_STATE.lock().unwrap();

                if g_state.config.settings.firing_range != firing_range_mode {
                    g_state.config.settings.firing_range = firing_range_mode;
                    update_menu = true;
                }
                if g_state.config.settings.team_death_match != tdm {
                    g_state.config.settings.team_death_match = tdm;
                    update_menu = true;
                }
                if g_state.config.settings.calc_game_fps
                    && let Some(fps_update) = game_fps_update
                {
                    g_state.config.settings.game_fps = fps_update;
                }
                if update_menu {
                    crate::global_state::G_TUI_FORCE_UPDATE.store(true, Ordering::Release);
                }
            });

            // Send aim targets
            aim_select_tx
                .send(collect_aim_targets(
                    shared_state.clone(),
                    apex_state,
                    &g_settings,
                    items_glow_rx.clone(),
                ))
                .unwrap_or_else(|e| {
                    tracing::error!(%e, ?aim_select_tx, "{}", s!("send aim targets"));
                });

            if actions_tick % 4 == 0 {
                tracing::trace_span!("Spectator check").in_scope(|| {
                    if player_ready {
                        let Some(lplayer_ptr) = shared_state.get_local_player_ptr() else {
                            return;
                        };
                        let players = shared_state.players.read().clone();
                        let Some(local_player) = players.get(&lplayer_ptr) else {
                            return;
                        };
                        let lplayer_team = local_player.get_buf().team_num;
                        let alter_local_team =
                            shared_state.map_testing_local_team.load(Ordering::Acquire);
                        let tdm_toggle = g_settings.team_death_match;
                        let is_teammate = |team_num| {
                            teammate_check(team_num, lplayer_team, alter_local_team, tdm_toggle)
                        };

                        // Update local entity yew
                        // let yew = mem.apex_mem_read::<f32>(lplayer_ptr + OFFSET_YAW)?;
                        // trace!(lplayer_ptr, ?yew);
                        // apexsky::tick_yew(lplayer_ptr, yew);

                        let mut teammates: Vec<_> = players
                            .values()
                            .filter_map(|target_entity| {
                                let player_buf = target_entity.get_buf();
                                if is_teammate(player_buf.team_num) {
                                    Some(player_buf.to_owned())
                                } else {
                                    None
                                }
                            })
                            .collect();
                        teammates.sort_by_key(|a| a.team_member_index);

                        // Update spectator checker
                        let tmp_specs: Vec<SpectatorInfo> = players
                            .iter()
                            .filter_map(|(&player_ptr, player)| {
                                apex_state
                                    .get_observer_target(player.get_entity().get_info().handle)
                                    .and_then(|observer_target| {
                                        let target_ptr = observer_target.entity_ptr.into_raw();
                                        if player_ptr == lplayer_ptr {
                                            Some(target_ptr)
                                        } else if target_ptr == lplayer_ptr {
                                            Some(player_ptr)
                                        } else {
                                            None
                                        }
                                    })
                                    .and_then(|item_ptr| {
                                        let player_buf = players.get(&item_ptr)?.get_buf();
                                        Some(SpectatorInfo {
                                            ptr: item_ptr,
                                            name: player_buf.player_name.clone(),
                                            is_teammate: is_teammate(player_buf.team_num),
                                            love_status: player_buf
                                                .love_status
                                                .try_into()
                                                .unwrap_or_else(|_| {
                                                    tracing::error!(
                                                        love_state = player_buf.love_status,
                                                        ?player_buf
                                                    );
                                                    LoveStatus::Normal
                                                })
                                                as i32,
                                        })
                                    })
                            })
                            .collect();

                        // Update spectator namelist
                        let (allied_spectators, spectators): (Vec<_>, Vec<_>) =
                            tmp_specs.into_iter().partition(|info| info.is_teammate);

                        *shared_state.spectator_list.lock() = (allied_spectators, spectators);
                        *shared_state.teammates.lock() = teammates;
                    } else {
                        {
                            let mut lock = shared_state.spectator_list.lock();
                            lock.0.clear();
                            lock.1.clear();
                        }
                        //shared_state.teammates.lock().clear();
                    }
                });

                if world_ready {
                    let highlight_injected =
                        shared_state.highlight_injected.load(Ordering::Acquire);

                    // Inject highlight settings
                    let task_inject_highlights = async {
                        let needs_inject = (g_settings.glow_settings.player_glow
                            || g_settings.glow_settings.item_glow
                            || g_settings.glow_settings.weapon_model_glow)
                            && world_ready;
                        if needs_inject {
                            // Resolve the shadow page-table root ONCE for this whole inject pass.
                            //
                            // Why the global highlight table needs this at all: the per-entity
                            // highlight writes in task_player_glow demonstrably land under the
                            // primary DTB — enemies pick up context 79 and far players glow. The
                            // GLOBAL table does not. Every setting that lives only in that table
                            // (color, brightness, outline size, and the context-79 bits that
                            // decide whether an occluded model outlines at all) is inert in a real
                            // match while working in the firing range, which is exactly the
                            // signature of a primary-DTB write landing on a decoy page.
                            //
                            // Err here is normal and not fatal: resolve_secondary_dtb never
                            // blocks, it kicks off background resolution and returns Err until a
                            // root is cached. The primary write still happens either way, so the
                            // firing-range path and any build where the table is genuinely
                            // primary-mapped keep working untouched.
                            let glow_api = crate::apexdream::api::Api {
                                apex_base: apex_base.into(),
                                mem_access: mem.clone(),
                                req_id: REQUEST_ID,
                            };
                            let shadow_dtb = crate::apexdream::base::cr3::resolve_secondary_dtb(
                                &glow_api, apex_base,
                            )
                            .await
                            .ok();
                            match inject_highlight(
                                mem,
                                apex_state.client.framecount,
                                &g_settings,
                                apex_base,
                                &glow_api,
                                shadow_dtb,
                            )
                                .await
                            {
                                Ok(_) => {
                                    if !highlight_injected {
                                        shared_state
                                            .highlight_injected
                                            .store(true, Ordering::Release);
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(%e, ?e, "{}", s!("err inject highlight settings"));
                                }
                            }
                        }
                    }.instrument(info_span!("1"));

                    // Write Player Glow

                    // tracing::debug!(
                    //     playre_glow_on = g_settings.player_glow,
                    //     highlight_injected,
                    //     need_update = aim_select_rx.has_changed().unwrap_or(false),
                    //     write_count = aim_select_rx.borrow().iter().count(),
                    //     "debug player glow"
                    // );
                    let task_player_glow = async {
                        if g_settings.glow_settings.player_glow {
                        // Collect target entity pointers. Reapply every tick so the game
                        // can't drift back to its default yellow after a context reset.
                        //
                        // Built here from the entity list rather than taken from `aim_select_rx`,
                        // because the aim target list is the wrong set for glow and that is the
                        // whole "a few players never glow" bug. `collect_aim_targets` filters on
                        // distance and FOV, which an aimbot needs and a wallhack must not care
                        // about, and `distance` is derived from `get_position()` — the secondary-DTB
                        // shadow read. Measured live, from `target_diag.txt`:
                        //
                        //   accepted=0 rejected_players=60 aim_entities=223 players_map=60
                        //     counts: [("dist_not_normal", 53), ("not_alive", 4), ("teammate", 3)]
                        //   ptr=0x2ab81460 team=11 alive=true dist=0.0 pos=[0.0, 0.0, 0.0]
                        //     -> dist_not_normal
                        //
                        // 53 living players sitting at `[0.0, 0.0, 0.0]` because their shadow origin
                        // read never landed. A player whose origin never resolves has no usable
                        // distance, gets deleted by the distance rule, and therefore never receives
                        // `m_highlightCurrent` — so it renders on the game's own default highlight,
                        // visible-only and distance-limited, for the rest of the match. That is
                        // exactly the reported symptom, and it is why it hits the teams who drop
                        // first: they activate earliest, with the best chance of landing in one of
                        // the windows where the shadow root is dead (measured at over half of all
                        // wall-clock time).
                        //
                        // Glow needs team, alive and not-self. Nothing else. Decoupled from the
                        // shadow read, a player either glows or does not for reasons we control.
                        let target_ptrs: Vec<u64> = {
                            let players = shared_state.players.read();
                            let lplayer_ptr = shared_state.get_local_player_ptr();
                            let local_team = lplayer_ptr
                                .and_then(|ptr| players.get(&ptr))
                                .map(|lplayer| lplayer.get_buf().team_num);
                            match local_team {
                                None => Vec::new(),
                                Some(local_team) => {
                                    let alter_local_team_id =
                                        shared_state.map_testing_local_team.load(Ordering::Acquire);
                                    let tdm = g_settings.team_death_match;
                                    shared_state
                                        .aim_entities
                                        .read()
                                        .values()
                                        .filter(|entity| {
                                            let team = entity.get_team_num();
                                            entity.is_player()
                                                && entity.is_alive()
                                                && (0..=50).contains(&team)
                                                && Some(entity.get_entity_ptr()) != lplayer_ptr
                                                && !teammate_check(
                                                    team,
                                                    local_team,
                                                    alter_local_team_id,
                                                    tdm,
                                                )
                                        })
                                        .map(|entity| entity.get_entity_ptr())
                                        .collect()
                                }
                            }
                        };

                        // The local-player origin probe proves that a candidate DTB resolves the
                        // shadow position pages. It does not prove that the per-entity highlight
                        // block is primary-mapped. Resolve once for this pass so we can test the
                        // entity context through both roots before choosing where to write it.
                        let glow_api = crate::apexdream::api::Api {
                            apex_base: apex_base.into(),
                            mem_access: mem.clone(),
                            req_id: REQUEST_ID,
                        };
                        let glow_shadow_dtb = crate::apexdream::base::cr3::resolve_secondary_dtb(
                            &glow_api,
                            apex_base,
                        )
                        .await
                        .ok()
                        .filter(|&dtb| crate::apexdream::base::cr3::is_probe_confirmed(dtb));

                        // Confirmed offsets from Nika offsets.ini (verified working against v3.0.3.65):
                        //   OFFSET_HIGHLIGHT_ID       = 0x294  [derived: m_highlightGenericContexts(0x2a0) - 1 entry]
                        //   OFFSET_HIGHLIGHT_WALLHACK = 0x25c  [visibility type — write 2 = ALWAYS = through wall]
                        //   OFFSET_HIGHLIGHT_DISTANCE = 0x254  [fade distance — write f32::MAX to disable culling]
                        //
                        // Previous code used entity+0x330 (= m_iHealth per DT_Player dump) and
                        // entity+0x338 (= m_visibilityFlags). Writing CTX_ENEMY=2 to m_iHealth set
                        // enemy health to 2 on every tick — enemies died, glow vanished. Fixed.
                        //
                        // Color: driven entirely by inject_highlight which writes the configured
                        // glow_r_not/glow_g_not/glow_b_not to the global highlight table for
                        // context HIGHLIGHT_PLAYER_RAINBOW (79). That context + wallhack offset
                        // gives correct color + through-wall. No per-entity param writes needed
                        // since the global table color is the source of truth for the renderer.
                        //
                        // All fields are on the PRIMARY PML4. Shadow PML4 maps only origin+bones.
                        // FIELD WIDTHS MATTER HERE. Per the Heashey offset map (UC 716406
                        // post #2252) the highlight block is packed single bytes:
                        //     0x0254 float  m_highlightFarFade
                        //     0x025C u8     m_highlightVisType   (HIGHLIGHT_VIS_*)
                        //     0x0280 SRWLock                     — never touch
                        //     0x0294 u8     m_highlightCurrent   (context id)
                        //     0x0295 u8[8]  m_highlightGenericContexts
                        //     0x029D u8     m_highlightFocused   (bitmask)
                        // Writing u32 to 0x294 spilled over 0x295..0x297 and wiped the first
                        // three generic-context slots; writing i32 to 0x25C spilled into
                        // 0x25D..0x25F. Both are byte fields — write them as bytes.
                        const CTX_PLAYER: u8 = HIGHLIGHT_PLAYER_RAINBOW;

                        // One write per entity, and only this one.
                        //
                        // `0x294` is the sole per-entity offset with live proof behind it: point an
                        // entity at context 79 and it picks up whatever slot 79 holds, which is why
                        // far players glow at all. The three that used to sit alongside it —
                        // `0x25c` (vis type), `0x29d` (focused bitmask), `0x254` (far fade) — had
                        // no such proof. They came from Heashey's reversed map in UC 716406 #2252,
                        // taken from a different build: he had current=0x294, generic=0x295,
                        // focused=0x29D, while BOTH offsets profiles here agree on generic=0x2a0
                        // and focused=0x2a8, so that sub-struct sits 0xB higher on this build and
                        // those three addresses land on fields nobody has identified.
                        //
                        // They are gone because they were never needed. Through-walls is decided by
                        // the HighlightBits in the GLOBAL table slot (the EntityVisible and
                        // AfterPostProcess bits), not per entity — the one build this session that
                        // showed close and far players glowing equally was the one whose shadow
                        // mirror landed those bits, with no `0x25c` write involved. Writing three
                        // unidentified bytes into every target on every tick bought nothing and is
                        // the most likely source of the two-to-three-minute death: 52 targets x 3
                        // writes x every tick into memory we cannot name.

                        // One-shot baseline of the per-entity highlight block, read as discrete
                        // bytes rather than one wide struct.
                        //
                        // The earlier attempt at this read `[u8; 0x70]` in a single call and never
                        // produced a line of output, so nothing was learned; small reads of the
                        // exact fields are the shape that demonstrably works everywhere else in
                        // this file. The question it answers: `0x25c` is Heashey's
                        // `m_highlightVisType` (UC 716406 #2252) and the value we want there is
                        // `HIGHLIGHT_VIS_ALWAYS = 2`. If the byte already holds a small enum value
                        // the offset is real; if it holds something arbitrary, it is not that field
                        // on this build and the write below must come straight back out.
                        if let Some(&probe_ptr) = target_ptrs.first() {
                            static LOGGED_ENTITY_BLOCK: AtomicBool = AtomicBool::new(false);
                            if !LOGGED_ENTITY_BLOCK.swap(true, Ordering::Relaxed) {
                                let mem = mem.clone();
                                tokio::spawn(async move {
                                    // Heashey's offsets and the same offsets shifted by +0xB, side
                                    // by side. The shift is not a hunch: his generic=0x295 and
                                    // focused=0x29D sit exactly 0xB below the profile's 0x2a0 and
                                    // 0x2a8, so if it applies at all it applies to the whole block.
                                    //
                                    // What to look for. A visibility type is a small enum, so the
                                    // one of 0x25c / 0x267 holding 0..3 is the real field. A far
                                    // fade is a sane world distance in the thousands, so the one of
                                    // 0x254 / 0x25f holding that rather than a denormal is the real
                                    // one. And `current` must hold 79 for a target we have written
                                    // — if 0x29f holds 79 while 0x294 only holds it because we put
                                    // it there, then the glow we see is coming from the global slot
                                    // alone and the context write has been landing in the wrong
                                    // place all along.
                                    let far_fade_254 =
                                        mem.read::<f32>(probe_ptr + 0x254, dmalib::PRIO_LOW, 0).await;
                                    let far_fade_25f =
                                        mem.read::<f32>(probe_ptr + 0x25f, dmalib::PRIO_LOW, 0).await;
                                    let vis_25c =
                                        mem.read::<u8>(probe_ptr + 0x25c, dmalib::PRIO_LOW, 0).await;
                                    let vis_267 =
                                        mem.read::<u8>(probe_ptr + 0x267, dmalib::PRIO_LOW, 0).await;
                                    let cur_294 =
                                        mem.read::<u8>(probe_ptr + 0x294, dmalib::PRIO_LOW, 0).await;
                                    let cur_29f =
                                        mem.read::<u8>(probe_ptr + 0x29f, dmalib::PRIO_LOW, 0).await;
                                    let generic_2a0 =
                                        mem.read::<[u8; 8]>(probe_ptr + 0x2a0, dmalib::PRIO_LOW, 0).await;
                                    let focused_2a8 =
                                        mem.read::<u32>(probe_ptr + 0x2a8, dmalib::PRIO_LOW, 0).await;
                                    tracing::info!(
                                        entity = %format!("{probe_ptr:#x}"),
                                        far_fade_0x254 = ?far_fade_254.as_ref().ok(),
                                        far_fade_0x25f = ?far_fade_25f.as_ref().ok(),
                                        vis_0x25c = ?vis_25c.as_ref().ok(),
                                        vis_0x267 = ?vis_267.as_ref().ok(),
                                        current_0x294 = ?cur_294.as_ref().ok(),
                                        current_0x29f = ?cur_29f.as_ref().ok(),
                                        generic_0x2a0 = ?generic_2a0.as_ref().ok(),
                                        focused_0x2a8 = ?focused_2a8.as_ref().ok(),
                                        "{}",
                                        s!("per-entity highlight block baseline")
                                    );
                                });
                            }
                        }

                        let handles: Vec<_> = target_ptrs
                            .into_iter()
                            .map(|target_ptr| {
                                let mem = mem.clone();
                                let glow_api = glow_api.clone();
                                tokio::spawn(async move {
                                    // Highlight context → RAINBOW (79), visibility type → ALWAYS.
                                    //
                                    // `0x25c` is back, and this time by elimination rather than by
                                    // guess. Everything else that could explain "close players glow
                                    // while visible and fade 2-3 s after going behind cover" has
                                    // been measured and ruled out:
                                    //   - the filter is not dropping them: target_diag shows
                                    //     accepted=36 steady with zero dist_* rejections in-match,
                                    //     so every one of them does receive our context;
                                    //   - the global slot bits are right: the renderer's own
                                    //     baseline reads back game_vis=false, game_app=true,
                                    //     identical to what we write;
                                    //   - the shadow root is alive for 15-68 s at a stretch, not
                                    //     2-3 s, so root loss cannot produce a 2-3 s fade.
                                    // A 2-3 second fade-out on loss of visibility is a per-entity
                                    // visibility-type behaviour, and `m_highlightVisType` is the
                                    // only field left that controls it. `HIGHLIGHT_VIS_ALWAYS = 2`
                                    // is what stops the renderer fading a highlight when the entity
                                    // stops being visible.
                                    //
                                    // Read-compare-then-write on both (UC 716406 #1003): in steady
                                    // state this is two byte reads and no writes per target.
                                    let primary_before = mem
                                        .read::<u8>(target_ptr + 0x294, dmalib::PRIO_LOW, 0)
                                        .await
                                        .ok();

                                    // Confirm that the shadow root resolves this entity before
                                    // writing through it. Matching the team field through both
                                    // roots is a stronger test than accepting an arbitrary byte
                                    // at +0x294, and prevents a valid origin DTB from authorizing
                                    // a write to an unrelated shadow-only page.
                                    let primary_team = mem
                                        .read::<i32>(
                                            target_ptr + G_OFFSETS.entity_team_num as u64,
                                            dmalib::PRIO_LOW,
                                            0,
                                        )
                                        .await
                                        .ok();
                                    let shadow_probe = if let Some(shadow_dtb) = glow_shadow_dtb {
                                        let shadow_context =
                                            crate::apexdream::base::cr3::read_at_dtb::<u8>(
                                                &glow_api,
                                                shadow_dtb,
                                                target_ptr + 0x294,
                                            )
                                            .await
                                            .ok();
                                        let shadow_team =
                                            crate::apexdream::base::cr3::read_at_dtb::<i32>(
                                                &glow_api,
                                                shadow_dtb,
                                                target_ptr + G_OFFSETS.entity_team_num as u64,
                                            )
                                            .await
                                            .ok();
                                        Some((shadow_dtb, shadow_context, shadow_team))
                                    } else {
                                        None
                                    };
                                    let shadow_entity_confirmed = shadow_probe.is_some_and(
                                        |(_, context, team)| {
                                            context.is_some_and(|value| value <= 90)
                                                && team.is_some_and(|value| {
                                                    (0..=50).contains(&value)
                                                        && primary_team == Some(value)
                                                })
                                        },
                                    );

                                    let mut primary_write = false;
                                    let mut shadow_write = false;
                                    if shadow_entity_confirmed {
                                        let (shadow_dtb, shadow_context, shadow_team) =
                                            shadow_probe.expect("shadow probe just confirmed");
                                        if shadow_context != Some(CTX_PLAYER) {
                                            match crate::apexdream::base::cr3::write_at_dtb(
                                                &glow_api,
                                                shadow_dtb,
                                                target_ptr + 0x294,
                                                &CTX_PLAYER,
                                            )
                                            .await
                                            {
                                                Ok(()) => shadow_write = true,
                                                Err(e) => tracing::error!(
                                                    ?e,
                                                    target_ptr,
                                                    dtb = %format!("{shadow_dtb:#x}"),
                                                    "shadow entity context write failed"
                                                ),
                                            }
                                        }

                                        // Once the same entity is confirmed under the shadow root,
                                        // do not also touch the primary decoy. The renderer-visible
                                        // write is the one through the confirmed mapping.
                                        let _ = shadow_team;
                                    } else if primary_before != Some(CTX_PLAYER) {
                                        match mem
                                            .write::<u8>(
                                                target_ptr + 0x294,
                                                &CTX_PLAYER,
                                                dmalib::PRIO_LOW,
                                                0,
                                            )
                                            .await
                                        {
                                            Ok(()) => primary_write = true,
                                            Err(e) => tracing::error!(
                                                ?e,
                                                target_ptr,
                                                "{}",
                                                s!("write player glow")
                                            ),
                                        }
                                    }

                                    // Log one target per confirmed DTB. A successful API write can
                                    // still be a write to the wrong virtual mapping; the post-read
                                    // values through both roots make that distinction visible.
                                    if let Some((shadow_dtb, shadow_context, shadow_team)) =
                                        shadow_probe
                                    {
                                        static LAST_GLOW_DUAL_DIAG_DTB: AtomicU64 =
                                            AtomicU64::new(0);
                                        let previous = LAST_GLOW_DUAL_DIAG_DTB
                                            .swap(shadow_dtb, Ordering::Relaxed);
                                        if previous != shadow_dtb {
                                            let primary_after = mem
                                                .read::<u8>(
                                                    target_ptr + 0x294,
                                                    dmalib::PRIO_LOW,
                                                    0,
                                                )
                                                .await
                                                .ok();
                                            let shadow_after =
                                                crate::apexdream::base::cr3::read_at_dtb::<u8>(
                                                    &glow_api,
                                                    shadow_dtb,
                                                    target_ptr + 0x294,
                                                )
                                                .await
                                                .ok();
                                            tracing::info!(
                                                target = %format!("{target_ptr:#x}"),
                                                dtb = %format!("{shadow_dtb:#x}"),
                                                primary_before,
                                                primary_team,
                                                shadow_before = shadow_context,
                                                shadow_team,
                                                shadow_entity_confirmed,
                                                primary_write,
                                                shadow_write,
                                                primary_after,
                                                shadow_after,
                                                "glow entity context dual-root diagnostic"
                                            );
                                        }
                                    }
                                    // The `0x25c` write is gone again, and this time it is measured
                                    // rather than argued. The baseline read came back:
                                    //
                                    //   far_fade_0x254: 2.571419e-39   vis_type_0x25c: 93
                                    //   current_0x294: 79             focused_0x29d: 38
                                    //   generic_0x2a0: 8
                                    //
                                    // `0x25c` holding 93 is not a visibility-type enum — that field
                                    // takes 0..3 — and `0x254` holding a denormal float is not a
                                    // fade distance. Both of those are unidentified entity memory
                                    // on this build, so writing 2 into one of them was exactly the
                                    // class of scribble that has been killing the process.
                                    //
                                    // Heashey's map is off by +0xB here, and the profile proves it
                                    // twice over: his generic=0x295 and focused=0x29D versus the
                                    // profile's 0x2a0 and 0x2a8 — the same +0xB on both. Applying
                                    // it gives farFade=0x25f, visType=0x267, current=0x29f. The
                                    // baseline below now reads those candidates so the next run
                                    // names the real field instead of inferring it.
                                })
                            })
                            .collect();

                        for result in futures_util::future::join_all(handles).await {
                            if let Err(e) = result {
                                tracing::error!(?e, "{}", s!("player glow write task panicked"));
                            }
                        }
                    }
                    }
                    .instrument(info_span!("2"));

                    // Write Items Glow
                    let task_items_glow =
                        async {
                            if g_settings.glow_settings.item_glow
                        && highlight_injected
                        && items_glow_rx.has_changed().unwrap_or_else(|e| {
                            tracing::error!(%e, ?items_glow_rx, "{}", s!("perform items glow"));
                            false
                        })
                    {
                        let writes = items_glow_rx.borrow_and_update().clone();
                        for (ptr, ctx_id) in writes {
                            let mem = mem.clone();
                            tokio::spawn(async move {
                                if let Err(e) = mem
                                    .write::<u32>(
                                        ptr + 0x330u64,
                                        &(ctx_id as u32),
                                        dmalib::PRIO_LOW,
                                        0,
                                    )
                                    .await
                                {
                                    tracing::error!(?e, ptr, ctx_id, "{}", s!("write loot glow"))
                                }
                            });
                        }
                    }
                        }
                        .instrument(info_span!("3"));

                    // Weapon model glow
                    let task_vmodel_glow = async {
                        if !(g_settings.glow_settings.weapon_model_glow && highlight_injected) {
                            return;
                        }
                        let Some(local_player) = shared_state
                            .get_local_player_ptr()
                            .and_then(|lplayer_ptr| shared_state.read_cached_player(&lplayer_ptr))
                            .filter(|pl| pl.is_alive())
                        else {
                            return;
                        };
                        let Ok(view_model_handle) = mem
                            .read::<u64>(
                                local_player.get_entity_ptr() + G_OFFSETS.cplayer_viewmodels,
                                dmalib::PRIO_LOW,
                                0,
                            )
                            .await
                            .inspect_err(|e| {
                                tracing::warn!(?e, "{}", s!("read player view_model"))
                            })
                        else {
                            return;
                        };

                        let handle = (view_model_handle & 0xffff) as u32;
                        let view_model_ptr = apex_state.entity_ptr(handle);
                        let Some(ptr) = view_model_ptr else {
                            return;
                        };
                        let (al_spec, spec) = {
                            let spec_list = shared_state.spectator_list.lock();
                            (spec_list.0.len(), spec_list.1.len())
                        };
                        let ctx_id = if spec > 0 {
                            HIGHLIGHT_DEATH_BOX
                        } else if al_spec > 0 {
                            HIGHLIGHT_LOOT_ENERGY
                        } else {
                            HIGHLIGHT_WEAPON_RAINBOW
                        };
                        if let Err(e) = mem
                            .write::<u32>(
                                ptr.into_raw() + 0x330u64,
                                &(ctx_id as u32),
                                dmalib::PRIO_LOW,
                                0,
                            )
                            .await
                        {
                            tracing::error!(?e, ?ptr, ctx_id, "{}", s!("write weapon model glow"))
                        }
                    }
                    .instrument(tracing::info_span!("4"));

                    tokio::join!(
                        task_inject_highlights,
                        task_player_glow,
                        task_items_glow,
                        task_vmodel_glow,
                    );

                    if actions_tick % 160 == 0 {
                        tracing::trace!(?apex_update_ctx.intresting);
                    }

                    // println!("in_use state {}", apex_state.buttons.in_use.state);
                    // if verbose {
                    //     tracing::warn!(ent_dir = ?apex_state.entity_list.gce.directory);
                    // }
                    // {"al_shield_anchor_l": "CScriptProp",
                    // "-1": "CInfoTarget",
                    // "shield_throw": "CScriptMover",
                    // "vortex_trigger_area": "CVortexSphere",
                    // "rebuilt_door": "CScriptProp",
                    // "caustic_trap": "CScriptProp",
                    // "player": "CPlayer",
                    // "dynamic_dummie": "CAI_BaseNPC",
                    // "crypto_camera": "CScriptProp",
                    // "al_shield_low_r": "CScriptProp",
                    // "al_shield_low_l": "CScriptProp",
                    // "al_shield_anchor_c": "CScriptProp",
                    // "axiom_impact_zone_threat": "CScriptProp",
                    // "drone_no_minimap_object": "CPlayerVehicle",
                    // "al_shield_anchor_r": "CScriptProp",
                    // "": "CScriptNetDataGlobalNonRewind"}
                }
            }
        }
    }
    tracing::debug!("{}", s!("task end"));
    Ok(())
}

#[tracing::instrument(skip_all)]
fn collect_aim_targets(
    state: SharedStateType,
    apex_state: &GameState,
    g_settings: &Settings,
    items_glow_rx: watch::Receiver<Vec<(u64, u8)>>,
) -> Vec<AimTargetInfo> {
    if !state.is_world_ready() {
        return vec![];
    }

    let quick_looting = apex_state.is_button_down(g_settings.hotkey_settings.quick_looting_key);

    let aim_entities = state.aim_entities.read().clone();
    let players = state.players.read().clone();
    let treasure_clues = if quick_looting {
        Some(state.treasure_clues.read().clone())
    } else {
        None
    };

    let Some(lplayer) = state
        .get_local_player_ptr()
        .and_then(|ptr| players.get(&ptr))
    else {
        return vec![];
    };

    let alter_local_team_id = state.map_testing_local_team.load(Ordering::Acquire);

    // Iterate over all targetable entities
    let mut rejected: Vec<(u64, i32, &'static str, f32, bool, [f32; 3])> = Vec::new();
    let mut aim_targets: Vec<AimTargetInfo> = aim_entities
        .values()
        .filter_map(|entity| {
            let entity = entity.as_ref();
            match process_player(lplayer, entity, &players, alter_local_team_id, g_settings) {
                Ok(target) => Some(target),
                Err(reason) => {
                    if entity.is_player() {
                        let position = entity.get_position();
                        rejected.push((
                            entity.get_entity_ptr(),
                            entity.get_team_num(),
                            reason,
                            math::dist(position, lplayer.get_entity().get_position()),
                            entity.is_alive(),
                            position,
                        ));
                    }
                    None
                }
            }
        })
        .collect();

    // Periodic dump of every player entity that did NOT make it into the write set.
    //
    // `task_player_glow` writes the highlight context only to `aim_select_rx`, which is this
    // vector. So this file answers the open question directly: if six players are rendering on the
    // game's own default highlight instead of ours, they are in here, and the reason string names
    // the rule that removed them. Teammates, self and dead players are summarised in the counts but
    // not listed individually — they are expected rejections and would bury the interesting ones.
    {
        static DUMP_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if DUMP_CALLS.fetch_add(1, Ordering::Relaxed) % 150 == 0 {
            use std::io::Write as _;
            let mut counts: HashMap<&'static str, usize> = HashMap::new();
            for (_, _, reason, _, _, _) in &rejected {
                *counts.entry(*reason).or_default() += 1;
            }
            let mut counts: Vec<(&'static str, usize)> = counts.into_iter().collect();
            counts.sort_unstable();
            let mut text = format!(
                "accepted={} rejected_players={} aim_entities={} players_map={} local_team={} \
                 max_dist={}\n  counts: {counts:?}\n",
                aim_targets.len(),
                rejected.len(),
                aim_entities.len(),
                players.len(),
                lplayer.get_buf().team_num,
                g_settings.max_dist,
            );
            for (ptr, team, reason, distance, alive, position) in &rejected {
                if matches!(*reason, "teammate" | "self" | "not_alive") {
                    continue;
                }
                text.push_str(&format!(
                    "  ptr={ptr:#x} team={team} alive={alive} dist={distance:.1} \
                     pos=[{:.1}, {:.1}, {:.1}] -> {reason}\n",
                    position[0], position[1], position[2],
                ));
            }
            text.push('\n');
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(
                "/home/sad/Music/apexsky-next/apexsky/dist-memflow-test-v11/log/target_diag.txt",
            ) {
                let _ = file.write_all(text.as_bytes());
            }
        }
    }

    if let Some(loots) = treasure_clues {
        aim_targets.extend(items_glow_rx.borrow().iter().filter_map(|(ptr, _)| {
            let clue = loots.get(ptr)?;
            let distance = clue.distance;
            (distance < 40.0 * 2.5).then_some({
                let target_entity = QuickLooting(*clue);
                let fov = calculate_target_fov(lplayer.as_ref(), &target_entity);
                AimTargetInfo {
                    fov,
                    distance,
                    is_visible: target_entity.is_visible(),
                    is_knocked: target_entity.is_knocked(),
                    health_points: 1,
                    love_status: LoveStatus::Normal as i32,
                    is_kill_leader: false,
                    entity_ptr: target_entity.get_entity_ptr(),
                    is_npc: false,
                    is_loot: target_entity.is_loot(),
                    is_crosshair_target: target_entity.is_crosshair_target(),
                }
            })
        }));
    }

    aim_targets.sort_by(|a, b| {
        a.distance.partial_cmp(&b.distance).unwrap_or_else(|| {
            tracing::error!(?a, ?b, "{}", s!("sort"));
            panic!()
        })
    });
    aim_targets
}

#[tracing::instrument(skip_all)]
/// `Err` carries the name of the rule that rejected the entity.
///
/// This used to return `Option`, which made every rejection look identical from the outside. The
/// glow write path only ever writes `m_highlightCurrent` to entities that survive this function,
/// so an entity silently dropped here renders on the game's own default highlight — visible-only
/// and distance-limited — while every accepted entity glows through walls on our context. Six
/// players out of fifty behaving exactly that way is this function rejecting six entities, and
/// without a reason string there is no way to tell which of the eight rules did it.
fn process_player(
    local_player: &GamePlayer,
    target_entity: &dyn AimEntity,
    players: &HashMap<u64, Arc<GamePlayer>>,
    alter_local_team_id: i32,
    g_settings: &Settings,
) -> Result<AimTargetInfo, &'static str> {
    if target_entity.is_loot() {
        return Err("is_loot");
    }

    let lplayer_ptr = local_player.get_entity().get_entity_ptr();
    let target_ptr = target_entity.get_entity_ptr();

    let entity_team = target_entity.get_team_num();
    let local_team = local_player.get_buf().team_num;

    let is_teammate = teammate_check(
        entity_team,
        local_team,
        alter_local_team_id,
        g_settings.team_death_match,
    );
    // trace!(target_ptr, entity_team, is_teammate);

    // Exclude eliminated players
    if !target_entity.is_alive() {
        return Err("not_alive");
    }

    // Teammate and 1v1 check
    if !g_settings.feature_settings.onevone {
        match (
            g_settings.firing_range,
            target_entity.is_player(),
            is_teammate,
        ) {
            (true, true, _) => return Err("firing_range_player"),
            (false, _, true) => return Err("teammate"),
            _ => (),
        }
    }

    // Exclude self
    if target_ptr == lplayer_ptr {
        return Err("self");
    }

    // Exclude players in invalid team
    if target_entity.is_player() && !(0..=50).contains(&entity_team) {
        tracing::warn!(?entity_team, ?target_entity, "{}", s!("invalid team"));
        return Err("invalid_team");
    }

    // Calc distance
    // Excluding targets that are too far or too close
    let distance = {
        let target_pos = target_entity.get_position();
        let local_pos = local_player.get_entity().get_position();
        math::dist(target_pos, local_pos)
    };

    // Split into three so the diagnostic can tell them apart.
    //
    // `distance` is derived from `target_entity.get_position()`, which for a remote player comes
    // from the secondary-DTB shadow read in `PlayerEntity::update`. When that read returns a page
    // the shadow root has not migrated yet — UC 716406 #2455/#2457, "bone matrix garbage until the
    // next rotation" on spawn — `validate_vec3` rejects the value and the entity keeps whatever
    // origin it last had, which for a player that was never successfully read is the default. A
    // bogus origin produces a bogus distance, and a bogus distance silently deletes that player
    // from the glow write set here. "not_normal" vs "too_far" vs "too_close" says which.
    if !distance.is_normal() {
        return Err("dist_not_normal");
    }
    if distance > g_settings.max_dist {
        return Err("dist_too_far");
    }
    if distance < 10.0 {
        return Err("dist_too_close");
    }

    // Calc FOV
    let fov = calculate_target_fov(local_player.get_entity(), target_entity);
    if !fov.is_finite() {
        // inf/neg_inf/nan
        return Err("fov_not_finite");
    }

    let target_player = target_entity.is_player().then(|| {
        let Some(p) = players.get(&target_ptr) else {
            tracing::error!(?target_ptr, "{}", s!("UNREACHABLE"));
            panic!();
        };
        p
    });

    Ok(AimTargetInfo {
        fov,
        distance,
        is_visible: target_entity.is_visible(),
        is_knocked: target_entity.is_knocked(),
        health_points: { target_entity.get_shield_health() + target_entity.get_health() },
        love_status: {
            if let Some(target_player) = target_player {
                target_player.get_buf().love_status.try_into()
                .unwrap_or_else(|_| {
                    tracing::error!(love_state = target_player.get_buf().love_status, player_buf = ?target_player.get_buf());
                    LoveStatus::Normal
                })
            } else {
                // not player (dummy)
                if g_settings.yuan_p {
                    LoveStatus::Love
                } else {
                    LoveStatus::Normal
                }
            }
        } as i32,
        is_kill_leader: {
            if let Some(target_player) = target_player {
                GamePlayer::is_kill_leader(target_player.get_buf())
            } else {
                false
            }
        },
        entity_ptr: target_ptr,
        is_npc: target_player.is_none(),
        is_loot: false,
        is_crosshair_target: target_entity.is_crosshair_target(),
    })
}

#[tracing::instrument(skip_all)]
fn player_glow(
    target: &AimTargetInfo,
    frame_count: i32,
    game_fps: f32,
    player_glow_armor_color: bool,
    player_glow_love_user: bool,
    player_filled_toggle: bool,
    player_glow_rainbow: bool,
) -> u8 {
    let game_fps = if game_fps.is_normal() {
        i32::max(1, game_fps as i32)
    } else {
        60
    };
    let mut setting_index = {
        if target.is_knocked {
            HIGHLIGHT_PLAYER_KNOCKED
        } else if player_glow_rainbow {
            // Routes every live player to the RAINBOW context (79). NOTE: inject_highlight is
            // one-shot, so this context's color is written ONCE and stays static — it does NOT
            // cycle. Driving a real cycle needs the highlight table rewritten over time, which the
            // one-shot design deliberately avoids (flicker; and under an active shadow-write path it
            // crashed the game). Left here but gated off by default via `player_glow_rainbow`.
            HIGHLIGHT_PLAYER_RAINBOW
        } else if player_filled_toggle {
            // Fill highlights only shade fragments the game actually rasterizes. A visible
            // target is drawn, so the inside-fill pass renders. An occluded target — above
            // all a CLOSE one fully behind an occluder — gets occlusion-culled, so the fill
            // pass has no fragments and the glow vanishes entirely (matching "close occluded
            // players don't show, only far away ones do": far targets stay rasterized at LOD
            // / in-frustum and still fill). Keep the fill context only while the target is
            // visible; route occluded targets to the outline-only twin, which draws through
            // walls regardless of culling — the same fix the non-filled branch below uses.
            if target.is_visible {
                HIGHLIGHT_PLAYER_NOTVIZ
            } else {
                HIGHLIGHT_PLAYER_NOTVIZ_OUTLINE
            }
        } else if player_glow_armor_color {
            match target.health_points {
                0..=100 => HIGHLIGHT_PLAYER_ORANGE,
                101..=150 => HIGHLIGHT_PLAYER_WHITE,
                151..=175 => HIGHLIGHT_PLAYER_BLUE,
                176..=200 => HIGHLIGHT_PLAYER_PURPLE,
                201..=225 => HIGHLIGHT_PLAYER_RED,
                _ => HIGHLIGHT_PLAYER_BLACK,
            }
        } else {
            // All live players (visible AND occluded) share the one context that draws through
            // walls on v3.0.3.65 (id 79). EMPIRICAL (user A/B): occlusion renders only when every
            // player is on this context — splitting visible onto a separate context (71) re-broke
            // occluded rendering even though the HighlightBits are byte-identical. Single color by
            // design (inject colors 79 bright pink); no per-visibility distinction here.
            HIGHLIGHT_PLAYER_RAINBOW
        }
    };

    // love player glow
    if player_glow_love_user {
        let frame_frag = frame_count / game_fps;
        if frame_frag % 2 == 0 {
            match target.love_status.try_into().unwrap_or(LoveStatus::Normal) {
                LoveStatus::Love => {
                    setting_index = HIGHLIGHT_PLAYER_RAINBOW;
                }
                LoveStatus::Hate => {
                    setting_index = HIGHLIGHT_PLAYER_BLACK;
                }
                _ => (),
            }
        }
    }

    // kill leader glow
    if target.is_kill_leader {
        let frame_frag = frame_count / game_fps;
        if frame_frag % 3 == 0 {
            setting_index = HIGHLIGHT_PLAYER_ORANGE;
        }
    }

    setting_index
}

/// The two fields `inject_highlight` drives in a global highlight-table slot, laid out exactly as
/// they sit in the guest so the shadow mirror can land both in one DTB-scoped write.
///
/// `HighlightBits` is `#[repr(C)]` over four `u8`s (4 bytes) and `[f32; 4]` is 4-aligned, so this
/// is 20 contiguous bytes with no padding — byte-identical to the primary path's
/// `write::<HighlightBits>(slot)` + `write::<[f32; 4]>(slot + 0x04)` pair. Coalescing matters here
/// because every shadow write takes memflow's process lock and does set_dtb → write → set_dtb;
/// one write per slot instead of two halves the lock traffic for the whole 22-slot pass.
#[derive(Copy, Clone, zerocopy::IntoBytes, zerocopy::Immutable)]
#[repr(C)]
struct HighlightSlotHead {
    bits: HighlightBits,
    color_rgba: [f32; 4],
}

/// Re-drive the shadow copy of the highlight table every Nth `inject_highlight` call.
///
/// The table is static between settings changes, so the mirror is normally skipped and only fires
/// on an actual content change (see `MIRRORED_FINGERPRINT`). This counter is the recovery path for
/// the other case: an anti-cheat that periodically rewrites table slots it does not recognise.
/// Without it a single overwrite would stick until the user next touched a glow setting.
const SHADOW_MIRROR_EVERY: u64 = 16;

/// Contexts the shadow mirror is allowed to write.
///
/// `task_player_glow` assigns `HIGHLIGHT_PLAYER_RAINBOW` (79) to every target unconditionally and
/// `HIGHLIGHT_PLAYER_KNOCKED` (70) to downed ones, so these are the only two slots the renderer
/// can be looking at on our behalf. Every other context in the inject table is reachable only
/// through `item_glow` or `weapon_model_glow`; the primary pass still writes them all, which is
/// free and has never been implicated, but the shadow mirror writes guest memory through a
/// page-table root we discovered by scanning, and that is not a cost to pay for slots nothing
/// reads.

/// Running bitwise-OR of every button-state word observed, plus whether the game's own attack and
/// zoom commands were ever asserted.
///
/// The diagnostic samples once per 120 ticks — roughly every two seconds — which is far too
/// sparse to catch a mouse click. 147 samples across a full match showed `bs[3]` (the word holding
/// mouse codes 107-112) as zero every single time and `in_attack` as 0 every single time, which
/// cannot distinguish "mouse buttons never appear in this bitmap" from "every click happened
/// between two samples". Accumulating on every tick removes the sampling question entirely: if a
/// mouse bit is ever set during a run, it lands here.
struct ButtonUnion {
    words: [std::sync::atomic::AtomicU32; 4],
    attack_ever: AtomicBool,
    zoom_ever: AtomicBool,
}

impl ButtonUnion {
    const fn new() -> Self {
        Self {
            words: [
                std::sync::atomic::AtomicU32::new(0),
                std::sync::atomic::AtomicU32::new(0),
                std::sync::atomic::AtomicU32::new(0),
                std::sync::atomic::AtomicU32::new(0),
            ],
            attack_ever: AtomicBool::new(false),
            zoom_ever: AtomicBool::new(false),
        }
    }

    fn observe(&self, state: &[u32; 4], attack: bool, zoom: bool) {
        for (slot, word) in self.words.iter().zip(state.iter()) {
            slot.fetch_or(*word, Ordering::Relaxed);
        }
        if attack {
            self.attack_ever.store(true, Ordering::Relaxed);
        }
        if zoom {
            self.zoom_ever.store(true, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> ([u32; 4], bool, bool) {
        (
            [
                self.words[0].load(Ordering::Relaxed),
                self.words[1].load(Ordering::Relaxed),
                self.words[2].load(Ordering::Relaxed),
                self.words[3].load(Ordering::Relaxed),
            ],
            self.attack_ever.load(Ordering::Relaxed),
            self.zoom_ever.load(Ordering::Relaxed),
        )
    }
}

static BUTTON_UNION: ButtonUnion = ButtonUnion::new();

/// Whether `addr` can be a live user-mode pointer in the game's address space.
///
/// Windows reserves the lowest 64 KiB of every process as the NULL-dereference guard region, so
/// it is never mapped and never a valid target; the user-mode half of a canonical x64 address
/// space ends at `0x0000_7FFF_FFFF_FFFF`. Anything outside that range is a failed or torn read,
/// not an address, and writing through it is how a pointer that momentarily reads as 0 turns a
/// cosmetic feature into an access violation inside the game.
///
/// This is deliberately a *range* check and not a null check. A torn 8-byte read during an engine
/// transition can surface a half-updated pointer — a plausible-looking low word with a garbage
/// high word — which is not zero and is still not somewhere to write 20 bytes.
fn is_plausible_user_va(addr: u64) -> bool {
    (0x1_0000..0x8000_0000_0000).contains(&addr)
}

#[tracing::instrument(skip_all)]
async fn inject_highlight(
    mem: &crate::MemAccess,
    frame_count: i32,
    g_settings: &Settings,
    module_base: u64,
    api: &crate::apexdream::api::Api,
    shadow_dtb: Option<u64>,
) -> anyhow::Result<()> {
    let bits_loot = HighlightBits::new(
        g_settings.glow_settings.loot_filled,
        125,
        64,
        7,
        true,
        false,
    );
    let bits_outline = HighlightBits::new(0, 125, 64, 7, true, false);
    // The two trailing flags are `is_entity_visible` and `is_after_post_process`, and they land in
    // bit 6 and bit 7 of the fourth byte: (AfterPostProcess << 7) | (EntityVisible << 6) | state.
    //
    // They were `(true, false)` for the player contexts, which asks the renderer for "apply this
    // highlight while the entity is visible, drawn in the depth-tested pass". That is literally the
    // reported symptom — glow renders while visible and not while occluded — and it explains both
    // halves of this bug at once, with no per-entity offset involved:
    //
    //   shadow mirror inactive -> our bits never reach the renderer's copy of the table, the game's
    //     own defaults stay in slot 79, so occluded players glow but our colour / brightness /
    //     outline size do nothing. That is the original report, and it is why spectating looks
    //     correct: the probe goes stale when the local player dies, the sweep refuses to run
    //     ("refusing secondary CR3 sweep: no fresh shadow-origin probe"), and the mirror stops.
    //   shadow mirror active -> our bits do reach it, colour and outline start working, and
    //     through-walls breaks for exactly the players who are behind geometry — the close ones.
    //
    // So the mirror was never the bug; it was the delivery mechanism that finally made this
    // payload visible. Clearing EntityVisible stops gating on visibility, and setting
    // AfterPostProcess moves the draw out of the depth-tested pass so geometry cannot cull it.
    // Both set. Byte[3] = 0xC0 | state, which is what the game itself uses for every context that
    // draws through geometry.
    //
    // Settled by dumping all 91 shipped context slots (`log/ctx_defaults.txt`). Two facts come
    // straight off that table and neither is arguable:
    //
    //   1. EntityVisible is set on EVERY slot the game ships. All 91. The only slots in the whole
    //      table with it clear were 70-80 — the ones we had overwritten ourselves. So bit 6 is not
    //      "draw only while visible"; it is a required enable, and clearing it is what stopped
    //      occluded players rendering. `HighlightBits::SONAR` in this repo already said so:
    //      `new(12, 169, 32, 7, true, false)`, Bloodhound's through-walls scan, EntityVisible set.
    //
    //   2. AfterPostProcess is what separates depth-tested from draw-over-geometry. Seven slots
    //      carry it, and they are exactly the ones with byte[3] = 0xC0 — both bits together:
    //
    //        15  0xc09f6500    41  0xc020810c    43  0xc020a992    48  0xc0407d7e
    //        49  0xc0200014    51  0xc040670f    62  0xc0200070
    //
    //      while the game's own player-colour contexts (31-40, 81) are 0x47407d00 — EntityVisible
    //      set, AfterPostProcess clear, state 7. Depth-tested, so occluded models get culled.
    //
    // The history of this constant, so nobody repeats it: (true, false) was the original, which is
    // the game's player-context behaviour and therefore culls through walls. I then tried
    // (false, true), reasoning that EntityVisible gated on visibility — that cleared a flag no
    // shipped slot clears and made it worse. (true, true) is the pairing the renderer itself uses
    // for through-geometry effects.
    // RESULT OF THE EXPERIMENT, recorded so it is never repeated.
    //
    // Four byte[3] values were tried live, and `AfterPostProcess` is fatal in every one:
    //
    //   0x47  vis=1 app=0 state=7   glow on all 56 enemies, occluded ones fade after 2-3 s
    //   0x87  vis=0 app=1 state=7   visible players only
    //   0xC7  vis=1 app=1 state=7   almost nothing
    //   0xC0  vis=1 app=1 state=0   nothing at all, for anyone
    //
    // The 0xC0 attempt was the strongest case on paper: it is byte-for-byte the flag pattern on
    // all nine shipped through-geometry slots, and the write was verified landing
    // (`game_bits: 0xc0787d00`) with all 56 enemies in the write set. It still rendered nothing.
    //
    // So setting AfterPostProcess on a slot is not sufficient to make that slot draw over
    // geometry. Those nine slots differ from ours in their inside/outline FUNCTION ids too, and a
    // function id is an index into the renderer's shader table — `0xC0407D7E` has inside=126,
    // ours has inside=0. Clearing the inside pass while asking for an after-post-process draw
    // plausibly leaves nothing to composite. Whatever the mechanism, the bit alone does not buy
    // through-walls, and guessing at function ids from here would be a fifth blind attempt.
    //
    // Back to (true, false): the only configuration that has ever put glow on every enemy with our
    // colour and outline. The occluded fade stays open, and the next thing to try is not another
    // bit — it is pointing entities at one of those nine shipped contexts and writing only its
    // colour, leaving the game's own working bits untouched.
    const PLAYER_ENTITY_VISIBLE: bool = true;
    const PLAYER_AFTER_POST_PROCESS: bool = false;
    let bits_player_fill = HighlightBits::new(
        g_settings.glow_settings.player_glow_inside_value,
        g_settings.glow_settings.player_glow_outline_value,
        g_settings.glow_settings.player_glow_outline_size,
        g_settings.glow_settings.player_glow_inside_opacity,
        PLAYER_ENTITY_VISIBLE,
        PLAYER_AFTER_POST_PROCESS,
    );
    let bits_player_outline = HighlightBits::new(
        0,
        g_settings.glow_settings.player_glow_outline_value,
        g_settings.glow_settings.player_glow_outline_size,
        g_settings.glow_settings.player_glow_inside_opacity,
        PLAYER_ENTITY_VISIBLE,
        PLAYER_AFTER_POST_PROCESS,
    );
    let bits_nobody_outline = HighlightBits::new(0, 125, 64, 0, false, false);
    let rainbow_col = rainbow_color(frame_count);

    let highlight_settings_inject: [(u8, &HighlightBits, [f32; 3]); 22] = [
        (HIGHLIGHT_LOOT_HEAVY, &bits_loot, [0.0, 1.0, 1.0]),
        (HIGHLIGHT_LOOT_LIGHT, &bits_loot, [1.0, 0.5490, 0.0]),
        (HIGHLIGHT_LOOT_RED, &bits_loot, [1.0, 0.0, 0.0]),
        (HIGHLIGHT_LOOT_BLUE, &bits_loot, [0.0, 0.7490, 1.0]),
        (HIGHLIGHT_LOOT_GREY, &bits_loot, [0.6, 0.6, 0.6]),
        (HIGHLIGHT_LOOT_WHITE, &bits_loot, [1.0, 1.0, 1.0]),
        (HIGHLIGHT_LOOT_ENERGY, &bits_loot, [0.2, 1.0, 0.0]),
        (HIGHLIGHT_LOOT_PURPLE, &bits_loot, [0.2941, 0.0, 0.5098]),
        (HIGHLIGHT_LOOT_GOLD, &bits_loot, [1.0, 0.8431, 0.0]),
        (HIGHLIGHT_DEATH_BOX, &bits_outline, [1.0, 0.0, 0.0]),
        (
            HIGHLIGHT_WEAPON_RAINBOW,
            if g_settings.glow_settings.weapon_model_transparent {
                &bits_nobody_outline
            } else {
                &bits_outline
            },
            rainbow_col,
        ),
        (
            HIGHLIGHT_PLAYER_KNOCKED,
            &bits_player_outline,
            [
                g_settings.color_settings.glow_r_knocked
                    * g_settings.glow_settings.player_brightness as f32,
                g_settings.color_settings.glow_g_knocked
                    * g_settings.glow_settings.player_brightness as f32,
                g_settings.color_settings.glow_b_knocked
                    * g_settings.glow_settings.player_brightness as f32,
            ],
        ),
        (
            HIGHLIGHT_PLAYER_VISIBLE,
            &bits_player_outline,
            [
                g_settings.color_settings.glow_r_viz
                    * g_settings.glow_settings.player_brightness as f32,
                g_settings.color_settings.glow_g_viz
                    * g_settings.glow_settings.player_brightness as f32,
                g_settings.color_settings.glow_b_viz
                    * g_settings.glow_settings.player_brightness as f32,
            ],
        ),
        (
            HIGHLIGHT_PLAYER_NOTVIZ,
            &bits_player_fill,
            [
                g_settings.color_settings.glow_r_not
                    * g_settings.glow_settings.player_brightness as f32,
                g_settings.color_settings.glow_g_not
                    * g_settings.glow_settings.player_brightness as f32,
                g_settings.color_settings.glow_b_not
                    * g_settings.glow_settings.player_brightness as f32,
            ],
        ),
        (
            // Same color as HIGHLIGHT_PLAYER_NOTVIZ, outline-only bits instead of fill. This is
            // the context the default (non-fill-toggle) not-visible case actually uses now — see
            // player_glow() — because a fill highlight can't render on a culled/occluded model.
            HIGHLIGHT_PLAYER_NOTVIZ_OUTLINE,
            &bits_player_outline,
            [
                g_settings.color_settings.glow_r_not
                    * g_settings.glow_settings.player_brightness as f32,
                g_settings.color_settings.glow_g_not
                    * g_settings.glow_settings.player_brightness as f32,
                g_settings.color_settings.glow_b_not
                    * g_settings.glow_settings.player_brightness as f32,
            ],
        ),
        (
            HIGHLIGHT_PLAYER_BLACK,
            &bits_player_fill,
            [
                2.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
                2.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
                2.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
            ],
        ),
        (
            HIGHLIGHT_PLAYER_ORANGE,
            &bits_player_fill,
            [
                255.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
                165.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
                0.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
            ],
        ),
        (
            HIGHLIGHT_PLAYER_WHITE,
            &bits_player_fill,
            [
                247.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
                247.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
                247.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
            ],
        ),
        (
            HIGHLIGHT_PLAYER_BLUE,
            &bits_player_fill,
            [
                39.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
                178.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
                255.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
            ],
        ),
        (
            HIGHLIGHT_PLAYER_PURPLE,
            &bits_player_fill,
            [
                206.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
                59.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
                255.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
            ],
        ),
        (
            HIGHLIGHT_PLAYER_RED,
            &bits_player_fill,
            [
                219.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
                2.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
                2.0 / 256.0 * g_settings.glow_settings.player_brightness as f32,
            ],
        ),
        (
            // Repurposed: id 79 is the only player context empirically confirmed to draw through
            // walls on v3.0.3.65, so ALL live players use it (see player_glow). The default
            // player_glow branch routes every live player here, so THIS context's color is the
            // one the glow-color menu must drive — it was previously a hardcoded pink that
            // ignored color_settings entirely ("color always stuck, menu does nothing"). Wire it
            // to the user's enemy/not-visible color, brightness-scaled like every other context.
            //
            // Fill vs outline: task_player_glow hardcodes CTX_PLAYER = 79 for every target, so
            // THIS slot's bits decide how an occluded model renders. `bits_player_fill` carries
            // both an inside function (player_glow_inside_value) and an outline function
            // (player_glow_outline_value, default 125), so once this write actually reaches the
            // renderer a close occluded player outlines through the wall on these bits alone.
            // `player_filled_toggle` now selects between the two here instead of being consulted
            // only by player_glow() (which the write path never calls) — turning it off drops the
            // inside pass entirely, which is the pure-outline A/B if the fill pass ever turns out
            // to be what the occlusion culler is throwing away.
            HIGHLIGHT_PLAYER_RAINBOW,
            if g_settings.glow_settings.player_filled_toggle {
                &bits_player_fill
            } else {
                &bits_player_outline
            },
            [
                g_settings.color_settings.glow_r_not
                    * g_settings.glow_settings.player_brightness as f32,
                g_settings.color_settings.glow_g_not
                    * g_settings.glow_settings.player_brightness as f32,
                g_settings.color_settings.glow_b_not
                    * g_settings.glow_settings.player_brightness as f32,
            ],
        ),
    ];

    // Use the module base passed in directly — avoids a second get_baseaddr() round-trip
    // that can return None on a slow tick and silently skip the entire inject.
    let base = module_base;
    if base == 0 {
        return Ok(());
    }

    let table_ptr_va = base + G_OFFSETS.highlight_settings;
    let highlight_settings_ptr = mem
        .read::<u64>(table_ptr_va, dmalib::PRIO_HIGH, 0)
        .await?;

    // Refuse to write anything unless the table pointer is a plausible user-mode address.
    //
    // This read returns 0 whenever it lands during an engine state transition — the pointer is
    // populated late and torn down between matches. Unguarded, every consumer below computes
    // `0 + 0x34 * context_id` and we issue 44 primary writes plus 22 shadow writes into the
    // first 4 KiB of the process's address space, every tick, for as long as the pointer stays
    // null. Observed live immediately before a crash:
    //
    //   inject_highlight wrote 22 contexts, highlight_settings_ptr: "0x0", ctx79_addr: "0x100c"
    //
    // UC 716406 #2609/#2610 describes exactly this class of failure — "If your read loop
    // attempts to read before the entity list is initialized, it reads 0x0, dereferences
    // [0x0 + offset], and crashes" — and every working glow snippet in that thread opens with
    // `if (!HighlightSettings) return;`. We had no such guard on any path.
    if !is_plausible_user_va(highlight_settings_ptr) {
        static LAST_BAD_PTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if LAST_BAD_PTR.swap(highlight_settings_ptr, Ordering::Relaxed) != highlight_settings_ptr {
            tracing::warn!(
                highlight_settings_ptr = format!("{highlight_settings_ptr:#x}"),
                table_ptr_va = format!("{table_ptr_va:#x}"),
                "{}",
                s!("highlight table pointer is not a usable address — skipping this inject pass")
            );
        }
        return Ok(());
    }

    // One-shot dump of the game's UNTOUCHED context table, read inline before the first write.
    //
    // This exists because the previous "ground truth" was circular and I built on it. The
    // `slot baseline read` line reads the slot through the shadow root inside the mirror loop —
    // which by then already contains OUR bits from an earlier pass. It reported
    // `game_vis: false, game_app: true` and I called that the renderer agreeing with us; it was
    // our own write echoed back. The giveaway was in the same line and I missed it:
    // `outline: 125  radius: 120  inside: 0` is the settings file verbatim.
    //
    // The counter-evidence is in this repo. `HighlightBits::SONAR` — Bloodhound's scan, a
    // through-walls effect that ships with the game — is `new(12, 169, 32, 7, true, false)`:
    // EntityVisible SET and AfterPostProcess CLEAR, the exact opposite of what the player contexts
    // now write. So the flip may be backwards, and I am not deciding that from a third argument.
    //
    // Read inline rather than spawned, because a spawned task races the write loop below and would
    // sample slots we had already clobbered. 91 slots at roughly 50 us is a few milliseconds, once
    // per process. Every non-zero slot the game ships is a configuration that demonstrably works,
    // and the ones that draw through walls carry the flag combination we need.
    {
        static DUMPED_CTX_DEFAULTS: AtomicBool = AtomicBool::new(false);
        if !DUMPED_CTX_DEFAULTS.swap(true, Ordering::Relaxed) {
            let mut text = String::from(
                "ctx  bits        inside outline radius state  vis    app    color\n",
            );
            for ctx in 0u8..=90 {
                let slot = highlight_settings_ptr + 0x34 * ctx as u64;
                let bits = mem.read::<HighlightBits>(slot, dmalib::PRIO_LOW, 0).await;
                let color = mem.read::<[f32; 4]>(slot + 0x04, dmalib::PRIO_LOW, 0).await;
                if let (Ok(bits), Ok(color)) = (bits, color) {
                    if bits.to_int() == 0 {
                        continue;
                    }
                    text.push_str(&format!(
                        "{ctx:3}  {:#010x}  {:6} {:7} {:6} {:5}  {:5}  {:5}  \
                         [{:.2}, {:.2}, {:.2}, {:.2}]\n",
                        bits.to_int(),
                        bits.inside_function,
                        bits.outline_function,
                        bits.outline_radius,
                        bits.inside_opacity & 0x3f,
                        bits.is_entity_visible(),
                        bits.is_after_post_process(),
                        color[0],
                        color[1],
                        color[2],
                        color[3],
                    ));
                }
            }
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(
                "/home/sad/Music/apexsky-next/apexsky/dist-memflow-test-v11/log/ctx_defaults.txt",
            ) {
                let _ = file.write_all(text.as_bytes());
            }
        }
    }

    // Where the shadow mirror should write, when a shadow root is available.
    //
    // Two distinct ways the primary connector can be looking at the wrong table:
    //   1. The POINTER itself is decoyed — base+offset holds one value under the primary root and
    //      a different one under the shadow root. Then the real table is at the shadow-read
    //      address and writing to the primary address is pointless no matter which root we use.
    //   2. The pointer agrees but the PAGES it addresses are decoyed — same VA, different
    //      physical backing per root. This is the absOrigin situation that cr3::write_shadow was
    //      written for, applied to the table instead of an entity.
    // Reading the pointer under both roots separates the two, and costs one 8-byte read per pass.
    let shadow_table_ptr: Option<u64> = match shadow_dtb {
        None => None,
        Some(dtb) => {
            let via_shadow =
                crate::apexdream::base::cr3::read_at_dtb::<u64>(api, dtb, table_ptr_va)
                    .await
                    .ok()
                    // Same range check as the primary pointer: a shadow-read pointer is a read
                    // through a root we are still proving, so it is the *least* trustworthy
                    // address in this function and gets no exemption for being non-zero.
                    .filter(|&p| is_plausible_user_va(p));
            // Log a pointer disagreement once per distinct pair — this is the single most useful
            // diagnostic for "settings work in the firing range and do nothing in a match".
            if let Some(shadow_ptr) = via_shadow {
                if shadow_ptr != highlight_settings_ptr {
                    static LAST_MISMATCH: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    if LAST_MISMATCH.swap(shadow_ptr, Ordering::Relaxed) != shadow_ptr {
                        tracing::warn!(
                            primary_ptr = format!("{highlight_settings_ptr:#x}"),
                            shadow_ptr = format!("{shadow_ptr:#x}"),
                            dtb = format!("{dtb:#x}"),
                            "{}",
                            s!("highlight table pointer differs under shadow DTB — primary \
                                connector holds a decoy table")
                        );
                    }
                }
            }
            // Only mirror when the table pointer was read successfully through the confirmed
            // shadow root. Falling back to the primary pointer after a shadow read failure makes
            // a successful virtual write indistinguishable from a write to an EA decoy page.
            // The primary pass above remains responsible for firing-range/primary-mapped cases.
            via_shadow
        }
    };

    let mut futs_write_highlight_settings: Vec<tokio::task::JoinHandle<anyhow::Result<()>>> =
        Vec::with_capacity(highlight_settings_inject.len());
    // Slot payloads kept for the shadow mirror below, so it writes byte-identical content to the
    // primary pass without recomputing colors or re-walking the settings struct.
    let mut mirror_slots: Vec<(u8, HighlightSlotHead)> =
        Vec::with_capacity(highlight_settings_inject.len());
    for (context_id, &bits, color) in highlight_settings_inject {
        // Global highlight table entry layout (0x34 = 52 bytes per slot, from UC thread):
        //   +0x00  HighlightBits (u32)     — inside/outline function, radius, opacity
        //   +0x04  color [f32; 4] RGBA     — R, G, B, A
        //   +0x14  ??? (remaining up to 0x34)
        //   +0x20  pulse_speed (f32)
        //   +0x24  min_brightness (f32)
        //   +0x28  max_brightness (f32)
        // The pointer read from base+hs_offset IS the array start — no extra sub-object offset.
        // m_highlightGenericContexts=0x2a0 is an entity-local field, NOT a global table offset.
        //
        // The +0x04 color placement is CONFIRMED, not a guess. Colors, brightness (a multiplier
        // on the same field) and outline size (HighlightBits at +0x00) all take effect in the
        // firing range with exactly these two writes. A speculative layout that put vis_type at
        // +0x04 and color at +0x0C used to be recorded here; it cannot be right, because under it
        // the color write would land on vis_type and nothing would ever colour correctly anywhere.
        // Do not "fix" these offsets — the match-only failure is a mapping problem, not an offset
        // problem, which is what the shadow mirror below addresses.
        let context_offset = highlight_settings_ptr + 0x34 * context_id as u64;
        let color_rgba = [color[0], color[1], color[2], 1.0f32];
        // Mirror only the slots the entity write path can actually assign.
        //
        // The primary pass writes all 22 contexts and always has; it is the *shadow* writes that
        // correlate with the process dying. Two runs, both stopping within milliseconds of the
        // first successful 22-slot mirror (04:50:51.882 and 05:13:39.790), while 12 band scans
        // and 12 sweep slices in between killed nothing.
        //
        // REVERTED: mirror every context the primary pass writes, not just two.
        //
        // The argument for cutting this to [79, 70] was that `task_player_glow` only ever points an
        // entity at those two, so the other twenty slots are unreferenced. That argument is wrong
        // in one specific way that matters: the primary pass writes all twenty-two, and in a match
        // the primary write lands on a decoyed page. So the renderer's copy of every context except
        // 79 and 70 kept the game's shipped defaults, and any entity the game itself routes to one
        // of those contexts — or any frame where it reassigns one — renders on vanilla settings
        // instead of ours.
        //
        // The evidence is the session history, not theory. The state the user described as "glow is
        // working amazing" was the twenty-two-slot mirror. I cut it to two on a crash correlation,
        // and both real crash causes were found and fixed independently afterwards: the unguarded
        // null table pointer (`highlight_settings_ptr: 0x0`, writing into the guard page every
        // tick) and the three unverified per-entity writes at `0x25c` / `0x29d` / `0x254`, which
        // the baseline read later proved were landing on unidentified memory. Neither had anything
        // to do with how many slots the mirror touched, and every regression since has been
        // measured on top of a table that was only two slots deep.
        //
        // It is also cheaper now than the version that worked. Read-compare-then-write means that
        // in steady state these are two reads and zero writes per slot per pass, where the build
        // the user liked issued twenty-two unconditional writes every pass.
        mirror_slots.push((context_id, HighlightSlotHead { bits, color_rgba }));
        futs_write_highlight_settings.push(tokio::spawn({
            let mem = mem.clone();
            async move {
                tokio::try_join!(
                    mem.write::<HighlightBits>(context_offset,        &bits,       dmalib::PRIO_HIGH, 0),
                    mem.write::<[f32; 4]>(     context_offset + 0x04, &color_rgba, dmalib::PRIO_HIGH, 0),
                )
                .context(format!("{context_id:?}"))?;
                Ok(())
            }
        }));
    }
    for task in futs_write_highlight_settings {
        task.await??;
    }

    // ── Shadow mirror ────────────────────────────────────────────────────────────────────────
    //
    // Same bytes, same slot addresses, written through the secondary page-table root. The primary
    // pass above is untouched, so this is strictly additive: whichever root actually maps the
    // renderer's copy of the table gets the settings, and the firing-range path (where the primary
    // write already lands) keeps working exactly as before.
    //
    // Driven on content change, plus a periodic re-drive every SHADOW_MIRROR_EVERY calls to
    // recover from an anti-cheat rewriting slots behind us. Skipping unchanged passes matters:
    // every write here takes memflow's process lock and does set_dtb → write → set_dtb, and this
    // function runs on every actions tick.
    // Only mirror through a root the live shadow-origin probe has confirmed. `write_at_dtb`
    // enforces this too, but checking here skips 22 doomed writes and 22 log lines per pass
    // rather than discovering it slot by slot.
    let shadow_dtb = shadow_dtb.filter(|&dtb| crate::apexdream::base::cr3::is_probe_confirmed(dtb));

    // Mirror availability transitions, logged both ways.
    //
    // Until now the success line was a one-shot per process, so "mirrored through shadow DTB"
    // appearing once told us the mirror had worked at some point and nothing about whether it was
    // working NOW. That blind spot is why the intermittent glow took so long to pin down: with no
    // live root the renderer keeps the game's own slot 79 — `0x40ff9b7d`, a blue non-player
    // effect — and the result on screen is a weak close-and-visible-only glow, which reads exactly
    // like a broken wallhack rather than a missing page-table root.
    //
    // One line per transition turns mirror uptime into something measurable: pair the timestamps
    // and the ratio falls out.
    {
        static MIRROR_DTB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let current = shadow_dtb.unwrap_or(0);
        let previous = MIRROR_DTB.swap(current, Ordering::Relaxed);
        if previous != current {
            tracing::info!(
                previous_dtb = %format!("{previous:#x}"),
                dtb = %format!("{current:#x}"),
                active = current != 0,
                "{}",
                s!("highlight shadow mirror availability changed")
            );
        }
    }

    if let (Some(dtb), Some(table_ptr)) = (shadow_dtb, shadow_table_ptr) {
        static MIRROR_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        static MIRRORED_FINGERPRINT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);

        // FNV-1a over (slot id, 20 payload bytes) for every slot, plus the table address, so a
        // table move or a re-attach also forces a re-drive.
        let fingerprint = {
            let mut hash = 0xcbf2_9ce4_8422_2325u64;
            let mut feed = |byte: u8| {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(0x100_0000_01b3);
            };
            for byte in table_ptr.to_le_bytes() {
                feed(byte);
            }
            for (context_id, head) in &mirror_slots {
                feed(*context_id);
                for &byte in head.as_bytes() {
                    feed(byte);
                }
            }
            hash
        };

        let calls = MIRROR_CALLS.fetch_add(1, Ordering::Relaxed);
        let content_changed = MIRRORED_FINGERPRINT.load(Ordering::Relaxed) != fingerprint;
        let periodic_redrive = calls % SHADOW_MIRROR_EVERY == 0;

        if content_changed || periodic_redrive {
            let started = Instant::now();
            let mut written = 0usize;
            let mut first_error: Option<anyhow::Error> = None;
            // Sequential on purpose. These all serialize on memflow's single process lock, so
            // spawning a task per slot would only add 22 contending tasks to the same critical
            // section.
            for (context_id, head) in &mirror_slots {
                let slot = table_ptr + 0x34 * *context_id as u64;
                // Two writes, not one 20-byte blob.
                //
                // The canonical sequence every working snippet in UC 716406 uses is a 4-byte
                // function-bits write at +0x00 and a separate 16-byte colour write at +0x04
                // (#42 Aimxex and ten verbatim re-posts). Coalescing them into one 20-byte store
                // was my own optimisation and it is not what any of that field-tested code does.
                // The renderer reads these slots every frame, so a single wide store leaves a
                // larger window in which it can observe a half-updated slot — and the function
                // bits are the half that decides how the model is drawn, not merely what colour
                // it is. Matching the proven access pattern costs one extra lock acquisition per
                // slot and removes the deviation.
                // Read-compare-then-write (UC 716406 #1003, xunyangjian) — the only
                // don't-fight-the-game idiom anyone in that thread posted:
                //
                //   const GlowMode oldGlowMode = mem::read<GlowMode>(... + 0);
                //   if (newGlowMode != oldGlowMode) mem::write<GlowMode>(... + 0, newGlowMode);
                //
                // In steady state the slot already holds what we want, so this turns two writes
                // per slot per pass into two reads and no writes. A read through a shadow root
                // that turns out to be wrong returns garbage the caller discards; a write through
                // one modifies whatever the bogus translation resolves to. Trading writes for
                // reads is the whole point.
                let unchanged = {
                    let have_bits = crate::apexdream::base::cr3::read_at_dtb::<HighlightBits>(
                        api, dtb, slot,
                    )
                    .await
                    .ok()
                    .map(|b| b.to_int());
                    let have_color = crate::apexdream::base::cr3::read_at_dtb::<[f32; 4]>(
                        api,
                        dtb,
                        slot + 0x04,
                    )
                    .await
                    .ok();
                    // Ground truth for the two visibility flags, logged once per context.
                    //
                    // Whatever the renderer is using right now for this slot is, by definition, a
                    // configuration that works — when the mirror is inactive, occluded players glow
                    // on these exact bits. So if `PLAYER_ENTITY_VISIBLE` /
                    // `PLAYER_AFTER_POST_PROCESS` above are the wrong way round, this line says so
                    // directly instead of costing another match: compare `game_vis` / `game_app`
                    // against `ours_vis` / `ours_app` and copy the game's.
                    if let Some(raw) = have_bits {
                        // Lock-free 128-bit "already logged" set over context ids, so this costs one
                        // atomic in steady state and cannot deadlock inside the mirror loop.
                        static LOGGED_LO: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        static LOGGED_HI: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        let first_time = if *context_id < 128 {
                            let bit = 1u64 << (*context_id % 64);
                            let word = if *context_id < 64 { &LOGGED_LO } else { &LOGGED_HI };
                            word.fetch_or(bit, Ordering::Relaxed) & bit == 0
                        } else {
                            false
                        };
                        if first_time {
                            let game = HighlightBits::from_uint(raw);
                            tracing::info!(
                                context_id = *context_id,
                                game_bits = %format!("{raw:#010x}"),
                                game_vis = game.is_entity_visible(),
                                game_app = game.is_after_post_process(),
                                game_inside = game.inside_function,
                                game_outline = game.outline_function,
                                game_radius = game.outline_radius,
                                ours_bits = %format!("{:#010x}", head.bits.to_int()),
                                ours_vis = head.bits.is_entity_visible(),
                                ours_app = head.bits.is_after_post_process(),
                                "{}",
                                s!("highlight slot baseline read through shadow DTB before first \
                                    overwrite")
                            );
                        }
                    }
                    have_bits == Some(head.bits.to_int()) && have_color == Some(head.color_rgba)
                };
                if unchanged {
                    written += 1;
                    continue;
                }

                let bits_result =
                    crate::apexdream::base::cr3::write_at_dtb(api, dtb, slot, &head.bits).await;
                let result = match bits_result {
                    Ok(()) => {
                        crate::apexdream::base::cr3::write_at_dtb(
                            api,
                            dtb,
                            slot + 0x04,
                            &head.color_rgba,
                        )
                        .await
                    }
                    Err(e) => Err(e),
                };
                match result {
                    Ok(()) => written += 1,
                    Err(e) => {
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                    }
                }
            }

            if written > 0 {
                MIRRORED_FINGERPRINT.store(fingerprint, Ordering::Relaxed);
            }

            // One line the first time the mirror lands, and one the first time it fails. This is
            // the confirmation that the global table was the decoyed write all along.
            static LOGGED_OK: AtomicBool = AtomicBool::new(false);
            static LOGGED_ERR: AtomicBool = AtomicBool::new(false);
            if written == mirror_slots.len() {
                if !LOGGED_OK.swap(true, Ordering::Relaxed) {
                    tracing::info!(
                        slots = written,
                        dtb = format!("{dtb:#x}"),
                        table_ptr = format!("{table_ptr:#x}"),
                        elapsed_us = started.elapsed().as_micros(),
                        "{}",
                        s!("highlight table mirrored through shadow DTB")
                    );
                }
            } else if let Some(e) = first_error {
                if !LOGGED_ERR.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        %e,
                        written,
                        total = mirror_slots.len(),
                        dtb = format!("{dtb:#x}"),
                        table_ptr = format!("{table_ptr:#x}"),
                        "{}",
                        s!("highlight table shadow mirror incomplete")
                    );
                }
            }
        }
    }
    // inject_highlight runs every tick (the rainbow context needs re-driving), so logging
    // unconditionally here spammed one INFO per tick straight over the TUI. The only part
    // worth announcing is a CHANGE of the table pointer — that's a re-attach or a table
    // move, which is genuinely worth seeing. Steady state stays silent.
    {
        static LAST_LOGGED_PTR: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        if LAST_LOGGED_PTR.swap(highlight_settings_ptr, Ordering::Relaxed)
            != highlight_settings_ptr
        {
            tracing::info!(
                highlight_settings_ptr = format!("{:#x}", highlight_settings_ptr),
                ctx79_addr = format!("{:#x}", highlight_settings_ptr + 0x34 * 79),
                "inject_highlight wrote {} contexts",
                highlight_settings_inject.len(),
            );
        }
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
fn calculate_target_fov(from: &dyn AimEntity, target: &dyn AimEntity) -> f32 {
    let view_angles = from.get_sway_angles();
    let local_camera = from.get_cam_pos();
    let entity_position = target.get_bone_position_by_hitbox(2);
    let angle = calc_angle(&local_camera, &entity_position);
    calc_fov(&view_angles, &angle)
}

fn rainbow_color(frame_count: i32) -> [f32; 3] {
    const FREQUENCY: f32 = 0.1; // Adjust the speed of color change
    const AMPLITUDE: f32 = 0.5; // Adjust the amplitude of color change

    // Use the sine function to generate rainbow color variation
    let frame_number = frame_count as f32;
    let r = (FREQUENCY * frame_number + 0.0).sin() * AMPLITUDE + 0.5;
    let g = (FREQUENCY * frame_number + 2.0).sin() * AMPLITUDE + 0.5;
    let b = (FREQUENCY * frame_number + 4.0).sin() * AMPLITUDE + 0.5;

    // Clamp the colors to the range [0, 1]
    [r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0)]
}

fn teammate_check(
    entity_team: i32,
    local_team: i32,
    map_testing_local_team: i32,
    tdm_toggle: bool,
) -> bool {
    if tdm_toggle {
        let ent_team = if entity_team % 2 == 1 { 1 } else { 2 };
        let loc_team = if local_team % 2 == 1 { 1 } else { 2 };
        tracing::trace!(
            target_team = ent_team,
            local_team = loc_team,
            "{}",
            s!("TDM check")
        );
        ent_team == loc_team
    } else {
        entity_team == local_team
            || (map_testing_local_team != 0 && entity_team == map_testing_local_team)
    }
}

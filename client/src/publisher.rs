//! SpacetimeDB publisher: owns the DbConnection on its own thread and turns `Update`s into
//! reducer calls. Batches entity moves (last position per entity wins) so a busy world does not
//! turn into thousands of tiny websocket messages per second.

use crate::module_bindings::*;
use spacetimedb_sdk::{DbContext, Table};
use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

pub enum Update {
    BotStatus {
        server_addr: String,
        username: String,
        state: String,
        detail: String,
        dimension: String,
        pos: (f64, f64, f64),
        view_distance: i32,
        chunks_loaded: u32,
        packets_seen: u64,
    },
    Time { dimension: String, game_time: i64, day_time: i64 },
    BlockStates(Vec<BlockStateDef>),
    Registry(Vec<RegistryDef>),
    Chunk { dimension: String, cx: i32, cz: i32, min_section: i32, sections: Vec<SectionData>, block_entities: Vec<BlockEntityData> },
    Sections { cx: i32, cz: i32, sections: Vec<SectionData>, changes: Vec<BlockChangeData> },
    Light { cx: i32, cz: i32, sections: Vec<(i32, Vec<u8>, Vec<u8>)> },
    Unload { cx: i32, cz: i32 },
    ClearWorld,
    BlockEntities(Vec<BlockEntityData>),
    RemoveBlockEntities(Vec<i64>),
    Entities(Vec<EntityData>),
    Moves(Vec<EntityMove>),
    RemoveEntities(Vec<i32>),
    EntityName { id: i32, name: Option<String> },
    /// merged SynchedEntityData scalars as JSON; sent only when the merged map actually changed
    EntityAppearance { id: i32, appearance: String },
    Players(Vec<PlayerData>),
    RemovePlayers(Vec<String>),
    /// ComputerCraft monitor screens, already rate-limited and de-duplicated by the client
    /// (see protocol.rs `flush_monitors`) -- by the time one gets here it is known to be a change.
    Monitors(Vec<MonitorData>),
    /// merged `set_equipment` slots as JSON; sent only when the merged map actually changed
    EntityEquipment { id: i32, equipment: String },
    Container(ContainerData),
    ContainerSlot { window_id: i32, slot: i32, stack: String, state_id: i32 },
    CloseContainer { window_id: i32 },
    CloseAllContainers,
    /// terminal outcome for one `bot_command` row
    CommandDone { id: u64, status: String, detail: String },
    /// every request made while the bot was down is abandoned on connect -- see the reducer
    ExpirePendingCommands,
}

/// One request from the `bot_command` table, handed to the packet loop.
///
/// The bot is the only thing that can act in the world, and it lives in the packet loop; the
/// SpacetimeDB connection lives on the publisher thread. So requests cross a channel in the opposite
/// direction from everything else in this file.
pub struct BotCmd {
    pub id: u64,
    pub kind: String,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub face: String,
    pub window_id: i32,
    pub slot: i32,
    pub button: i32,
    pub mode: i32,
    /// `drag` only: comma-separated slot list. See the `slots` column on `BotCommand` for why it is a
    /// String and not a `Vec<i32>`.
    pub slots: String,
}

pub type Tx = Sender<Update>;

/// Subscribe to pending `bot_command` rows and forward each one to the packet loop exactly once.
///
/// Deliberately narrow: the query is only the PENDING rows, so a finished command never comes back,
/// and the row is forwarded on insert only. A subscription that matched every row would re-deliver
/// the whole history on every reconnect and the bot would re-open containers from last week.
fn subscribe_commands(conn: &DbConnection, tx: std::sync::mpsc::Sender<BotCmd>) {
    let t2 = tx.clone();
    conn.db.bot_command().on_insert(move |_ctx, row| {
        if row.status != "pending" {
            return;
        }
        let _ = t2.send(BotCmd {
            id: row.id,
            kind: row.kind.clone(),
            x: row.x,
            y: row.y,
            z: row.z,
            face: row.face.clone(),
            window_id: row.window_id,
            slot: row.slot,
            button: row.button,
            mode: row.mode,
            slots: row.slots.clone(),
        });
    });
    conn.subscription_builder()
        .on_applied(|_ctx| info!("subscribed to bot_command"))
        .on_error(|_ctx, e| error!("bot_command subscription failed: {e}"))
        .subscribe(["SELECT * FROM bot_command WHERE status = 'pending'"]);
}

pub fn connect(uri: &str, db_name: &str) -> anyhow::Result<DbConnection> {
    let connected = Arc::new(Mutex::new(false));
    let c2 = connected.clone();
    let conn = DbConnection::builder()
        .with_uri(uri)
        .with_database_name(db_name)
        .on_connect(move |_ctx, identity, _token| {
            info!("SpacetimeDB connected as {identity}");
            *c2.lock().unwrap() = true;
        })
        .on_connect_error(|_ctx, e| {
            error!("SpacetimeDB connect error: {e}");
        })
        .on_disconnect(|_ctx, e| {
            warn!("SpacetimeDB disconnected: {e:?}");
        })
        .build()?;
    conn.run_threaded();
    // wait (bounded) for the connection to come up
    let start = Instant::now();
    while !*connected.lock().unwrap() {
        if start.elapsed() > Duration::from_secs(20) {
            anyhow::bail!("SpacetimeDB did not connect within 20s ({uri})");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(conn)
}

/// Blocking loop; run on its own thread.
pub fn run(conn: DbConnection, rx: Receiver<Update>, cmd_tx: Sender<BotCmd>) {
    subscribe_commands(&conn, cmd_tx);
    let mut calls: u64 = 0;
    let mut last_log = Instant::now();
    loop {
        let first = match rx.recv() {
            Ok(u) => u,
            Err(_) => {
                info!("publisher: channel closed");
                return;
            }
        };
        let mut batch = vec![first];
        while batch.len() < 256 {
            match rx.try_recv() {
                Ok(u) => batch.push(u),
                Err(_) => break,
            }
        }
        // coalesce moves
        let mut moves: BTreeMap<i32, EntityMove> = BTreeMap::new();
        for u in batch {
            match u {
                Update::Moves(ms) => {
                    for m in ms {
                        moves.insert(m.id, m);
                    }
                }
                other => {
                    if !moves.is_empty() {
                        apply(&conn, Update::Moves(moves.values().cloned().collect()), &mut calls);
                        moves.clear();
                    }
                    apply(&conn, other, &mut calls);
                }
            }
        }
        if !moves.is_empty() {
            apply(&conn, Update::Moves(moves.into_values().collect()), &mut calls);
        }
        if last_log.elapsed() > Duration::from_secs(30) {
            info!("publisher: {calls} reducer calls so far");
            last_log = Instant::now();
        }
        if !conn.is_active() {
            error!("SpacetimeDB connection lost; exiting so the supervisor restarts us");
            std::process::exit(3);
        }
    }
}

fn apply(conn: &DbConnection, u: Update, calls: &mut u64) {
    *calls += 1;
    let r = match u {
        Update::BotStatus { server_addr, username, state, detail, dimension, pos, view_distance, chunks_loaded, packets_seen } => {
            conn.reducers.set_bot_status(server_addr, username, state, detail, dimension, pos.0, pos.1, pos.2, view_distance, chunks_loaded, packets_seen)
        }
        Update::Time { dimension, game_time, day_time } => conn.reducers.set_time(dimension, game_time, day_time),
        Update::BlockStates(defs) => conn.reducers.set_block_states(defs),
        Update::Registry(defs) => conn.reducers.set_registry_entries(defs),
        Update::Chunk { dimension, cx, cz, min_section, sections, block_entities } => conn.reducers.upsert_chunk(dimension, cx, cz, min_section, sections, block_entities),
        Update::Sections { cx, cz, sections, changes } => conn.reducers.update_sections(cx, cz, sections, changes),
        Update::Light { cx, cz, sections } => conn.reducers.update_light(cx, cz,
            sections.into_iter().map(|(sy, sky_light, block_light)| LightData { sy, sky_light, block_light }).collect()),
        Update::Unload { cx, cz } => conn.reducers.unload_chunk(cx, cz),
        Update::ClearWorld => conn.reducers.clear_world(),
        Update::BlockEntities(items) => conn.reducers.upsert_block_entities(items),
        Update::RemoveBlockEntities(keys) => conn.reducers.remove_block_entities(keys),
        Update::Entities(items) => conn.reducers.upsert_entities(items),
        Update::Moves(ms) => conn.reducers.move_entities(ms),
        Update::RemoveEntities(ids) => conn.reducers.remove_entities(ids),
        Update::EntityName { id, name } => conn.reducers.set_entity_name(id, name),
        Update::EntityAppearance { id, appearance } => conn.reducers.set_entity_appearance(id, appearance),
        Update::Players(items) => conn.reducers.upsert_players(items),
        Update::RemovePlayers(uuids) => conn.reducers.remove_players(uuids),
        Update::Monitors(items) => conn.reducers.update_monitors(items),
        Update::EntityEquipment { id, equipment } => conn.reducers.set_entity_equipment(id, equipment),
        Update::Container(c) => conn.reducers.upsert_container(c),
        Update::ContainerSlot { window_id, slot, stack, state_id } => conn.reducers.set_container_slot(window_id, slot, stack, state_id),
        Update::CloseContainer { window_id } => conn.reducers.close_container(window_id),
        Update::CloseAllContainers => conn.reducers.close_all_containers(),
        Update::CommandDone { id, status, detail } => conn.reducers.complete_bot_command(id, status, detail),
        Update::ExpirePendingCommands => conn.reducers.expire_pending_bot_commands(),
    };
    if let Err(e) = r {
        error!("reducer call failed: {e}");
    }
}

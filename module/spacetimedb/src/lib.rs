//! mcspacetime — SpacetimeDB module holding a live mirror of a Minecraft world as seen by a
//! protocol client (the bot in ../client). The bot is the single writer; everything else
//! subscribes.
//!
//! Design: the bot keeps the authoritative in-memory copy of every loaded chunk and pushes
//! WHOLE SECTIONS (16x16x16, in the vanilla paletted-container wire layout) on every change.
//! The module never has to unpack bit-packed block data; it just replaces rows. Block-level
//! deltas are additionally appended to `block_change` for consumers that want to react to
//! single blocks without diffing sections.
//!
//! Positions are packed like vanilla `BlockPos.asLong()` / `ChunkPos.toLong()` so primary
//! keys stay 64-bit integers and the viewer can compute them without a lookup.

use spacetimedb::{ReducerContext, SpacetimeType, Table, Timestamp};

// ───────────────────────────── key packing ─────────────────────────────

pub fn chunk_key(cx: i32, cz: i32) -> i64 {
    ((cx as i64) << 32) | (cz as u32 as i64)
}

/// (cx, cz, section_y) -> one i64. section_y is the section index (block y >> 4), may be negative.
pub fn section_key(cx: i32, cz: i32, sy: i32) -> i64 {
    // 24 bits cx, 24 bits cz, 16 bits sy
    (((cx as i64) & 0xFF_FFFF) << 40) | (((cz as i64) & 0xFF_FFFF) << 16) | ((sy as i64) & 0xFFFF)
}

/// vanilla BlockPos.asLong(): x 26 bits, z 26 bits, y 12 bits
pub fn block_key(x: i32, y: i32, z: i32) -> i64 {
    (((x as i64) & 0x3FF_FFFF) << 38) | (((z as i64) & 0x3FF_FFFF) << 12) | ((y as i64) & 0xFFF)
}

// ───────────────────────────── tables ─────────────────────────────

/// One row (id = 0). Bot connection state.
#[spacetimedb::table(accessor = bot_status, public)]
pub struct BotStatus {
    #[primary_key]
    pub id: u32,
    pub server_addr: String,
    pub username: String,
    /// "connecting" | "login" | "configuration" | "play" | "disconnected"
    pub state: String,
    pub detail: String,
    pub dimension: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub view_distance: i32,
    pub chunks_loaded: u32,
    pub packets_seen: u64,
    pub updated_at: Timestamp,
}

/// World clock, one row per dimension name.
#[spacetimedb::table(accessor = world_time, public)]
pub struct WorldTime {
    #[primary_key]
    pub dimension: String,
    pub game_time: i64,
    pub day_time: i64,
    pub updated_at: Timestamp,
}

/// Global block-state id -> block name + properties. Loaded from the dump the bot carries
/// (blockstates.json); the bot verifies it against the server's registry sync at connect time.
#[spacetimedb::table(accessor = block_state, public)]
pub struct BlockStateRow {
    #[primary_key]
    pub id: u32,
    /// "minecraft:oak_stairs"
    #[index(btree)]
    pub name: String,
    /// "facing=north,half=bottom,shape=straight,waterlogged=false" or ""
    pub properties: String,
}

/// id -> name for a registry the wire refers to by numeric id.
/// registry: "block" | "block_entity_type" | "entity_type" | "biome" | ...
#[spacetimedb::table(accessor = registry_entry, public,
    index(accessor = by_registry_id, btree(columns = [registry, id])))]
pub struct RegistryEntry {
    #[primary_key]
    pub key: String, // "<registry>:<id>"
    pub registry: String,
    pub id: u32,
    pub name: String,
}

#[spacetimedb::table(accessor = chunk, public)]
pub struct Chunk {
    #[primary_key]
    pub key: i64,
    pub dimension: String,
    #[index(btree)]
    pub cx: i32,
    #[index(btree)]
    pub cz: i32,
    /// lowest section index present (e.g. -4 for the overworld)
    pub min_section: i32,
    pub section_count: u32,
    pub loaded_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A 16x16x16 section in the vanilla paletted-container wire layout.
///
/// bits == 0: single-value palette, `palette[0]` fills the section, `data` empty.
/// bits 4..=8: indirect, `palette` holds global state ids, `data` holds 4096 entries of
///   `bits` bits each packed little-end-first into u64 words without straddling words
///   (exactly the Anvil `block_states.data` layout, 64/bits entries per word).
/// bits >= 15 (block) / >= 6 (biome): direct — `palette` empty, entries are global ids.
#[spacetimedb::table(accessor = chunk_section, public)]
pub struct ChunkSection {
    #[primary_key]
    pub key: i64,
    #[index(btree)]
    pub chunk_key: i64,
    pub cx: i32,
    pub cz: i32,
    pub sy: i32,
    pub non_air_count: u16,
    pub block_bits: u8,
    pub block_palette: Vec<u32>,
    pub block_data: Vec<u64>,
    /// 4x4x4 biomes, same layout with 64 entries
    pub biome_bits: u8,
    pub biome_palette: Vec<u32>,
    pub biome_data: Vec<u64>,
    pub updated_at: Timestamp,
}

/// LIGHT LIVES IN ITS OWN TABLE, FOR TWO REASONS.
///
/// The practical one: `Vec<u8>` cannot have a compile-time default, so adding these as columns on
/// the existing `chunk_section` could only land by wiping the database. A new table is a
/// non-breaking migration.
///
/// The better one: light and blocks change independently. The server sends light updates in their
/// own packet, and a subscriber that only wants terrain should not pay 4 KB per section for data it
/// will not read. Keyed identically to `chunk_section`, so joining them is free.
///
/// Both arrays are 2048 bytes: one nibble per cell, low nibble first, in the same
/// `(y<<8)|(z<<4)|x` order as the block data. An ABSENT ROW means the server has said nothing about
/// this section's light -- it does NOT mean darkness. A section the server calls *empty* is
/// uniformly zero (a sealed cave really is dark) and is stored as 2048 zero bytes, which is a
/// present row. Consumers must tell those two apart or the world goes black every time a chunk
/// arrives before its light does.
#[spacetimedb::table(accessor = chunk_light, public)]
pub struct ChunkLight {
    #[primary_key]
    pub key: i64,
    #[index(btree)]
    pub chunk_key: i64,
    pub cx: i32,
    pub cz: i32,
    pub sy: i32,
    pub sky_light: Vec<u8>,
    pub block_light: Vec<u8>,
    pub updated_at: Timestamp,
}

/// A block entity as the server describes it to clients (the *update tag*, not the full save
/// NBT). `nbt` is the raw network NBT (nameless root compound); `nbt_json` is a lossy JSON
/// rendering for humans/JS. ComputerCraft fields are lifted out when present.
#[spacetimedb::table(accessor = block_entity, public)]
pub struct BlockEntity {
    #[primary_key]
    pub key: i64,
    #[index(btree)]
    pub chunk_key: i64,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub type_id: u32,
    #[index(btree)]
    pub type_name: String,
    pub block_state_id: u32,
    pub nbt: Vec<u8>,
    pub nbt_json: String,
    #[index(btree)]
    pub computer_id: Option<i32>,
    pub label: Option<String>,
    pub fuel: Option<i32>,
    pub on: Option<bool>,
    pub left_upgrade: Option<String>,
    pub right_upgrade: Option<String>,
    pub updated_at: Timestamp,
}

/// One ComputerCraft monitor panel's screen, decoded from the mod's own
/// `computercraft:monitor_client` payload (see `client/src/computercraft.rs`).
///
/// **Keyed by the ORIGIN block**, which is the top-left of the panel as seen facing the screen —
/// that is the only block the mod ever addresses, and it is also the corner a renderer needs, so
/// there is exactly one row per panel however many blocks the panel is made of.
///
/// `block_width`/`block_height` and `facing` do NOT come from that payload — it carries the
/// terminal's size in characters and nothing about the blocks. They are filled in by the client
/// from the monitor block entity's update tag and the origin block's state, and are 0/"" until a
/// chunk carrying them has arrived. `term_width`/`term_height` are characters and are always
/// present; the two are related by the monitor's text scale, which is not on the wire at all.
///
/// `lines`, `fg` and `bg` are each `term_height` strings of `term_width` characters: the text, and
/// the foreground and background palette index of every single cell as a hex digit into `palette`.
/// CC colours per character, so a consumer holding one colour per screen has to flatten this
/// itself — the mirror does not choose for it.
#[spacetimedb::table(accessor = monitor, public)]
pub struct Monitor {
    /// block_key of the origin block
    #[primary_key]
    pub key: i64,
    #[index(btree)]
    pub chunk_key: i64,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// the origin block's `facing` property ("north"/"south"/"east"/"west"/...), "" if unknown
    pub facing: String,
    /// panel size in BLOCKS; 0 until the monitor's block entity has been seen
    pub block_width: u32,
    pub block_height: u32,
    /// terminal size in CHARACTERS
    pub term_width: u32,
    pub term_height: u32,
    /// advanced (colour) monitor
    pub colour: bool,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub cursor_blink: bool,
    pub cursor_fg: u8,
    pub cursor_bg: u8,
    /// false when the mod sent an empty terminal: the monitor exists but nothing drives it. The
    /// grids below are then empty, and that is a real state, not a gap in the mirror.
    pub has_screen: bool,
    pub lines: Vec<String>,
    pub fg: Vec<String>,
    pub bg: Vec<String>,
    /// 16 `#rrggbb`, indexed by the hex digits in `fg`/`bg`
    pub palette: Vec<String>,
    pub updated_at: Timestamp,
}

#[spacetimedb::table(accessor = entity, public)]
pub struct Entity {
    #[primary_key]
    pub id: i32,
    pub uuid: String,
    pub type_id: u32,
    #[index(btree)]
    pub type_name: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
    pub head_yaw: f32,
    pub vx: f64,
    pub vy: f64,
    pub vz: f64,
    /// entity-specific spawn data varint (block state for falling blocks, etc.)
    pub data: i32,
    pub custom_name: Option<String>,
    pub updated_at: Timestamp,
    /// SynchedEntityData scalars as JSON, `{"<index>": <value>}` -- what a sheep's colour byte or a
    /// villager's profession triple actually is on the wire, with no interpretation applied.
    ///
    /// **Deliberately uninterpreted.** "Index 17 is sheep colour" is true of one Minecraft version
    /// and false of any mod that adds a field to `Sheep`, so the proxy publishes indices and the
    /// consumer owns the per-mob rules. `None` means the walk has produced nothing for this entity
    /// yet -- not that the entity has no appearance.
    ///
    /// TWO THINGS MAKE THIS A NON-BREAKING MIGRATION, and it fails without either:
    ///  - `#[default(None)]`. An added column needs an explicit default annotation; "the type has a
    ///    sensible default" is not enough, the publish aborts with "requires a default value
    ///    annotation". (This is also why the `Vec<u8>` light columns could not be added this way --
    ///    see CLAUDE.md: a `Vec` default cannot be evaluated at compile time.)
    ///  - **Being LAST in the struct.** Inserting it next to `custom_name`, where it reads better,
    ///    makes the publish abort with "Reordering table entity requires a manual migration" --
    ///    column order is part of the schema, so a new column can only be appended.
    #[default(None)]
    pub appearance: Option<String>,
    /// Worn and held items as JSON, `{"<slot>": {stack} | null}` with slots
    /// `mainhand`/`offhand`/`feet`/`legs`/`chest`/`head`/`body` -- from `set_equipment`, which is a
    /// DIFFERENT packet from entity metadata and carries actual item stacks rather than indices.
    ///
    /// A slot present with `null` means the server said that slot is empty, which is a real
    /// statement; a slot absent means nothing has been said about it. Deltas, so the merged map
    /// lives in the client, same as `appearance`.
    ///
    /// Appended LAST, with `#[default(None)]` -- see the note on `appearance` for why both are
    /// required and what fails without each.
    #[default(None)]
    pub equipment: Option<String>,
}

/// An OPEN container, as the bot's own player sees it. **Session state, not world state.**
///
/// This is deliberately not modelled like `chunk` or `block_entity`: a container belongs to a player
/// and stops existing when it closes. A stale open container is worse than none — it shows a chest's
/// contents from ten minutes ago as if they were live — so `container_close` DELETES the row and so
/// does a disconnect or a dimension change. If there is no row, nothing is open. That is the whole
/// contract.
///
/// Window 0 is special and always present: it is the player's own inventory, which the server sends
/// unprompted. Any other window id is a container the bot opened, and there is at most one at a time
/// because a player can only have one open.
#[spacetimedb::table(accessor = container, public)]
pub struct Container {
    /// the server's window id. 0 is the player's own inventory.
    #[primary_key]
    pub window_id: i32,
    /// `minecraft:generic_9x3`, `minecraft:shulker_box`, … from the `minecraft:menu` registry.
    /// Empty for window 0, which has no `open_screen` packet.
    pub menu_type: String,
    /// the screen title as plain text ("Chest", or whatever a named block entity is called)
    pub title: String,
    /// The block the bot right-clicked to open this, when the proxy opened it itself. `None` for
    /// window 0 and for anything the server opened on its own.
    pub opened_from_x: Option<i32>,
    pub opened_from_y: Option<i32>,
    pub opened_from_z: Option<i32>,
    /// One entry per slot, in the menu's own slot order: a decoded item stack as JSON, or `""` for
    /// an empty slot. `""` rather than absent so slot indices stay meaningful.
    pub slots: Vec<String>,
    /// the stack held by the cursor, `""` if none
    pub carried: String,
    /// the server's container state id, from the last content/slot packet. Carried because
    /// `ContainerClick` will need it; nothing reads it yet.
    pub state_id: i32,
    /// `Some(name)` if a slot's stack carried a data component with no codec, so at least one slot is
    /// incomplete. Names the component — see `client/src/itemstack.rs`.
    pub undecoded_component: Option<String>,
    pub opened_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A request for the bot to DO something in the world, and its outcome.
///
/// This is the table that makes spacetime mode no longer read-only. The bot subscribes to it, runs
/// each `pending` row exactly once, and writes back a terminal status — so a browser can ask for an
/// action without holding a connection to the bot, and every request leaves a record of what
/// happened. Requests are never deleted by the bot; the caller decides how long to keep them.
///
/// `status`: `pending` -> `done` | `failed` | `rejected`. `rejected` means the bot refused before
/// sending anything (out of reach, not connected); `failed` means it acted and the expected result
/// did not arrive.
#[spacetimedb::table(accessor = bot_command, public)]
pub struct BotCommand {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    /// `open_container` | `close_container` | `click_slot` | `walk_to`
    ///
    /// `walk_to` is the only one that is not one packet and a reply: it stays `pending` for the whole
    /// walk, which can be a minute or more, so a caller polling for a terminal status must not treat
    /// a long `pending` as a hung bot.
    pub kind: String,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// which block face to click: `down`/`up`/`north`/`south`/`west`/`east`. Empty means "pick one",
    /// which is what a caller that just wants a chest opened should send.
    pub face: String,
    #[index(btree)]
    pub status: String,
    pub detail: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// `click_slot` only. Appended last with defaults -- see the note on `entity.appearance` for why
    /// both are required.
    #[default(0)]
    pub window_id: i32,
    #[default(0)]
    pub slot: i32,
    /// What this means DEPENDS ON `mode`: for pickup/quick_move it is the mouse button (0 left,
    /// 1 right), but for **swap (mode 2) it is the hotbar slot** 0-8, or 40 for the offhand -- so a
    /// caller that assumes 0/1 everywhere cannot express most legal swaps.
    #[default(0)]
    pub button: i32,
    /// `ClickType` ordinal. Implemented: **0 pickup, 1 quick_move (shift-click), 2 swap, 4 throw,
    /// 6 pickup_all (double-click)**. Anything else is rejected by the bot rather than sent.
    ///
    /// **4 (THROW) DROPS ITEMS ON THE GROUND.** It was withheld under a standing instruction that the bot
    /// must never do that; the user lifted it explicitly on 2026-09-12. The server ignores it unless the
    /// cursor is empty, so the bot refuses it in that case rather than sending a silent no-op.
    ///
    /// 5 (quick_craft) is not available here: it is a three-phase drag, so it has its own `request_drag`
    /// reducer that sends the whole sequence. 3 (clone) is creative-mode only.
    #[default(0)]
    pub mode: i32,
    /// `drag` only: the slots to spread the carried stack across, comma-separated.
    ///
    /// A **String** and not a `Vec<i32>` because a `Vec` column cannot have a compile-time default
    /// (`E0493`: the destructor cannot be evaluated at compile time), and without a default the column
    /// can only be added by wiping the database -- see the note on `entity.appearance`. The reducer
    /// takes a proper `Vec<i32>` and joins it, so callers never see this encoding. Last in the struct,
    /// because SpacetimeDB refuses a reorder.
    #[default("")]
    pub slots: String,
}

#[spacetimedb::table(accessor = player, public)]
pub struct Player {
    #[primary_key]
    pub uuid: String,
    #[index(btree)]
    pub name: String,
    pub entity_id: Option<i32>,
    pub online: bool,
    pub gamemode: i32,
    pub latency: i32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
    pub updated_at: Timestamp,
}

/// Append-only feed of single-block changes (pruned to the last `BLOCK_CHANGE_KEEP` rows).
#[spacetimedb::table(accessor = block_change, public)]
pub struct BlockChange {
    #[primary_key]
    #[auto_inc]
    pub seq: u64,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub old_state_id: u32,
    pub new_state_id: u32,
    pub at: Timestamp,
}

const BLOCK_CHANGE_KEEP: u64 = 20_000;

// ───────────────────────────── argument types ─────────────────────────────

#[derive(SpacetimeType, Clone)]
pub struct SectionData {
    pub sy: i32,
    pub non_air_count: u16,
    pub block_bits: u8,
    pub block_palette: Vec<u32>,
    pub block_data: Vec<u64>,
    pub biome_bits: u8,
    pub biome_palette: Vec<u32>,
    pub biome_data: Vec<u64>,
    /// Sky and block light, 2048 bytes each: one nibble per cell, low nibble first, in the same
    /// `(y<<8)|(z<<4)|x` order as the block data.
    ///
    /// EMPTY MEANS UNKNOWN, NOT DARK. The server sends light per section and omits sections it has
    /// nothing to say about; a section it calls *empty* is uniformly zero, which is a real value (a
    /// sealed cave IS dark) and arrives here as 2048 zero bytes. A consumer that treats the absent
    /// case as darkness will black out the world every time a chunk arrives before its light does.
    pub sky_light: Vec<u8>,
    pub block_light: Vec<u8>,
}

#[derive(SpacetimeType, Clone)]
pub struct BlockEntityData {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub type_id: u32,
    pub type_name: String,
    pub block_state_id: u32,
    pub nbt: Vec<u8>,
    pub nbt_json: String,
    pub computer_id: Option<i32>,
    pub label: Option<String>,
    pub fuel: Option<i32>,
    pub on: Option<bool>,
    pub left_upgrade: Option<String>,
    pub right_upgrade: Option<String>,
}

#[derive(SpacetimeType, Clone)]
pub struct ContainerData {
    pub window_id: i32,
    pub menu_type: String,
    pub title: String,
    pub opened_from_x: Option<i32>,
    pub opened_from_y: Option<i32>,
    pub opened_from_z: Option<i32>,
    pub slots: Vec<String>,
    pub carried: String,
    pub state_id: i32,
    pub undecoded_component: Option<String>,
}

#[derive(SpacetimeType, Clone)]
pub struct MonitorData {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub facing: String,
    pub block_width: u32,
    pub block_height: u32,
    pub term_width: u32,
    pub term_height: u32,
    pub colour: bool,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub cursor_blink: bool,
    pub cursor_fg: u8,
    pub cursor_bg: u8,
    pub has_screen: bool,
    pub lines: Vec<String>,
    pub fg: Vec<String>,
    pub bg: Vec<String>,
    pub palette: Vec<String>,
}

#[derive(SpacetimeType, Clone)]
pub struct EntityData {
    pub id: i32,
    pub uuid: String,
    pub type_id: u32,
    pub type_name: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
    pub head_yaw: f32,
    pub vx: f64,
    pub vy: f64,
    pub vz: f64,
    pub data: i32,
}

#[derive(SpacetimeType, Clone)]
pub struct EntityMove {
    pub id: i32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
    pub head_yaw: f32,
}

#[derive(SpacetimeType, Clone)]
pub struct BlockStateDef {
    pub id: u32,
    pub name: String,
    pub properties: String,
}

#[derive(SpacetimeType, Clone)]
pub struct RegistryDef {
    pub registry: String,
    pub id: u32,
    pub name: String,
}

#[derive(SpacetimeType, Clone)]
pub struct BlockChangeData {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub old_state_id: u32,
    pub new_state_id: u32,
}

#[derive(SpacetimeType, Clone)]
pub struct PlayerData {
    pub uuid: String,
    pub name: String,
    pub entity_id: Option<i32>,
    pub online: bool,
    pub gamemode: i32,
    pub latency: i32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
}

// ───────────────────────────── reducers ─────────────────────────────

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    ctx.db.bot_status().insert(BotStatus {
        id: 0,
        server_addr: String::new(),
        username: String::new(),
        state: "disconnected".into(),
        detail: String::new(),
        dimension: String::new(),
        x: 0.0,
        y: 0.0,
        z: 0.0,
        view_distance: 0,
        chunks_loaded: 0,
        packets_seen: 0,
        updated_at: ctx.timestamp,
    });
}

#[spacetimedb::reducer]
pub fn set_bot_status(
    ctx: &ReducerContext,
    server_addr: String,
    username: String,
    state: String,
    detail: String,
    dimension: String,
    x: f64,
    y: f64,
    z: f64,
    view_distance: i32,
    chunks_loaded: u32,
    packets_seen: u64,
) {
    let row = BotStatus {
        id: 0,
        server_addr,
        username,
        state,
        detail,
        dimension,
        x,
        y,
        z,
        view_distance,
        chunks_loaded,
        packets_seen,
        updated_at: ctx.timestamp,
    };
    if ctx.db.bot_status().id().find(0).is_some() {
        ctx.db.bot_status().id().update(row);
    } else {
        ctx.db.bot_status().insert(row);
    }
}

#[spacetimedb::reducer]
pub fn set_time(ctx: &ReducerContext, dimension: String, game_time: i64, day_time: i64) {
    let row = WorldTime { dimension: dimension.clone(), game_time, day_time, updated_at: ctx.timestamp };
    if ctx.db.world_time().dimension().find(&dimension).is_some() {
        ctx.db.world_time().dimension().update(row);
    } else {
        ctx.db.world_time().insert(row);
    }
}

#[spacetimedb::reducer]
pub fn set_block_states(ctx: &ReducerContext, defs: Vec<BlockStateDef>) {
    for d in defs {
        let row = BlockStateRow { id: d.id, name: d.name, properties: d.properties };
        if ctx.db.block_state().id().find(d.id).is_some() {
            ctx.db.block_state().id().update(row);
        } else {
            ctx.db.block_state().insert(row);
        }
    }
}

#[spacetimedb::reducer]
pub fn set_registry_entries(ctx: &ReducerContext, defs: Vec<RegistryDef>) {
    for d in defs {
        let key = format!("{}:{}", d.registry, d.id);
        let row = RegistryEntry { key: key.clone(), registry: d.registry, id: d.id, name: d.name };
        if ctx.db.registry_entry().key().find(&key).is_some() {
            ctx.db.registry_entry().key().update(row);
        } else {
            ctx.db.registry_entry().insert(row);
        }
    }
}

/// Whole chunk (all sections) — used on ClientboundLevelChunkWithLight.
#[spacetimedb::reducer]
pub fn upsert_chunk(
    ctx: &ReducerContext,
    dimension: String,
    cx: i32,
    cz: i32,
    min_section: i32,
    sections: Vec<SectionData>,
    block_entities: Vec<BlockEntityData>,
) {
    let ck = chunk_key(cx, cz);
    let now = ctx.timestamp;
    let count = sections.len() as u32;
    match ctx.db.chunk().key().find(ck) {
        Some(existing) => {
            ctx.db.chunk().key().update(Chunk {
                dimension,
                min_section,
                section_count: count,
                updated_at: now,
                ..existing
            });
        }
        None => {
            ctx.db.chunk().insert(Chunk {
                key: ck,
                dimension,
                cx,
                cz,
                min_section,
                section_count: count,
                loaded_at: now,
                updated_at: now,
            });
        }
    }
    for s in sections {
        put_section(ctx, ck, cx, cz, s);
    }
    // block entities: replace the chunk's set
    let old: Vec<i64> = ctx.db.block_entity().chunk_key().filter(ck).map(|b| b.key).collect();
    for k in old {
        ctx.db.block_entity().key().delete(k);
    }
    for be in block_entities {
        put_block_entity(ctx, be);
    }
}

fn put_section(ctx: &ReducerContext, ck: i64, cx: i32, cz: i32, s: SectionData) {
    let key = section_key(cx, cz, s.sy);
    // A BLOCK CHANGE CARRIES NO LIGHT, AND MUST NOT ERASE IT.
    //
    // put_section serves both the full chunk (which has light) and a section re-sent because blocks
    // changed (which does not). Writing empty arrays through would blank the light of exactly the
    // sections anyone edits -- a room going dark the moment you place a block in it, and only that
    // room, which is a miserable thing to debug. Empty means "nothing said", so it writes nothing
    // and leaves whatever is already known in place.
    if !s.sky_light.is_empty() || !s.block_light.is_empty() {
        let light = ChunkLight {
            key, chunk_key: ck, cx, cz, sy: s.sy,
            sky_light: s.sky_light.clone(), block_light: s.block_light.clone(),
            updated_at: ctx.timestamp,
        };
        if ctx.db.chunk_light().key().find(key).is_some() {
            ctx.db.chunk_light().key().update(light);
        } else {
            ctx.db.chunk_light().insert(light);
        }
    }
    let prior = ctx.db.chunk_section().key().find(key);
    let row = ChunkSection {
        key,
        chunk_key: ck,
        cx,
        cz,
        sy: s.sy,
        non_air_count: s.non_air_count,
        block_bits: s.block_bits,
        block_palette: s.block_palette,
        block_data: s.block_data,
        biome_bits: s.biome_bits,
        biome_palette: s.biome_palette,
        biome_data: s.biome_data,
        updated_at: ctx.timestamp,
    };
    if prior.is_some() {
        ctx.db.chunk_section().key().update(row);
    } else {
        ctx.db.chunk_section().insert(row);
    }
}

fn put_block_entity(ctx: &ReducerContext, be: BlockEntityData) {
    let key = block_key(be.x, be.y, be.z);
    let row = BlockEntity {
        key,
        chunk_key: chunk_key(be.x >> 4, be.z >> 4),
        x: be.x,
        y: be.y,
        z: be.z,
        type_id: be.type_id,
        type_name: be.type_name,
        block_state_id: be.block_state_id,
        nbt: be.nbt,
        nbt_json: be.nbt_json,
        computer_id: be.computer_id,
        label: be.label,
        fuel: be.fuel,
        on: be.on,
        left_upgrade: be.left_upgrade,
        right_upgrade: be.right_upgrade,
        updated_at: ctx.timestamp,
    };
    if ctx.db.block_entity().key().find(key).is_some() {
        ctx.db.block_entity().key().update(row);
    } else {
        ctx.db.block_entity().insert(row);
    }
}

/// Re-publish one or more sections of an already loaded chunk after block updates, with the
/// individual block deltas.
/// One section's light, as the light-update packet delivers it.
#[derive(SpacetimeType, Clone)]
pub struct LightData {
    pub sy: i32,
    pub sky_light: Vec<u8>,
    pub block_light: Vec<u8>,
}

/// LIGHT CHANGES WITHOUT BLOCKS CHANGING. Place a torch and the server sends a light update for the
/// affected sections and nothing else -- no chunk, no block change. Without this the mirrored light
/// stayed as it was when the chunk first arrived, so a freshly lit room stayed dark until something
/// unrelated forced a re-send. Writes only what it is given, so a section the update does not mention
/// keeps whatever is already known.
#[spacetimedb::reducer]
pub fn update_light(ctx: &ReducerContext, cx: i32, cz: i32, sections: Vec<LightData>) {
    let ck = chunk_key(cx, cz);
    for l in sections {
        if l.sky_light.is_empty() && l.block_light.is_empty() {
            continue;
        }
        let key = section_key(cx, cz, l.sy);
        let row = ChunkLight {
            key, chunk_key: ck, cx, cz, sy: l.sy,
            sky_light: l.sky_light, block_light: l.block_light,
            updated_at: ctx.timestamp,
        };
        if ctx.db.chunk_light().key().find(key).is_some() {
            ctx.db.chunk_light().key().update(row);
        } else {
            ctx.db.chunk_light().insert(row);
        }
    }
}

#[spacetimedb::reducer]
pub fn update_sections(
    ctx: &ReducerContext,
    cx: i32,
    cz: i32,
    sections: Vec<SectionData>,
    changes: Vec<BlockChangeData>,
) {
    let ck = chunk_key(cx, cz);
    if let Some(existing) = ctx.db.chunk().key().find(ck) {
        ctx.db.chunk().key().update(Chunk { updated_at: ctx.timestamp, ..existing });
    }
    for s in sections {
        put_section(ctx, ck, cx, cz, s);
    }
    for c in changes {
        ctx.db.block_change().insert(BlockChange {
            seq: 0,
            x: c.x,
            y: c.y,
            z: c.z,
            old_state_id: c.old_state_id,
            new_state_id: c.new_state_id,
            at: ctx.timestamp,
        });
    }
    prune_block_changes(ctx);
}

fn prune_block_changes(ctx: &ReducerContext) {
    let n = ctx.db.block_change().count();
    if n > BLOCK_CHANGE_KEEP + 1000 {
        let excess = (n - BLOCK_CHANGE_KEEP) as usize;
        let mut seqs: Vec<u64> = ctx.db.block_change().iter().map(|r| r.seq).collect();
        seqs.sort_unstable();
        for s in seqs.into_iter().take(excess) {
            ctx.db.block_change().seq().delete(s);
        }
    }
}

#[spacetimedb::reducer]
pub fn unload_chunk(ctx: &ReducerContext, cx: i32, cz: i32) {
    let ck = chunk_key(cx, cz);
    ctx.db.chunk().key().delete(ck);
    let secs: Vec<i64> = ctx.db.chunk_section().chunk_key().filter(ck).map(|s| s.key).collect();
    for k in secs {
        ctx.db.chunk_section().key().delete(k);
    }
    let bes: Vec<i64> = ctx.db.block_entity().chunk_key().filter(ck).map(|b| b.key).collect();
    for k in bes {
        ctx.db.block_entity().key().delete(k);
    }
    // A monitor row goes with its origin block: once the chunk is out of the bot's view the server
    // stops sending that screen, so keeping the row would leave a frozen screen on the map with no
    // way for a consumer to tell it from a live one.
    let mons: Vec<i64> = ctx.db.monitor().chunk_key().filter(ck).map(|m| m.key).collect();
    for k in mons {
        ctx.db.monitor().key().delete(k);
    }
}

/// Drop every chunk/section/block entity/entity (dimension change or reconnect).
#[spacetimedb::reducer]
pub fn clear_world(ctx: &ReducerContext) {
    let keys: Vec<i64> = ctx.db.chunk().iter().map(|c| c.key).collect();
    for k in keys {
        ctx.db.chunk().key().delete(k);
    }
    let keys: Vec<i64> = ctx.db.chunk_section().iter().map(|c| c.key).collect();
    for k in keys {
        ctx.db.chunk_section().key().delete(k);
    }
    let keys: Vec<i64> = ctx.db.block_entity().iter().map(|c| c.key).collect();
    for k in keys {
        ctx.db.block_entity().key().delete(k);
    }
    let keys: Vec<i64> = ctx.db.monitor().iter().map(|c| c.key).collect();
    for k in keys {
        ctx.db.monitor().key().delete(k);
    }
    let ids: Vec<i32> = ctx.db.entity().iter().map(|c| c.id).collect();
    for k in ids {
        ctx.db.entity().id().delete(k);
    }
}

#[spacetimedb::reducer]
pub fn upsert_block_entities(ctx: &ReducerContext, items: Vec<BlockEntityData>) {
    for be in items {
        put_block_entity(ctx, be);
    }
}

#[spacetimedb::reducer]
pub fn remove_block_entities(ctx: &ReducerContext, keys: Vec<i64>) {
    for k in keys {
        ctx.db.block_entity().key().delete(k);
    }
}

/// Create or replace a container session. Called by the bot on `open_screen` and on every
/// `container_set_content`.
#[spacetimedb::reducer]
pub fn upsert_container(ctx: &ReducerContext, c: ContainerData) {
    let existing = ctx.db.container().window_id().find(c.window_id);
    let row = Container {
        window_id: c.window_id,
        menu_type: c.menu_type,
        title: c.title,
        opened_from_x: c.opened_from_x,
        opened_from_y: c.opened_from_y,
        opened_from_z: c.opened_from_z,
        slots: c.slots,
        carried: c.carried,
        state_id: c.state_id,
        undecoded_component: c.undecoded_component,
        // The moment it OPENED, not the moment it last changed -- a consumer showing "open for 3s"
        // needs the first timestamp to survive every content update.
        opened_at: existing.as_ref().map(|e| e.opened_at).unwrap_or(ctx.timestamp),
        updated_at: ctx.timestamp,
    };
    if existing.is_some() {
        ctx.db.container().window_id().update(row);
    } else {
        ctx.db.container().insert(row);
    }
}

/// Replace one slot (a `container_set_slot` packet). Does nothing if the window is not open, and
/// does NOT create the row: a slot update for a window we never saw content for would produce a
/// container whose other slots are silently empty rather than unknown.
#[spacetimedb::reducer]
pub fn set_container_slot(ctx: &ReducerContext, window_id: i32, slot: i32, stack: String, state_id: i32) {
    let Some(existing) = ctx.db.container().window_id().find(window_id) else { return };
    let mut slots = existing.slots.clone();
    // -1 is the CARRIED item, not a slot index (ClientboundContainerSetSlotPacket.CARRIED_ITEM).
    if slot < 0 {
        ctx.db.container().window_id().update(Container { carried: stack, state_id, updated_at: ctx.timestamp, ..existing });
        return;
    }
    let i = slot as usize;
    if i >= slots.len() {
        // The menu is bigger than the content packet said. Grow rather than drop the update: a
        // missing slot reads as an empty one, and "the chest looks empty" is the bug that hides.
        slots.resize(i + 1, String::new());
    }
    slots[i] = stack;
    ctx.db.container().window_id().update(Container { slots, state_id, updated_at: ctx.timestamp, ..existing });
}

/// The container is gone. Authoritative: the row is deleted, never marked closed.
#[spacetimedb::reducer]
pub fn close_container(ctx: &ReducerContext, window_id: i32) {
    ctx.db.container().window_id().delete(window_id);
}

/// Every container session ends -- disconnect, dimension change, reconnect. Window 0 goes too: the
/// bot's inventory is only known while it is connected.
#[spacetimedb::reducer]
pub fn close_all_containers(ctx: &ReducerContext) {
    let ids: Vec<i32> = ctx.db.container().iter().map(|c| c.window_id).collect();
    for id in ids {
        ctx.db.container().window_id().delete(id);
    }
}

/// Ask the bot to open the container at a block. Returns nothing; watch the row's `status`.
///
/// Anyone who can call reducers on this database can move the bot's hands. That is a real change in
/// what this module is -- see README, "spacetime mode is no longer read-only".
#[spacetimedb::reducer]
pub fn request_open_container(ctx: &ReducerContext, x: i32, y: i32, z: i32, face: String) {
    ctx.db.bot_command().insert(BotCommand {
        id: 0,
        kind: "open_container".into(),
        x,
        y,
        z,
        face,
        status: "pending".into(),
        detail: String::new(),
        created_at: ctx.timestamp,
        updated_at: ctx.timestamp,
        window_id: 0,
        slot: 0,
        button: 0,
        mode: 0,
        slots: String::new(),
    });
}

/// Ask the bot to walk to a block, so it can mirror somewhere other than where it spawned.
///
/// `x, y, z` is a block; the bot aims at the centre of it and stands with its feet at `y`. The row
/// stays `pending` for the whole walk -- unlike every other command, which is one packet and a reply
/// -- and becomes `done` on arrival or `failed` with the distance still to go.
///
/// **It walks in a straight line and does not route around obstacles.** A wall between here and
/// there fails the command rather than solving it; see `client/src/walk.rs` for why that is the
/// honest first cut rather than a half-built pathfinder.
#[spacetimedb::reducer]
pub fn request_walk_to(ctx: &ReducerContext, x: i32, y: i32, z: i32) {
    ctx.db.bot_command().insert(BotCommand {
        id: 0,
        kind: "walk_to".into(),
        x,
        y,
        z,
        face: String::new(),
        status: "pending".into(),
        detail: String::new(),
        created_at: ctx.timestamp,
        updated_at: ctx.timestamp,
        window_id: 0,
        slot: 0,
        button: 0,
        mode: 0,
        slots: String::new(),
    });
}

/// Spread the carried stack across `slots` — vanilla's click-and-drag, `ClickType.QUICK_CRAFT`.
///
/// **All three phases are sent by one call**, which is the point of the API: the drag is a state machine
/// on the server (begin / add-slot / add-slot / … / end) and exposing the phases separately invites a
/// caller to leave one half-open. `right = false` splits the stack evenly between the slots; `right =
/// true` places one item in each.
///
/// The bot must already be carrying something — the drag distributes the CURSOR, not a slot — and
/// vanilla ignores any slot that cannot accept the carried item, so a partial result is normal rather
/// than an error.
#[spacetimedb::reducer]
pub fn request_drag(ctx: &ReducerContext, window_id: i32, slots: Vec<i32>, right: bool) {
    ctx.db.bot_command().insert(BotCommand {
        id: 0,
        kind: "drag".into(),
        x: 0,
        y: 0,
        z: 0,
        face: String::new(),
        status: "pending".into(),
        detail: String::new(),
        created_at: ctx.timestamp,
        updated_at: ctx.timestamp,
        window_id,
        slot: 0,
        button: if right { 1 } else { 0 },
        mode: 5, // ClickType.QUICK_CRAFT
        slots: slots.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(","),
    });
}

/// Ask the bot to close whatever it has open.
#[spacetimedb::reducer]
pub fn request_close_container(ctx: &ReducerContext) {
    ctx.db.bot_command().insert(BotCommand {
        id: 0,
        kind: "close_container".into(),
        x: 0,
        y: 0,
        z: 0,
        face: String::new(),
        status: "pending".into(),
        detail: String::new(),
        created_at: ctx.timestamp,
        updated_at: ctx.timestamp,
        window_id: 0,
        slot: 0,
        button: 0,
        mode: 0,
        slots: String::new(),
    });
}

/// Ask the bot to click a slot in the container it currently has open.
///
/// **This MUTATES the world.** Everything else this module does is a mirror or a window; a click moves
/// real items in a real base. The bot validates before sending (see `run_bot_command`): the window must
/// be one it actually has open, the slot must exist in that window, and `mode` must be one of the two
/// that are implemented. A stale `window_id` is REJECTED rather than sent, because the server silently
/// ignores a click for a window it does not have open — so a stale id would look like nothing happening
/// while the real risk is a future window reusing that id.
///
/// `mode`: 0 = pickup (button 0 left / 1 right), 1 = quick_move (shift-click, button ignored).
/// `slot` is an index into `container.slots`.
#[spacetimedb::reducer]
pub fn request_click_slot(ctx: &ReducerContext, window_id: i32, slot: i32, button: i32, mode: i32) {
    ctx.db.bot_command().insert(BotCommand {
        id: 0,
        kind: "click_slot".into(),
        x: 0,
        y: 0,
        z: 0,
        face: String::new(),
        status: "pending".into(),
        detail: String::new(),
        created_at: ctx.timestamp,
        updated_at: ctx.timestamp,
        window_id,
        slot,
        button,
        mode,
        slots: String::new(),
    });
}

/// The bot reporting what happened. Only ever moves a row OUT of `pending`.
#[spacetimedb::reducer]
pub fn complete_bot_command(ctx: &ReducerContext, id: u64, status: String, detail: String) {
    let Some(existing) = ctx.db.bot_command().id().find(id) else { return };
    // Guard against a second bot (or a restarted one replaying a subscription) re-finishing a row
    // that already has an outcome -- the FIRST outcome is the true one.
    if existing.status != "pending" {
        return;
    }
    ctx.db.bot_command().id().update(BotCommand { status, detail, updated_at: ctx.timestamp, ..existing });
}

/// Abandon every pending request. The bot calls this when it (re)connects: a request made while the
/// bot was down was never going to run, and leaving it pending would make it fire on reconnect at a
/// position the bot may no longer be anywhere near.
#[spacetimedb::reducer]
pub fn expire_pending_bot_commands(ctx: &ReducerContext) {
    let stale: Vec<BotCommand> = ctx.db.bot_command().status().filter(&"pending".to_string()).collect();
    for c in stale {
        ctx.db.bot_command().id().update(BotCommand { status: "rejected".into(), detail: "the bot was not connected".into(), updated_at: ctx.timestamp, ..c });
    }
}

#[spacetimedb::reducer]
pub fn update_monitors(ctx: &ReducerContext, items: Vec<MonitorData>) {
    for m in items {
        let key = block_key(m.x, m.y, m.z);
        let row = Monitor {
            key,
            chunk_key: chunk_key(m.x >> 4, m.z >> 4),
            x: m.x,
            y: m.y,
            z: m.z,
            facing: m.facing,
            block_width: m.block_width,
            block_height: m.block_height,
            term_width: m.term_width,
            term_height: m.term_height,
            colour: m.colour,
            cursor_x: m.cursor_x,
            cursor_y: m.cursor_y,
            cursor_blink: m.cursor_blink,
            cursor_fg: m.cursor_fg,
            cursor_bg: m.cursor_bg,
            has_screen: m.has_screen,
            lines: m.lines,
            fg: m.fg,
            bg: m.bg,
            palette: m.palette,
            updated_at: ctx.timestamp,
        };
        if ctx.db.monitor().key().find(key).is_some() {
            ctx.db.monitor().key().update(row);
        } else {
            ctx.db.monitor().insert(row);
        }
    }
}

#[spacetimedb::reducer]
pub fn remove_monitors(ctx: &ReducerContext, keys: Vec<i64>) {
    for k in keys {
        ctx.db.monitor().key().delete(k);
    }
}

#[spacetimedb::reducer]
pub fn upsert_entities(ctx: &ReducerContext, items: Vec<EntityData>) {
    for e in items {
        let row = Entity {
            id: e.id,
            uuid: e.uuid,
            type_id: e.type_id,
            type_name: e.type_name,
            x: e.x,
            y: e.y,
            z: e.z,
            yaw: e.yaw,
            pitch: e.pitch,
            head_yaw: e.head_yaw,
            vx: e.vx,
            vy: e.vy,
            vz: e.vz,
            data: e.data,
            custom_name: None,
            appearance: None,
            equipment: None,
            updated_at: ctx.timestamp,
        };
        if let Some(existing) = ctx.db.entity().id().find(e.id) {
            // A respawn/re-add packet carries no name or appearance, so both are carried over
            // rather than blanked -- losing them here would make an entity flicker back to its
            // default skin every time the server re-sent it.
            ctx.db.entity().id().update(Entity { custom_name: existing.custom_name, appearance: existing.appearance, equipment: existing.equipment, ..row });
        } else {
            ctx.db.entity().insert(row);
        }
    }
}

#[spacetimedb::reducer]
pub fn move_entities(ctx: &ReducerContext, moves: Vec<EntityMove>) {
    for m in moves {
        if let Some(existing) = ctx.db.entity().id().find(m.id) {
            ctx.db.entity().id().update(Entity {
                x: m.x,
                y: m.y,
                z: m.z,
                yaw: m.yaw,
                pitch: m.pitch,
                head_yaw: m.head_yaw,
                updated_at: ctx.timestamp,
                ..existing
            });
        }
    }
}

#[spacetimedb::reducer]
pub fn set_entity_name(ctx: &ReducerContext, id: i32, custom_name: Option<String>) {
    if let Some(existing) = ctx.db.entity().id().find(id) {
        ctx.db.entity().id().update(Entity { custom_name, updated_at: ctx.timestamp, ..existing });
    }
}

/// Replace an entity's appearance blob. The client sends the MERGED map, not the delta, and only
/// when something it carries actually changed -- so this is never called to re-state what the row
/// already holds.
#[spacetimedb::reducer]
pub fn set_entity_appearance(ctx: &ReducerContext, id: i32, appearance: String) {
    if let Some(existing) = ctx.db.entity().id().find(id) {
        ctx.db.entity().id().update(Entity { appearance: Some(appearance), updated_at: ctx.timestamp, ..existing });
    }
}

/// Replace an entity's equipment blob. Merged map, like the appearance one.
#[spacetimedb::reducer]
pub fn set_entity_equipment(ctx: &ReducerContext, id: i32, equipment: String) {
    if let Some(existing) = ctx.db.entity().id().find(id) {
        ctx.db.entity().id().update(Entity { equipment: Some(equipment), updated_at: ctx.timestamp, ..existing });
    }
}

#[spacetimedb::reducer]
pub fn remove_entities(ctx: &ReducerContext, ids: Vec<i32>) {
    for id in ids {
        ctx.db.entity().id().delete(id);
    }
}

#[spacetimedb::reducer]
pub fn upsert_players(ctx: &ReducerContext, items: Vec<PlayerData>) {
    for p in items {
        let row = Player {
            uuid: p.uuid.clone(),
            name: p.name,
            entity_id: p.entity_id,
            online: p.online,
            gamemode: p.gamemode,
            latency: p.latency,
            x: p.x,
            y: p.y,
            z: p.z,
            yaw: p.yaw,
            pitch: p.pitch,
            updated_at: ctx.timestamp,
        };
        if ctx.db.player().uuid().find(&p.uuid).is_some() {
            ctx.db.player().uuid().update(row);
        } else {
            ctx.db.player().insert(row);
        }
    }
}

#[spacetimedb::reducer]
pub fn remove_players(ctx: &ReducerContext, uuids: Vec<String>) {
    for u in uuids {
        if let Some(existing) = ctx.db.player().uuid().find(&u) {
            ctx.db.player().uuid().update(Player { online: false, entity_id: None, updated_at: ctx.timestamp, ..existing });
        }
    }
}

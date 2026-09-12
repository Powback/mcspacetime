//! One Minecraft session: handshake -> login -> configuration (NeoForge negotiation) -> play.
//! Everything after login is read as raw frames and decoded by hand (see wire.rs for why).

use crate::anvil;
use crate::blockstates::BlockStates;
use crate::computercraft;
use crate::itemstack;
use crate::metadata;
use crate::module_bindings::*;
use crate::neoforge::{self, Channels};
use crate::publisher::{Tx, Update};
use crate::wire::{component_to_text, nbt_compound_to_json, unpack_section_pos, Reader, Writer};
use crate::world::{Chunk, Section, World};
use anyhow::{anyhow, bail, Context, Result};

/// What we tell the server we can take per tick -- the ceiling vanilla clamps this to.
/// See the `chunk_batch_finished` handler: a headless mirror has no frame budget to protect.
const CHUNKS_PER_TICK: f32 = 64.0;
use azalea_protocol::connect::{Connection, RawReadConnection, RawWriteConnection};
use azalea_protocol::packets::handshaking::client_intention_packet::ClientIntentionPacket;
use azalea_protocol::packets::login::serverbound_custom_query_answer_packet::ServerboundCustomQueryAnswerPacket;
use azalea_protocol::packets::login::serverbound_hello_packet::ServerboundHelloPacket;
use azalea_protocol::packets::login::serverbound_key_packet::ServerboundKeyPacket;
use azalea_protocol::packets::login::serverbound_login_acknowledged_packet::ServerboundLoginAcknowledgedPacket;
use azalea_protocol::packets::login::ClientboundLoginPacket;
use azalea_protocol::packets::{ClientIntention, PROTOCOL_VERSION};
use azalea_protocol::ServerAddress;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, trace, warn};

pub enum Auth {
    Offline,
    Microsoft { email: String, cache_file: PathBuf },
}

pub struct Config {
    pub addr: String,
    pub username: String,
    pub auth: Auth,
    pub channels_path: PathBuf,
    pub view_distance: u8,
}

/// Shared across reconnects.
pub struct Ctx {
    pub tx: Tx,
    pub block_states: BlockStates,
    pub channels: Channels,
    pub block_states_published: bool,
    /// registry name -> id -> key (from NeoForge frozen registry sync, or the dump as fallback)
    pub registries: HashMap<String, HashMap<u32, String>>,
    /// world download (Anvil .mca). `None` unless WORLD_SAVE is on. Bounded on purpose -- see
    /// anvil.rs: a full channel drops chunks rather than stalling the packet loop.
    pub saver: Option<anvil::Tx>,
    /// chunks the saver could not keep up with, so the drop is visible instead of silent
    pub save_dropped: u64,
    /// requests from the `bot_command` table, forwarded by the publisher thread. The ONLY way
    /// anything outside this process makes the bot act.
    pub commands: std::sync::mpsc::Receiver<crate::publisher::BotCmd>,
}

#[derive(Debug)]
pub enum SessionEnd {
    /// negotiation failed but we learned something; reconnect right away
    NegotiationProgress(Vec<String>),
    /// negotiation failed and nothing could be learned from the reasons
    NegotiationStuck(Vec<String>),
    Disconnected(String),
    Error(anyhow::Error),
}

struct EntityInfo {
    uuid: String,
    type_id: u32,
    type_name: String,
    x: f64,
    y: f64,
    z: f64,
    yaw: f32,
    pitch: f32,
    head_yaw: f32,
}

struct PlayerInfo {
    name: String,
    gamemode: i32,
    latency: i32,
    listed: bool,
    entity_id: Option<i32>,
    pos: (f64, f64, f64),
    rot: (f32, f32),
}

struct Play {
    dimension: String,
    min_section: i32,
    section_count: usize,
    world: World,
    entities: HashMap<i32, EntityInfo>,
    players: HashMap<String, PlayerInfo>,
    bot_pos: (f64, f64, f64),
    view_distance: i32,
    packets: u64,
    last_status: Instant,
    biome_direct_bits: u8,
    /// dimension type name -> (min_y, height)
    dim_types: HashMap<String, (i32, i32)>,
    dim_type_ids: Vec<String>,
    /// `minecraft:enchantment` id -> name. A DATAPACK registry, so it arrives in `registry_data`
    /// during configuration and NOT in the NeoForge frozen sync -- the same reason biome names have
    /// to be captured from there. Item stacks refer to enchantments by index into this.
    enchantment_names: Vec<String>,
    /// `minecraft:banner_pattern` id -> name; another datapack registry, needed by `banner_patterns`.
    banner_pattern_names: Vec<String>,
    /// entity id -> merged metadata fields. Metadata packets are DELTAS -- the server sends the
    /// full set on spawn and only the changed indices afterwards -- so the merged map has to live
    /// somewhere, and keeping it here lets the client suppress the reducer call entirely when a
    /// packet changes nothing we carry (see the 0x58 handler).
    entity_meta: HashMap<i32, MetaState>,
    /// entities whose merged map changed but whose rate limit has not elapsed yet
    meta_dirty: std::collections::HashSet<i32>,
    last_meta_flush: Instant,
    meta_stats: MetaStats,
    /// monitor origin block -> its latest screen. Monitor payloads are FULL screens (see
    /// computercraft.rs), so unlike entity metadata there is nothing to *merge* -- but a program
    /// redrawing every tick re-sends the same screen, so the last published one is kept to drop
    /// no-op writes, and the pending one to rate-limit the rest.
    monitors: HashMap<(i32, i32, i32), MonitorSlot>,
    monitor_dirty: std::collections::HashSet<(i32, i32, i32)>,
    /// monitor origin block -> panel size in BLOCKS, lifted from the block entity's update tag.
    /// The screen payload carries characters, never blocks, so this is the only source for it.
    monitor_geom: HashMap<(i32, i32, i32), (u32, u32)>,
    monitor_stats: MonitorStats,
    /// entity id -> merged equipment slots. `set_equipment` is a DELTA like metadata: the server
    /// sends only the slots that changed, so the merged map has to live here or a mob that swaps its
    /// held item would appear to lose its armour.
    equipment: HashMap<i32, BTreeMap<&'static str, Value>>,
    /// window id -> open container. Session state; cleared on disconnect and dimension change.
    containers: HashMap<i32, ContainerState>,
    /// the `use_item_on` sequence number. A real client counts these and the server acks them; a
    /// client that reuses one gets its block changes rolled back.
    sequence: i32,
    /// the request we are waiting for a container to appear for, if any
    pending_open: Option<PendingOpen>,
    /// the click we are collecting the server's answer to, if any
    pending_click: Option<PendingClick>,
    /// where the bot is walking, if anywhere. See `walk.rs` for the movement rules.
    walking: Option<WalkTo>,
    /// Last known health, from `set_health`. 20.0 is full. Used to refuse a fall that would kill --
    /// the walker takes drops of up to `SAFE_FALL`, and a handful of those is a dead bot, which stops
    /// the mirror dead because a dead player is sent no chunks.
    health: f32,
    /// When a respawn was last asked for, so a server that ignores the request is retried rather than
    /// asked twenty times a second.
    respawn_asked: Option<Instant>,
    /// the yaw/pitch last sent. Kept so a walk can face where it is going, and so the accept-teleport
    /// reply does not have to invent a facing.
    bot_look: (f32, f32),
}

/// A walk in progress. One `bot_command` row, stepped one tick at a time by `advance_walk`.
struct WalkTo {
    id: u64,
    /// The block the caller asked for, kept for the final report.
    goal: (i32, i32, i32),
    /// Waypoints still to visit, nearest first, as block centres. Popped as each is reached. Empty
    /// means the last leg is in progress toward `target`.
    route: std::collections::VecDeque<(f64, f64, f64)>,
    /// The waypoint currently being walked to.
    target: (f64, f64, f64),
    /// Whether the route reached the goal or only the closest reachable point -- so the terminal
    /// status can say "arrived" or "got as close as the world allows" without re-deriving it.
    partial: bool,
    /// When the bot last actually moved. A walk that stops making progress has to end, or a
    /// `walk_to` into a wall stays `pending` for ever and the caller cannot tell it from a dead bot.
    last_progress: Instant,
    /// Where it was at `last_progress`, so "progress" means distance covered rather than ticks spent.
    progress_pos: (f64, f64, f64),
    started: Instant,
    steps: u32,
}

/// What the client knows about an open container.
///
/// **`slots`, `carried` and `state_id` are written from SERVER PACKETS ONLY.** Nothing in the click
/// path touches them. That is the whole answer to desync: a client that never predicts has nothing to
/// be wrong about, and the cost — the server sending a correction for every slot a click changed —
/// is a few hundred bytes on an action a human takes once.
struct ContainerState {
    menu_type: String,
    title: String,
    from: Option<(i32, i32, i32)>,
    /// the last state id the server stated. Sent back on a click so the server can answer with
    /// targeted corrections instead of a full resync.
    state_id: i32,
    /// one entry per slot, stack JSON or "" -- the server's last word, never a prediction
    slots: Vec<String>,
    /// the cursor stack, "" if none
    carried: String,
}

impl ContainerState {
    fn new(menu_type: String, title: String, from: Option<(i32, i32, i32)>) -> Self {
        ContainerState { menu_type, title, from, state_id: 0, slots: Vec::new(), carried: String::new() }
    }

    /// A whole `container_set_content`.
    fn set_content(&mut self, state_id: i32, slots: &[String], carried: &str) {
        self.state_id = state_id;
        self.slots = slots.to_vec();
        self.carried = carried.to_string();
    }

    /// One `container_set_slot`. This is also how the server CORRECTS a click it disagreed with, which
    /// is the normal case rather than the exceptional one -- so it overwrites unconditionally and
    /// never merges.
    fn apply_correction(&mut self, state_id: i32, slot: i32, stack: &str) {
        self.state_id = state_id;
        // Slot -1 is the CARRIED item (ClientboundContainerSetSlotPacket.CARRIED_ITEM), not index -1.
        if slot < 0 {
            self.carried = stack.to_string();
            return;
        }
        let i = slot as usize;
        if i >= self.slots.len() {
            // Grow rather than drop: a missing slot reads as an empty one, and "the chest looks
            // empty" is the failure that hides.
            self.slots.resize(i + 1, String::new());
        }
        self.slots[i] = stack.to_string();
    }
}

struct PendingClick {
    id: u64,
    window_id: i32,
    slot: i32,
    /// what the slot held before the click, purely so the outcome can say what moved
    before: String,
    sent: Instant,
    /// corrections the server has sent for this window since
    corrections: u32,
}

/// How long to collect the server's corrections after a click before reporting the outcome.
///
/// A click that changes nothing produces NO packets at all -- clicking an empty slot with an empty
/// hand is a legitimate no-op -- so there is no "the click landed" acknowledgement to wait for. The
/// window exists to collect what did change, and its expiry is a normal ending, not a failure.
const CLICK_SETTLE: Duration = Duration::from_millis(600);

struct PendingOpen {
    id: u64,
    pos: (i32, i32, i32),
    sent: Instant,
}

/// How long to wait for `open_screen` after clicking a block before calling the request failed.
///
/// Not a guess at latency: the point is that a *silent* failure is the normal one. The server drops
/// an out-of-reach or wrong-block interaction without telling the client anything, so "no screen
/// arrived" is the only signal there is, and a request has to be able to end.
const OPEN_TIMEOUT: Duration = Duration::from_millis(1500);

/// How often the play loop wakes when it has nothing pending, so a queued `bot_command` does not sit
/// behind the next inbound packet. The 90s dead-connection check is now kept separately (see
/// `last_packet`) rather than riding on this timeout.
const IDLE_TICK: Duration = Duration::from_millis(200);

struct MonitorSlot {
    pending: Option<MonitorData>,
    /// what was last handed to the publisher, for the "did anything actually change?" check
    published: Option<MonitorData>,
    last_sent: Instant,
}

/// Same reason as `MetaStats`: the whole risk of this feature is write volume, so it is counted.
struct MonitorStats {
    payloads: u64,
    /// payloads whose screen was byte-identical to the one already published
    unchanged: u64,
    /// changes folded into a later send by the rate limit, costing nothing
    coalesced: u64,
    sent: u64,
    failed: u64,
    /// largest row published, in bytes of text + colour grids
    max_bytes: usize,
    bytes: u64,
}

impl Default for MonitorStats {
    fn default() -> Self {
        MonitorStats { payloads: 0, unchanged: 0, coalesced: 0, sent: 0, failed: 0, max_bytes: 0, bytes: 0 }
    }
}

struct MetaState {
    fields: std::collections::BTreeMap<u8, serde_json::Value>,
    last_sent: Instant,
    dirty: bool,
}

/// Measurement, not decoration: this feature's whole risk is write volume on a table that already
/// updates several times a second per entity, so the cost is counted rather than guessed.
struct MetaStats {
    started: Instant,
    packets: u64,
    /// packets that actually changed a carried field, i.e. that cost a reducer call
    changed: u64,
    truncated: u64,
    /// changes that were folded into a later send by the rate limit and so cost nothing
    coalesced: u64,
    /// reducer calls actually made
    sent: u64,
    /// type name -> (rows seen, max field count, max json bytes, sum json bytes, sample)
    by_type: HashMap<String, (u64, usize, usize, u64, String)>,
    /// item-stack data components seen, by name, and how many of those had no codec. This is the
    /// census that says how big the item-component job actually is on THIS pack -- there is no way
    /// to know it from the jar, because which components appear depends on what is in the world.
    components_seen: HashMap<String, u64>,
    components_unknown: HashMap<String, u64>,
    stacks: u64,
}

impl Default for MetaStats {
    fn default() -> Self {
        MetaStats { started: Instant::now(), packets: 0, changed: 0, truncated: 0, coalesced: 0, sent: 0, by_type: HashMap::new(), components_seen: HashMap::new(), components_unknown: HashMap::new(), stacks: 0 }
    }
}

/// A metadata packet that changes a carried field costs a reducer call and a row rewrite, and the
/// wire will happily produce one per tick per entity: a drowning axolotl re-sends its air-supply
/// counter every tick, which is 20 writes a second carrying no appearance at all (measured -- two
/// axolotls were a third of all metadata traffic in a test world of twenty mobs).
///
/// So an entity's blob is sent at most this often. The FIRST change after a quiet period goes
/// immediately, which is the one that matters -- an entity's full non-default set arrives when it
/// comes into view -- and only a stream of changes gets folded together. Rate limiting is used
/// rather than an index blocklist on purpose: "index 1 is air supply" is exactly the
/// version-and-mod-specific knowledge this proxy refuses to encode, whereas "do not rewrite the
/// same row twenty times a second" is true regardless of what the pack does.
const META_MIN_INTERVAL: Duration = Duration::from_millis(500);
/// How often the deferred set is swept. Cheap: the sweep only runs when something is deferred.
const META_FLUSH_TICK: Duration = Duration::from_millis(100);

/// Same argument as `META_MIN_INTERVAL`, with more at stake: a monitor row carries the whole screen
/// (a 7x5-block advanced monitor is 71x40 characters, so ~8.5 KB of text and colour grids), and
/// CC:T will happily emit one payload per monitor per tick -- 20 a second per screen, before you
/// count the several screens a base has. `computercraft:monitor_client` was the single loudest mod
/// channel on this server when it was being dropped on the floor: 1377 payloads a minute.
///
/// So a monitor's row is rewritten at most this often. The first change after a quiet period goes
/// immediately, so a screen that updates once and stops is still live within a frame.
const MONITOR_MIN_INTERVAL: Duration = Duration::from_millis(500);

fn offline_uuid(name: &str) -> uuid::Uuid {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(format!("OfflinePlayer:{name}").as_bytes());
    let mut b: [u8; 16] = h.finalize().into();
    b[6] = (b[6] & 0x0f) | 0x30; // version 3
    b[8] = (b[8] & 0x3f) | 0x80; // variant
    uuid::Uuid::from_bytes(b)
}

fn angle(b: i8) -> f32 {
    (b as f32) * 360.0 / 256.0
}

pub async fn run_session(cfg: &Config, ctx: &mut Ctx) -> SessionEnd {
    match run_session_inner(cfg, ctx).await {
        Ok(end) => end,
        Err(e) => SessionEnd::Error(e),
    }
}

async fn write_raw(wr: &mut RawWriteConnection, frame: Vec<u8>) -> Result<()> {
    wr.write(&frame).await.context("write")
}

fn status(ctx: &Ctx, cfg: &Config, state: &str, detail: &str, play: Option<&Play>) {
    let (dimension, pos, vd, chunks, packets) = match play {
        Some(p) => (p.dimension.clone(), p.bot_pos, p.view_distance, p.world.chunks.len() as u32, p.packets),
        None => (String::new(), (0.0, 0.0, 0.0), 0, 0, 0),
    };
    let _ = ctx.tx.send(Update::BotStatus {
        server_addr: cfg.addr.clone(),
        username: cfg.username.clone(),
        state: state.into(),
        detail: detail.into(),
        dimension,
        pos,
        view_distance: vd,
        chunks_loaded: chunks,
        packets_seen: packets,
    });
}

async fn run_session_inner(cfg: &Config, ctx: &mut Ctx) -> Result<SessionEnd> {
    status(ctx, cfg, "connecting", "", None);
    let server_addr = ServerAddress::try_from(cfg.addr.as_str()).map_err(|e| anyhow!("bad address: {e}"))?;
    let resolved = azalea_protocol::resolver::resolve_address(&server_addr).await.map_err(|e| anyhow!("resolve: {e}"))?;
    info!("connecting to {} ({resolved})", cfg.addr);
    let mut conn = Connection::new(&resolved).await.map_err(|e| anyhow!("connect: {e}"))?;
    conn.write(
        ClientIntentionPacket {
            protocol_version: PROTOCOL_VERSION,
            hostname: server_addr.host.clone(),
            port: server_addr.port,
            intention: ClientIntention::Login,
        }
        .get(),
    )
    .await?;
    let mut conn = conn.login();

    // ── login ──
    // THE NAME IN LOGIN START MUST BE THE AUTHENTICATED PROFILE'S NAME.
    //
    // An online-mode server looks the session up by the name it was given here, not by the UUID --
    // so sending MC_USERNAME while holding a token for someone else fails `hasJoinedServer` and the
    // kick is the maximally unhelpful "Failed to verify username!". The UUID was already taken from
    // the profile; the name was not, and the two disagreeing is invisible until the server says no.
    // Measured against the live server: authenticated as Powback, announced as "spacetime" (the
    // MC_USERNAME default), rejected every time (2026-09-12).
    //
    // MC_USERNAME still names an OFFLINE bot, where there is no profile to take a name from and the
    // name is the only identity there is.
    let (uuid, access_token, username) = match &cfg.auth {
        Auth::Offline => (offline_uuid(&cfg.username), None, cfg.username.clone()),
        Auth::Microsoft { email, cache_file } => {
            info!("Microsoft auth for {email} (cache {})", cache_file.display());
            let res = azalea_auth::auth(
                email,
                azalea_auth::AuthOpts { check_ownership: false, cache_file: Some(cache_file.clone()), client_id: None, scope: None },
            )
            .await
            .map_err(|e| anyhow!("microsoft auth: {e}"))?;
            info!("authenticated as {} ({})", res.profile.name, res.profile.id);
            (res.profile.id, Some(res.access_token), res.profile.name)
        }
    };
    status(ctx, cfg, "login", "", None);
    conn.write(ServerboundHelloPacket { name: username.clone(), profile_id: uuid }.get()).await?;
    let profile = loop {
        let packet = match conn.read().await {
            Ok(p) => p,
            Err(e) => return Ok(SessionEnd::Disconnected(format!("during login: {e}"))),
        };
        match packet {
            ClientboundLoginPacket::Hello(p) => {
                let e = azalea_crypto::encrypt(&p.public_key, &p.challenge).map_err(|e| anyhow!("encrypt: {e}"))?;
                if p.should_authenticate {
                    match &access_token {
                        Some(tok) => {
                            azalea_auth::sessionserver::join(tok, &p.public_key, &e.secret_key, &uuid, &p.server_id)
                                .await
                                .map_err(|e| anyhow!("sessionserver join: {e}"))?;
                        }
                        None => {
                            warn!("server is online-mode (should_authenticate=true) but we have no Microsoft token; the server will reject us with 'Failed to verify username'");
                        }
                    }
                }
                conn.write(ServerboundKeyPacket { key_bytes: e.encrypted_public_key, encrypted_challenge: e.encrypted_challenge }.get()).await?;
                conn.set_encryption_key(e.secret_key);
            }
            ClientboundLoginPacket::LoginCompression(p) => conn.set_compression_threshold(p.compression_threshold),
            ClientboundLoginPacket::GameProfile(p) => break p.game_profile,
            ClientboundLoginPacket::LoginDisconnect(p) => {
                return Ok(SessionEnd::Disconnected(format!("login: {}", p.reason.to_string())));
            }
            ClientboundLoginPacket::CustomQuery(p) => {
                // Forge-style login plugin messages (not used by NeoForge 21.1, but answer anyway)
                debug!("login custom query {} ({} bytes)", p.identifier, p.data.len());
                conn.write(ServerboundCustomQueryAnswerPacket { transaction_id: p.transaction_id, data: None }.get()).await?;
            }
            ClientboundLoginPacket::CookieRequest(p) => {
                let mut w = Writer::with_id(0x04);
                w.string(&p.key.to_string());
                w.bool(false);
                conn.writer.raw.write(&w.into_inner()).await?;
            }
        }
    };
    info!("logged in as {} ({})", profile.name, profile.uuid);
    conn.write(ServerboundLoginAcknowledgedPacket {}.get()).await?;
    let (rd, wr) = conn.into_split();
    let mut rd = rd.raw;
    let mut wr = wr.raw;

    let mut play = Play {
        dimension: String::new(),
        min_section: -4,
        section_count: 24,
        world: World::new(),
        entities: HashMap::new(),
        players: HashMap::new(),
        bot_pos: (0.0, 0.0, 0.0),
        view_distance: 0,
        packets: 0,
        last_status: Instant::now(),
        biome_direct_bits: 6,
        dim_types: HashMap::new(),
        dim_type_ids: vec![],
        enchantment_names: vec![],
        banner_pattern_names: vec![],
        entity_meta: HashMap::new(),
        meta_dirty: std::collections::HashSet::new(),
        last_meta_flush: Instant::now(),
        meta_stats: MetaStats::default(),
        monitors: HashMap::new(),
        monitor_dirty: std::collections::HashSet::new(),
        monitor_geom: HashMap::new(),
        monitor_stats: MonitorStats::default(),
        equipment: HashMap::new(),
        containers: HashMap::new(),
        sequence: 0,
        pending_open: None,
        pending_click: None,
        walking: None,
        // Assume full health until the server says otherwise. It sends `set_health` on join, so this
        // is only the value for the first few milliseconds -- and erring high here is safe, because the
        // fall check that reads it only ever makes the bot MORE cautious than this default.
        health: 20.0,
        respawn_asked: None,
        bot_look: (0.0, 0.0),
    };

    loop {
        // ── configuration ──
        status(ctx, cfg, "configuration", "", None);
        match configuration(cfg, ctx, &mut rd, &mut wr, &mut play).await? {
            Some(end) => return Ok(end),
            None => {}
        }
        // ── play ──
        status(ctx, cfg, "play", "", Some(&play));
        match play_loop(cfg, ctx, &mut rd, &mut wr, &mut play).await? {
            PlayExit::Reconfigure => continue,
            PlayExit::End(end) => return Ok(end),
        }
    }
}

// ───────────────────────────── configuration ─────────────────────────────

async fn configuration(cfg: &Config, ctx: &mut Ctx, rd: &mut RawReadConnection, wr: &mut RawWriteConnection, play: &mut Play) -> Result<Option<SessionEnd>> {
    let complete = ctx.channels.configuration_complete;
    ctx.channels.set_config_passed(complete);
    // client information
    {
        let mut w = Writer::with_id(0x00);
        w.string("en_us");
        w.u8(cfg.view_distance);
        w.varint(0); // chat visibility: full
        w.bool(true); // chat colors
        w.u8(0x7f); // skin parts
        w.varint(1); // main hand: right
        w.bool(false); // text filtering
        w.bool(true); // allow server listings
        write_raw(wr, w.into_inner()).await?;
    }
    let mut negotiation_failed: Option<(Vec<String>, bool)> = None;
    let mut frozen_expected: Vec<String> = vec![];
    let mut biome_count = 0usize;
    let mut biome_names: Vec<String> = Vec::new();
    let started = Instant::now();
    let mut last_packet = Instant::now();
    loop {
        let frame = match tokio::time::timeout(Duration::from_secs(60), rd.read()).await {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => {
                if let Some((summary, changed)) = negotiation_failed {
                    return Ok(Some(if changed { SessionEnd::NegotiationProgress(summary) } else { SessionEnd::NegotiationStuck(summary) }));
                }
                return Ok(Some(SessionEnd::Disconnected(format!("configuration read: {e}"))));
            }
            Err(_) => {
                bail!("configuration phase stalled: no packet for 60s (started {:?} ago, last {:?} ago)", started.elapsed(), last_packet.elapsed());
            }
        };
        last_packet = Instant::now();
        play.packets += 1;
        let mut r = Reader::new(&frame);
        let id = r.varint()?;
        match id {
            0x00 => {
                // cookie request
                let key = r.string()?;
                let mut w = Writer::with_id(0x01);
                w.string(&key);
                w.bool(false);
                write_raw(wr, w.into_inner()).await?;
            }
            0x01 => {
                let channel = r.string()?;
                let body = r.rest();
                trace!("config payload {channel} ({} bytes)", body.len());
                match channel.as_str() {
                    neoforge::CH_QUERY => {
                        // Announce ad-hoc channels (Fabric-API configuration tasks) BEFORE the query
                        // reply, so NeoForge's ad-hoc set holds them before the query triggers
                        // initializeNeoForgeConnection and the tasks begin.
                        if !ctx.channels.adhoc.is_empty() {
                            let mut w = Writer::with_id(0x02);
                            w.string("minecraft:register");
                            w.raw(&neoforge::encode_minecraft_register(&ctx.channels.adhoc));
                            write_raw(wr, w.into_inner()).await?;
                            info!("announced {} ad-hoc channels via minecraft:register: {:?}", ctx.channels.adhoc.len(), ctx.channels.adhoc);
                        }
                        let q = ctx.channels.encode_query();
                        info!(
                            "NeoForge query received; answering with {} configuration + {} play channels",
                            ctx.channels.configuration.len(),
                            ctx.channels.play.len()
                        );
                        let mut w = Writer::with_id(0x02);
                        w.string(neoforge::CH_QUERY);
                        w.raw(&q);
                        write_raw(wr, w.into_inner()).await?;
                    }
                    neoforge::CH_NETWORK => {
                        let setup = ctx.channels.parse_network(body)?;
                        let cfgn = setup.get(&neoforge::PROTO_CONFIG).map(|m| m.len()).unwrap_or(0);
                        let playn = setup.get(&neoforge::PROTO_PLAY).map(|m| m.len()).unwrap_or(0);
                        info!("NeoForge negotiation SUCCEEDED: {cfgn} configuration + {playn} play channels in the setup");
                        ctx.channels.save(&cfg.channels_path);
                    }
                    neoforge::CH_SETUP_FAILED => {
                        let (summary, changed) = ctx.channels.learn_from_failure(body)?;
                        warn!("NeoForge negotiation failed with {} reasons (learned something: {changed})", summary.len());
                        for s in summary.iter().take(8) {
                            warn!("  {s}");
                        }
                        if changed {
                            ctx.channels.save(&cfg.channels_path);
                        }
                        negotiation_failed = Some((summary, changed));
                    }
                    neoforge::CH_FROZEN_START => {
                        frozen_expected = neoforge::parse_frozen_start(body)?;
                        info!("frozen registry sync: {} registries", frozen_expected.len());
                    }
                    neoforge::CH_FROZEN => {
                        let (name, ids) = neoforge::parse_frozen_registry(body)?;
                        debug!("registry {name}: {} entries", ids.len());
                        ctx.registries.insert(name, ids.into_iter().collect());
                    }
                    neoforge::CH_FROZEN_DONE => {
                        info!("frozen registry sync complete ({} registries received)", ctx.registries.len());
                        let mut w = Writer::with_id(0x02);
                        w.string(neoforge::CH_FROZEN_DONE);
                        write_raw(wr, w.into_inner()).await?;
                        reconcile_registries(ctx);
                    }
                    neoforge::CH_DATAMAPS => {
                        let mut w = Writer::with_id(0x02);
                        w.string(neoforge::CH_DATAMAPS_REPLY);
                        w.varint(0);
                        write_raw(wr, w.into_inner()).await?;
                    }
                    neoforge::CH_ENUMS => {
                        let mut w = Writer::with_id(0x02);
                        w.string(neoforge::CH_ENUMS_ACK);
                        write_raw(wr, w.into_inner()).await?;
                    }
                    neoforge::CH_FLAGS => {
                        let mut w = Writer::with_id(0x02);
                        w.string(neoforge::CH_FLAGS_ACK);
                        write_raw(wr, w.into_inner()).await?;
                    }
                    "minecraft:brand" | "minecraft:register" | "minecraft:unregister" => {}
                    other => {
                        // A Fabric-API configuration task's payload (e.g. spell_engine:config_sync):
                        // reply on the namespace ack channel so the server completes the task.
                        if let Some((ns, _)) = other.split_once(':') {
                            let ack = format!("{ns}:ack");
                            let known = ctx.channels.adhoc.iter().any(|c| c == &ack)
                                || ctx.channels.configuration.contains_key(&ack)
                                || ctx.channels.play.contains_key(&ack);
                            if known {
                                // Ack{code:String} carries the TASK id so the server can
                                // completeTask(Type(code)). The task id is the payload id minus
                                // the Fabric "_sync" suffix (spell_engine:config_sync -> :config).
                                let code = other.strip_suffix("_sync").unwrap_or(other);
                                let mut w = Writer::with_id(0x02);
                                w.string(&ack);
                                w.string(code);
                                write_raw(wr, w.into_inner()).await?;
                                info!("acked Fabric config task {code} (payload {other}) on {ack}");
                            } else {
                                debug!("unhandled configuration payload {other} ({} bytes)", body.len());
                            }
                        } else {
                            debug!("unhandled configuration payload {other} ({} bytes)", body.len());
                        }
                    }
                }
            }
            0x02 => {
                let reason = component_to_text(r.rest());
                if let Some((summary, changed)) = negotiation_failed {
                    info!("disconnected after negotiation failure: {reason}");
                    return Ok(Some(if changed { SessionEnd::NegotiationProgress(summary) } else { SessionEnd::NegotiationStuck(summary) }));
                }
                if let Some(id) = neoforge::unsupported_task_channel(&reason) {
                    // The kick names the TASK id; the payload the task must send has a related id
                    // (Fabric convention: <task>_sync). Register both plus the namespace ack
                    // channel ad hoc so NeoForge's hasChannel() lets the task run, and we can
                    // reply. See neoforge::adhoc_candidates.
                    let cands = neoforge::adhoc_candidates(&id);
                    let added: Vec<String> = cands.into_iter().filter(|c| !ctx.channels.adhoc.contains(c)).collect();
                    if !added.is_empty() {
                        warn!("server runs a Fabric-API configuration task on {id}; registering {added:?} ad hoc and reconnecting");
                        ctx.channels.adhoc.extend(added.iter().cloned());
                        ctx.channels.save(&cfg.channels_path);
                        return Ok(Some(SessionEnd::NegotiationProgress(vec![format!("adhoc {id}")])));
                    }
                }
                return Ok(Some(SessionEnd::Disconnected(format!("configuration: {reason}"))));
            }
            0x03 => {
                write_raw(wr, Writer::with_id(0x03).into_inner()).await?;
                info!("configuration finished after {:?}", started.elapsed());
                if !ctx.channels.verified {
                    ctx.channels.verified = true;
                    ctx.channels.save(&cfg.channels_path);
                }
                if biome_count > 0 {
                    play.biome_direct_bits = ((usize::BITS - (biome_count.max(2) - 1).leading_zeros()) as u8).max(1);
                }
                // The world downloader needs both id tables before the first chunk, and this is
                // the one point where both are final: block states are re-flattened during the
                // frozen registry sync, biomes arrive as registry data, and chunks only start
                // after this packet.
                if let Some(tx) = &ctx.saver {
                    let _ = tx.send(anvil::Msg::Tables { blocks: ctx.block_states.table.clone(), biomes: std::sync::Arc::new(std::mem::take(&mut biome_names)) });
                }
                return Ok(None);
            }
            0x04 => {
                let v = r.i64()?;
                let mut w = Writer::with_id(0x04);
                w.i64(v);
                write_raw(wr, w.into_inner()).await?;
            }
            0x05 => {
                let v = r.i32()?;
                let mut w = Writer::with_id(0x05);
                w.i32(v);
                write_raw(wr, w.into_inner()).await?;
            }
            0x07 => {
                // registry data
                let registry = r.string()?;
                let n = r.varint()?;
                trace!("registry data {registry}: {n} entries, {} bytes remain", r.remaining());
                let mut names = Vec::with_capacity(n.max(0) as usize);
                for ei in 0..n {
                    let name = r.string()?;
                    let has = r.bool()?;
                    if has {
                        // Root may be any tag type on a modded server (spell_engine's spell
                        // registry uses a byte-array-of-JSON root), so read generically.
                        let v = r.nbt_any().with_context(|| format!("registry {registry} entry {ei} ({name})"))?;
                        if registry == "minecraft:dimension_type" {
                            let min_y = v.get("min_y").and_then(|x| x.as_i64()).unwrap_or(-64) as i32;
                            let height = v.get("height").and_then(|x| x.as_i64()).unwrap_or(384) as i32;
                            play.dim_types.insert(name.clone(), (min_y, height));
                        }
                    }
                    names.push(name);
                }
                debug!("registry data {registry}: {} entries", names.len());
                if registry == "minecraft:worldgen/biome" {
                    biome_count = names.len();
                    let defs: Vec<RegistryDef> = names.iter().enumerate().map(|(i, n)| RegistryDef { registry: "biome".into(), id: i as u32, name: n.clone() }).collect();
                    let _ = ctx.tx.send(Update::Registry(defs));
                    // Anvil stores biomes by NAME, and this ordered list is the only place the id
                    // -> name mapping exists: biomes are a datapack registry, so they never appear
                    // in the NeoForge frozen sync that `ctx.registries` is built from.
                    biome_names = names;
                } else if registry == "minecraft:dimension_type" {
                    play.dim_type_ids = names;
                } else if registry == "minecraft:enchantment" {
                    // Item stacks name enchantments by index into this ordered list, and nothing
                    // else carries it: enchantments are a datapack registry since 1.21, so they are
                    // absent from the frozen sync that `ctx.registries` is built from.
                    let defs: Vec<RegistryDef> = names.iter().enumerate().map(|(i, n)| RegistryDef { registry: "enchantment".into(), id: i as u32, name: n.clone() }).collect();
                    let _ = ctx.tx.send(Update::Registry(defs));
                    play.enchantment_names = names;
                } else if registry == "minecraft:banner_pattern" {
                    play.banner_pattern_names = names;
                }
            }
            0x09 => {
                // resource pack push: uuid, url, hash, forced, prompt
                let uuid_hi = r.u64()?;
                let uuid_lo = r.u64()?;
                let url = r.string()?;
                warn!("server pushes a resource pack ({url}); accepting and reporting loaded");
                for st in [3i32, 0i32] {
                    let mut w = Writer::with_id(0x06);
                    w.i64(uuid_hi as i64);
                    w.i64(uuid_lo as i64);
                    w.varint(st);
                    write_raw(wr, w.into_inner()).await?;
                }
            }
            0x0e => {
                // select known packs -> we know none, so the server sends everything
                let mut w = Writer::with_id(0x07);
                w.varint(0);
                write_raw(wr, w.into_inner()).await?;
            }
            0x06 | 0x08 | 0x0a | 0x0b | 0x0c | 0x0d => {}
            other => debug!("unhandled configuration packet 0x{other:02x}"),
        }
    }
}

/// After the frozen registry sync: publish id->name tables and re-flatten block states.
fn reconcile_registries(ctx: &mut Ctx) {
    let mut defs = vec![];
    // `data_component_type` is here because an item stack's components are numbered on the wire by
    // this registry and the numbering is PACK-SPECIFIC: 327 types on this server against vanilla's
    // ~100, so any hardcoded id table would be wrong. itemstack.rs dispatches on the NAME for the
    // same reason the metadata walk refuses to name indices, and a consumer reading a stack's
    // components needs the same mapping.
    for (reg, short) in [
        ("minecraft:block", "block"),
        ("minecraft:block_entity_type", "block_entity_type"),
        ("minecraft:entity_type", "entity_type"),
        ("minecraft:item", "item"),
        ("minecraft:data_component_type", "data_component_type"),
    ] {
        if let Some(m) = ctx.registries.get(reg) {
            for (id, name) in m {
                defs.push(RegistryDef { registry: short.into(), id: *id, name: name.clone() });
            }
        }
    }
    for chunk in defs.chunks(5000) {
        let _ = ctx.tx.send(Update::Registry(chunk.to_vec()));
    }
    if let Some(blocks) = ctx.registries.get("minecraft:block") {
        let live: Vec<(u32, String)> = blocks.iter().map(|(i, n)| (*i, n.clone())).collect();
        if ctx.block_states.matches_registry(&live) {
            info!("live block registry ({} blocks) matches the dump exactly; {} block states", live.len(), ctx.block_states.count());
        } else {
            warn!("live block registry differs from the dump ({} live vs {} dumped blocks); re-flattening state ids in live order", live.len(), ctx.block_states.per_block.len());
            ctx.block_states.flatten(Some(&live));
        }
    } else {
        warn!("no minecraft:block registry received; using the dump's order for block-state ids");
    }
    if !ctx.block_states_published {
        ctx.block_states_published = true;
        let table = &ctx.block_states.table;
        info!("publishing {} block states", table.len());
        let mut batch = Vec::with_capacity(4000);
        for (i, (name, props)) in table.iter().enumerate() {
            batch.push(BlockStateDef { id: i as u32, name: name.clone(), properties: props.clone() });
            if batch.len() == 4000 {
                let _ = ctx.tx.send(Update::BlockStates(std::mem::take(&mut batch)));
                batch.reserve(4000);
            }
        }
        if !batch.is_empty() {
            let _ = ctx.tx.send(Update::BlockStates(batch));
        }
    }
}

// ───────────────────────────── play ─────────────────────────────

enum PlayExit {
    Reconfigure,
    End(SessionEnd),
}

fn registry_name(ctx: &Ctx, reg: &str, id: u32) -> String {
    ctx.registries.get(reg).and_then(|m| m.get(&id)).cloned().unwrap_or_else(|| format!("{reg}#{id}"))
}

/// SKY AND BLOCK LIGHT, WHICH NOTHING HERE HAS EVER READ.
///
/// `ClientboundLevelChunkWithLightPacket` carries light immediately after the block entities, and
/// this parser simply stopped at the block entities and let the rest fall off the end of the buffer.
/// So the mirrored world had no light at all and every consumer had to render it fully lit -- fine
/// on a top-down map, wrong the moment anyone looks at a cave.
///
/// The layout is four BitSets then two arrays of 2048-byte nibble blocks:
///   sky mask, block mask, empty-sky mask, empty-block mask, then sky arrays, then block arrays.
/// A mask bit is set per section INCLUDING the one below the world and the one above it, so bit i
/// means section `min_section - 1 + i`. "Empty" means uniformly zero and is sent with no array --
/// that is a real value (a sealed cave is dark), not an absence, so it is recorded as all-zero
/// rather than skipped.
///
/// Arrays appear in bit order, so the count of set bits before bit i is that array's index.
fn read_light(r: &mut Reader, min_section: i32) -> Result<Vec<(i32, Vec<u8>, Vec<u8>)>> {
    fn bitset(r: &mut Reader) -> Result<Vec<i64>> {
        let n = r.varint()?.max(0) as usize;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n { v.push(r.i64()?); }
        Ok(v)
    }
    fn has(mask: &[i64], i: usize) -> bool {
        mask.get(i / 64).map(|w| (*w >> (i % 64)) & 1 == 1).unwrap_or(false)
    }
    let sky_mask = bitset(r)?;
    let block_mask = bitset(r)?;
    let empty_sky = bitset(r)?;
    let empty_block = bitset(r)?;
    let mut sky: Vec<Vec<u8>> = Vec::new();
    let n = r.varint()?.max(0) as usize;
    for _ in 0..n { sky.push(r.byte_array()?.to_vec()); }
    let mut block: Vec<Vec<u8>> = Vec::new();
    let n = r.varint()?.max(0) as usize;
    for _ in 0..n { block.push(r.byte_array()?.to_vec()); }

    // Widest of the four masks: a section may be lit in one and empty in the other.
    let bits = 64 * sky_mask.len().max(block_mask.len()).max(empty_sky.len()).max(empty_block.len());
    let mut out = Vec::new();
    let (mut si, mut bi) = (0usize, 0usize);
    for i in 0..bits {
        let s = if has(&sky_mask, i) { let v = sky.get(si).cloned().unwrap_or_default(); si += 1; v }
                else if has(&empty_sky, i) { vec![0u8; 2048] } else { Vec::new() };
        let b = if has(&block_mask, i) { let v = block.get(bi).cloned().unwrap_or_default(); bi += 1; v }
                else if has(&empty_block, i) { vec![0u8; 2048] } else { Vec::new() };
        if !s.is_empty() || !b.is_empty() {
            out.push((min_section - 1 + i as i32, s, b));
        }
    }
    Ok(out)
}

fn section_data(s: &Section, sy: i32) -> SectionData {
    let (bb, bp, bd) = s.blocks.export();
    let (ib, ip, idata) = s.biomes.export();
    // Light is filled in by the chunk handler when the packet carries it. A section the server said
    // nothing about keeps these empty, which consumers must read as "unknown", never as "dark".
    SectionData { sy, non_air_count: s.non_air, block_bits: bb, block_palette: bp, block_data: bd, biome_bits: ib, biome_palette: ip, biome_data: idata, sky_light: Vec::new(), block_light: Vec::new() }
}

/// Hand one full chunk to the world downloader.
///
/// Everything here is a CLONE of data already built for the SpacetimeDB mirror, sent over a
/// bounded channel with `try_send`. The protocol loop must never block on the saver: a full
/// channel means the disk is behind, and dropping a chunk from a backup is recoverable by
/// revisiting it, whereas stalling this loop throttles the chunk stream and the mirror with it.
fn save_chunk(ctx: &mut Ctx, play: &Play, cx: i32, cz: i32, heightmaps: simdnbt::owned::Nbt, sections: &[SectionData], edge_light: &[(i32, Vec<u8>, Vec<u8>)], bes: &[BlockEntityData]) {
    // Chunks from another level (the Aeronautics airship level at 1280064,1280064 -- see the 0x27
    // handler) are inside the world border and would be filed as overworld terrain. Distance from
    // the bot is what separates them, and this is the only place the bot's position is known.
    if !anvil::plausible(cx, cz, (play.bot_pos.0 as i32) >> 4, (play.bot_pos.2 as i32) >> 4) {
        ctx.save_dropped += 1;
        warn!("world download: refusing chunk {cx},{cz} -- not in the level the bot is in (bot at {:.0},{:.0})", play.bot_pos.0, play.bot_pos.2);
        return;
    }
    let packed = |bits: u8, palette: &Vec<u32>, data: &Vec<u64>| anvil::Packed { bits, palette: palette.clone(), data: data.clone() };
    let mut secs: Vec<anvil::SectionSave> = sections
        .iter()
        .map(|s| anvil::SectionSave {
            sy: s.sy,
            blocks: Some(packed(s.block_bits, &s.block_palette, &s.block_data)),
            biomes: Some(packed(s.biome_bits, &s.biome_palette, &s.biome_data)),
            sky_light: s.sky_light.clone(),
            block_light: s.block_light.clone(),
        })
        .collect();
    for (sy, sky, blk) in edge_light {
        secs.push(anvil::SectionSave { sy: *sy, blocks: None, biomes: None, sky_light: sky.clone(), block_light: blk.clone() });
    }
    let job = anvil::ChunkSave {
        dimension: play.dimension.clone(),
        cx,
        cz,
        min_section: play.min_section,
        heightmaps: match heightmaps {
            simdnbt::owned::Nbt::Some(base) => base.into_inner(),
            simdnbt::owned::Nbt::None => simdnbt::owned::NbtCompound::new(),
        },
        sections: secs,
        block_entities: bes.iter().map(|b| anvil::BlockEntitySave { x: b.x, y: b.y, z: b.z, id: b.type_name.clone(), raw: b.nbt.clone() }).collect(),
    };
    if let Some(tx) = &ctx.saver {
        if tx.try_send(anvil::Msg::Chunk(Box::new(job))).is_err() {
            ctx.save_dropped += 1;
            if ctx.save_dropped % 64 == 1 {
                warn!("world download is behind; {} chunks dropped so far", ctx.save_dropped);
            }
        }
    }
}

fn block_entity_data(ctx: &Ctx, play: &Play, x: i32, y: i32, z: i32, type_id: u32, nbt: &simdnbt::owned::Nbt, raw: &[u8]) -> BlockEntityData {
    let type_name = registry_name(ctx, "minecraft:block_entity_type", type_id);
    let block_state_id = play.world.get_block(x, y, z).unwrap_or(0);
    let (json, computer_id, label, fuel, on, left, right) = match nbt {
        simdnbt::owned::Nbt::Some(base) => {
            let c: &simdnbt::owned::NbtCompound = base;
            let upgrade = |key: &str| -> Option<String> {
                match c.get(key) {
                    Some(simdnbt::owned::NbtTag::String(s)) => Some(s.to_str().into_owned()),
                    Some(simdnbt::owned::NbtTag::Compound(u)) => u.string("id").map(|s| s.to_str().into_owned()).or_else(|| Some(nbt_compound_to_json(&u).to_string())),
                    _ => None,
                }
            };
            (
                nbt_compound_to_json(c).to_string(),
                c.int("ComputerId"),
                c.string("Label").map(|s| s.to_str().into_owned()),
                c.int("Fuel"),
                c.byte("On").map(|b| b != 0),
                upgrade("LeftUpgrade"),
                upgrade("RightUpgrade"),
            )
        }
        simdnbt::owned::Nbt::None => ("{}".to_string(), None, None, None, None, None, None),
    };
    BlockEntityData { x, y, z, type_id, type_name, block_state_id, nbt: raw.to_vec(), nbt_json: json, computer_id, label, fuel, on, left_upgrade: left, right_upgrade: right }
}

fn entity_data(id: i32, e: &EntityInfo, data: i32, vel: (f64, f64, f64)) -> EntityData {
    EntityData { id, uuid: e.uuid.clone(), type_id: e.type_id, type_name: e.type_name.clone(), x: e.x, y: e.y, z: e.z, yaw: e.yaw, pitch: e.pitch, head_yaw: e.head_yaw, vx: vel.0, vy: vel.1, vz: vel.2, data }
}

fn player_data(uuid: &str, p: &PlayerInfo) -> PlayerData {
    PlayerData { uuid: uuid.to_string(), name: p.name.clone(), entity_id: p.entity_id, online: true, gamemode: p.gamemode, latency: p.latency, x: p.pos.0, y: p.pos.1, z: p.pos.2, yaw: p.rot.0, pitch: p.rot.1 }
}

fn apply_dimension(play: &mut Play, dim_type_id: i32, dimension: String) {
    let tname = play.dim_type_ids.get(dim_type_id as usize).cloned().unwrap_or_default();
    let (min_y, height) = play.dim_types.get(&tname).copied().unwrap_or((-64, 384));
    play.min_section = min_y >> 4;
    play.section_count = (height >> 4) as usize;
    info!("dimension {dimension} (type {tname}): min_y {min_y} height {height} => sections {}..{}", play.min_section, play.min_section + play.section_count as i32);
    play.dimension = dimension;
}

/// Read CommonPlayerSpawnInfo; returns (dimension_type id, dimension name)
fn read_spawn_info(r: &mut Reader) -> Result<(i32, String)> {
    let dim_type = r.varint()?;
    let dimension = r.string()?;
    let _seed = r.i64()?;
    let _game_type = r.u8()?;
    let _prev = r.i8()?;
    let _debug = r.bool()?;
    let _flat = r.bool()?;
    if r.bool()? {
        let _dim = r.string()?;
        let _pos = r.i64()?;
    }
    let _portal_cooldown = r.varint()?;
    Ok((dim_type, dimension))
}

async fn play_loop(cfg: &Config, ctx: &mut Ctx, rd: &mut RawReadConnection, wr: &mut RawWriteConnection, play: &mut Play) -> Result<PlayExit> {
    let mut payload_counts: HashMap<String, u64> = HashMap::new();
    let mut unknown_counts: HashMap<i32, u64> = HashMap::new();
    let mut last_summary = Instant::now();
    let mut last_packet = Instant::now();

    // A request made while the bot was down was never going to run, and running it now would aim it
    // at a block the bot may be nowhere near. Expire them in the database FIRST and then drain
    // whatever the subscription already handed us -- in that order, because the initial subscription
    // delivers existing rows as inserts, so the channel can already hold them. A command that
    // arrives after the expire is genuinely new and survives.
    let _ = ctx.tx.send(Update::ExpirePendingCommands);
    let mut drained = 0;
    while ctx.commands.try_recv().is_ok() {
        drained += 1;
    }
    if drained > 0 {
        info!("discarded {drained} bot command(s) queued while the bot was not in play");
    }
    loop {
        // While an entity's blob is waiting out its rate limit, wake up often enough to send it.
        // Without this the flush would only ever run when the NEXT packet happened to arrive, and
        // an entity that changed appearance and then went quiet -- a sheep sheared and left alone
        // -- would sit unsent behind the 90s liveness timeout.
        // Wake often even with nothing pending, so a queued `bot_command` runs within a tick or two
        // instead of waiting for the next inbound packet. The dead-connection check moved to
        // `last_packet` below -- it used to ride on this timeout, which a short poll would have
        // silently disabled.
        // A walk needs the tightest wake of the three: one position packet per Minecraft tick is what
        // a real client sends, and batching them into 200 ms lumps is what the server's per-packet
        // speed check objects to.
        let wait = if play.walking.is_some() {
            WALK_TICK
        } else if play.meta_dirty.is_empty() && play.monitor_dirty.is_empty() {
            IDLE_TICK
        } else {
            META_FLUSH_TICK
        };
        // A timeout with work pending is a flush tick, NOT a dead connection -- and it must fall
        // through to the housekeeping below rather than `continue`, or a permanently-churning
        // entity (a frog re-sending its pose) starves the status and summary logs entirely.
        let frame = match tokio::time::timeout(wait, rd.read()).await {
            Ok(Ok(f)) => Some(f),
            Ok(Err(e)) => return Ok(PlayExit::End(SessionEnd::Disconnected(format!("play read: {e}")))),
            Err(_) => None,
        };
        if frame.is_none() && last_packet.elapsed() > Duration::from_secs(90) {
            bail!("no packet for 90s in play");
        }
        if let Some(frame) = frame {
            last_packet = Instant::now();
            play.packets += 1;
            let mut r = Reader::new(&frame);
            let id = r.varint()?;
            trace!("play <- 0x{id:02x} ({} bytes)", frame.len());
            let res: Result<Option<PlayExit>> = handle_play_packet(id, &mut r, cfg, ctx, wr, play, &mut payload_counts).await;
            match res {
                Ok(Some(exit)) => return Ok(exit),
                Ok(None) => {}
                Err(e) => {
                    *unknown_counts.entry(id).or_default() += 1;
                    warn!("packet 0x{id:02x} ({} bytes) failed to decode: {e}", frame.len());
                }
            }
        }
        if !play.meta_dirty.is_empty() && play.last_meta_flush.elapsed() >= META_FLUSH_TICK {
            flush_entity_meta(ctx, play);
        }
        if !play.monitor_dirty.is_empty() {
            flush_monitors(ctx, play);
        }
        advance_walk(ctx, play, wr).await?;
        // One request per iteration: acting on the world is rare and doing them one at a time keeps
        // `pending_open` meaningful (a second open cannot be in flight while the first is waiting).
        if let Ok(cmd) = ctx.commands.try_recv() {
            if let Err(e) = run_bot_command(ctx, play, wr, cmd).await {
                error!("bot command failed to send: {e:#}");
            }
        }
        // A click's outcome is whatever the server corrected within the settle window. NOT a failure
        // when nothing arrives: a click on an empty slot with an empty hand legitimately changes
        // nothing and produces no packets at all.
        if let Some(pc) = &play.pending_click {
            if pc.sent.elapsed() > CLICK_SETTLE {
                let pc = play.pending_click.take().expect("just checked");
                let after = play
                    .containers
                    .get(&pc.window_id)
                    .and_then(|st| st.slots.get(pc.slot as usize))
                    .cloned()
                    .unwrap_or_default();
                let carried = play.containers.get(&pc.window_id).map(|st| st.carried.clone()).unwrap_or_default();
                let detail = if pc.corrections == 0 {
                    "the server sent no correction: it applied no change".to_string()
                } else {
                    format!(
                        "{} slot correction(s); slot {} went from {} to {}; carried is now {}",
                        pc.corrections,
                        pc.slot,
                        if pc.before.is_empty() { "empty" } else { &pc.before },
                        if after.is_empty() { "empty" } else { &after },
                        if carried.is_empty() { "empty" } else { &carried },
                    )
                };
                info!("click settled: {detail}");
                let _ = ctx.tx.send(Update::CommandDone { id: pc.id, status: "done".into(), detail });
            }
        }
        // An open that produced no screen has to end. The server drops an interaction it does not
        // like WITHOUT telling the client, so a timeout is the only available answer.
        if let Some(p) = &play.pending_open {
            if p.sent.elapsed() > OPEN_TIMEOUT {
                let p = play.pending_open.take().expect("just checked");
                warn!("no container opened at {},{},{} within {:?}", p.pos.0, p.pos.1, p.pos.2, OPEN_TIMEOUT);
                let _ = ctx.tx.send(Update::CommandDone {
                    id: p.id,
                    status: "failed".into(),
                    detail: format!("no screen opened for {},{},{} within {}ms -- the block may not be a container, or the server refused the interaction silently", p.pos.0, p.pos.1, p.pos.2, OPEN_TIMEOUT.as_millis()),
                });
            }
        }
        if play.last_status.elapsed() > Duration::from_secs(2) {
            play.last_status = Instant::now();
            status(ctx, cfg, "play", "", Some(play));
        }
        if last_summary.elapsed() > Duration::from_secs(60) {
            last_summary = Instant::now();
            let mut top: Vec<(&String, &u64)> = payload_counts.iter().collect();
            top.sort_by(|a, b| b.1.cmp(a.1));
            let top: Vec<String> = top.iter().take(6).map(|(k, v)| format!("{k}={v}")).collect();
            info!(
                "play: {} packets, {} chunks, {} entities, {} players; mod payloads: {}",
                play.packets,
                play.world.chunks.len(),
                play.entities.len(),
                play.players.len(),
                top.join(" ")
            );
            if !unknown_counts.is_empty() {
                warn!("decode failures by packet id: {unknown_counts:?}");
            }
            log_meta_stats(play);
            log_monitor_stats(play);
        }
    }
}

/// Append one raw mod payload to `data/payload-dump/<channel>.bin` as `<u32 LE length><bytes>`.
///
/// Measurement hook for `MCST_DUMP_PAYLOAD`. A mod's payload layout is not in any protocol table
/// and cannot be looked up, so the only honest way to write a decoder for one is to capture the
/// bytes and read them next to the mod's own jar. Failures are logged once and then swallowed --
/// a dump that cannot be written must never take the packet loop down with it.
fn dump_payload(channel: &str, body: &[u8]) {
    use std::io::Write;
    let dir = std::path::Path::new("data/payload-dump");
    let path = dir.join(format!("{}.bin", channel.replace([':', '/'], "_")));
    let r = std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(&path))
        .and_then(|mut f| {
            f.write_all(&(body.len() as u32).to_le_bytes())?;
            f.write_all(body)
        });
    if let Err(e) = r {
        warn!("payload dump to {} failed: {e}", path.display());
    }
}

/// A `computercraft:monitor_client` payload: decode, drop it if the screen did not change, and
/// otherwise queue it for the rate-limited flush.
fn monitor_payload(ctx: &mut Ctx, play: &mut Play, body: &[u8]) {
    play.monitor_stats.payloads += 1;
    let msg = match computercraft::decode(body) {
        Ok(m) => m,
        Err(e) => {
            // Counted, not fatal, and NOT retried differently: this is one mod's private format and
            // a mod update is allowed to change it. A rising `failed` count in the summary is the
            // signal to go back to the jar.
            play.monitor_stats.failed += 1;
            if play.monitor_stats.failed <= 3 {
                warn!("monitor payload ({} bytes) failed to decode: {e}", body.len());
            }
            return;
        }
    };
    let pos = (msg.x, msg.y, msg.z);
    let (block_width, block_height) = play.monitor_geom.get(&pos).copied().unwrap_or((0, 0));
    let facing = play
        .world
        .get_block(msg.x, msg.y, msg.z)
        .and_then(|id| ctx.block_states.table.get(id as usize))
        .map(|(_, props)| computercraft::facing_of(props).unwrap_or_default())
        .unwrap_or_default();

    let data = match msg.terminal {
        Some(t) => MonitorData {
            x: msg.x,
            y: msg.y,
            z: msg.z,
            facing,
            block_width,
            block_height,
            term_width: t.width,
            term_height: t.height,
            colour: t.colour,
            cursor_x: t.cursor_x,
            cursor_y: t.cursor_y,
            cursor_blink: t.cursor_blink,
            cursor_fg: t.cursor_fg,
            cursor_bg: t.cursor_bg,
            has_screen: true,
            lines: t.lines,
            fg: t.fg,
            bg: t.bg,
            palette: t.palette,
        },
        None => MonitorData {
            x: msg.x,
            y: msg.y,
            z: msg.z,
            facing,
            block_width,
            block_height,
            term_width: 0,
            term_height: 0,
            colour: false,
            cursor_x: 0,
            cursor_y: 0,
            cursor_blink: false,
            cursor_fg: 0,
            cursor_bg: 0,
            has_screen: false,
            lines: vec![],
            fg: vec![],
            bg: vec![],
            palette: vec![],
        },
    };

    let now = Instant::now();
    let slot = play.monitors.entry(pos).or_insert_with(|| MonitorSlot {
        pending: None,
        published: None,
        // A monitor seen for the first time must publish IMMEDIATELY -- the payload that arrives
        // when a chunk starts being watched is the whole screen, and making it wait behind the
        // rate limit would leave every screen blank for half a second after the bot arrives.
        last_sent: now - MONITOR_MIN_INTERVAL,
        });
    // The payload is a full screen every time, so an unchanged screen is a common case, not a rare
    // one: a program that calls term.setCursorPos in a loop without writing anything re-sends the
    // identical grid every tick. Comparing here is what stops that reaching the database at all.
    if monitor_same(slot.published.as_ref(), &data) && slot.pending.is_none() {
        play.monitor_stats.unchanged += 1;
        return;
    }
    if slot.pending.is_some() {
        play.monitor_stats.coalesced += 1;
    }
    slot.pending = Some(data);
    play.monitor_dirty.insert(pos);
}

/// Screens equal for publishing purposes: everything a consumer would render.
///
/// `updated_at` is deliberately not part of it -- republishing a row only to move its timestamp is
/// the write this whole path exists to avoid.
fn monitor_same(a: Option<&MonitorData>, b: &MonitorData) -> bool {
    let Some(a) = a else { return false };
    a.has_screen == b.has_screen
        && a.term_width == b.term_width
        && a.term_height == b.term_height
        && a.colour == b.colour
        && a.cursor_x == b.cursor_x
        && a.cursor_y == b.cursor_y
        && a.cursor_blink == b.cursor_blink
        && a.cursor_fg == b.cursor_fg
        && a.cursor_bg == b.cursor_bg
        && a.facing == b.facing
        && a.block_width == b.block_width
        && a.block_height == b.block_height
        && a.lines == b.lines
        && a.fg == b.fg
        && a.bg == b.bg
        && a.palette == b.palette
}

/// Publish every monitor whose rate limit has elapsed. One reducer call for the whole sweep.
fn flush_monitors(ctx: &mut Ctx, play: &mut Play) {
    let now = Instant::now();
    let mut batch: Vec<MonitorData> = Vec::new();
    let mut done: Vec<(i32, i32, i32)> = Vec::new();
    for &pos in play.monitor_dirty.iter() {
        let Some(slot) = play.monitors.get(&pos) else {
            done.push(pos);
            continue;
        };
        if slot.pending.is_none() {
            done.push(pos);
            continue;
        }
        if now.duration_since(slot.last_sent) < MONITOR_MIN_INTERVAL {
            continue;
        }
        done.push(pos);
    }
    for pos in &done {
        let Some(slot) = play.monitors.get_mut(pos) else { continue };
        let Some(data) = slot.pending.take() else { continue };
        let bytes: usize = data.lines.iter().chain(data.fg.iter()).chain(data.bg.iter()).map(|s| s.len()).sum();
        play.monitor_stats.max_bytes = play.monitor_stats.max_bytes.max(bytes);
        play.monitor_stats.bytes += bytes as u64;
        play.monitor_stats.sent += 1;
        slot.last_sent = now;
        slot.published = Some(data.clone());
        batch.push(data);
    }
    for pos in done {
        play.monitor_dirty.remove(&pos);
    }
    if !batch.is_empty() {
        let _ = ctx.tx.send(Update::Monitors(batch));
    }
}

/// Drop what we know about the monitors in a chunk the server just told us to forget.
///
/// NOT bookkeeping: `unload_chunk` DELETES those rows on the module side, and the client's
/// "is this screen different from the one I published?" check would still be holding the deleted
/// screen. Walk away from a base and come back and the identical screen arrives, compares equal,
/// and is suppressed -- leaving a monitor with no row at all until its content happens to change.
/// Measured cost of getting this wrong is a permanently blank screen, so it is deliberately
/// forgotten here rather than left to expire.
///
/// Takes the three collections rather than `Play` so the rule is exercisable on its own -- the
/// whole point is that all three are dropped together.
fn forget_chunk_monitors(
    monitors: &mut HashMap<(i32, i32, i32), MonitorSlot>,
    dirty: &mut std::collections::HashSet<(i32, i32, i32)>,
    geom: &mut HashMap<(i32, i32, i32), (u32, u32)>,
    cx: i32,
    cz: i32,
) {
    monitors.retain(|&(x, _, z), _| (x >> 4, z >> 4) != (cx, cz));
    dirty.retain(|&(x, _, z)| (x >> 4, z >> 4) != (cx, cz));
    geom.retain(|&(x, _, z), _| (x >> 4, z >> 4) != (cx, cz));
}

/// `EquipmentSlot.values()` ordinals, as `set_equipment` packs them. BODY is 1.21's addition (horse
/// and wolf armour), so an older table would mis-name it.
fn equipment_slot_name(ord: i8) -> &'static str {
    match ord {
        0 => "mainhand",
        1 => "offhand",
        2 => "feet",
        3 => "legs",
        4 => "chest",
        5 => "head",
        6 => "body",
        _ => "unknown",
    }
}

/// Component names seen while decoding a batch of stacks, accumulated so they can be folded into the
/// stats AFTER the `itemstack::Env` borrow of `play` has ended.
///
/// Containers are where the non-trivial stacks live -- exactly what the entity-metadata census could
/// not see -- so they feed the same counters.
#[derive(Default)]
struct StackCensus {
    stacks: u64,
    seen: Vec<String>,
    unknown: Vec<String>,
}

impl StackCensus {
    fn note(&mut self, d: &crate::itemstack::Decoded) {
        if let Some(st) = &d.stack {
            self.stacks += 1;
            self.seen.extend(st.components.keys().cloned());
        }
        if let Some(c) = &d.stopped_at {
            self.seen.push(c.clone());
            self.unknown.push(c.clone());
        }
    }
    fn apply(self, play: &mut Play) {
        play.meta_stats.stacks += self.stacks;
        for n in self.seen {
            *play.meta_stats.components_seen.entry(n).or_default() += 1;
        }
        for n in self.unknown {
            *play.meta_stats.components_unknown.entry(n).or_default() += 1;
        }
    }
}

/// Publish a container row, taking the menu type/title/origin from the session state the
/// `open_screen` packet set up.
///
/// Window 0 never gets an `open_screen`, so it has no session entry and gets an empty menu type --
/// which is correct: it is the player's own inventory and the server just sends it.
fn publish_container(ctx: &mut Ctx, play: &Play, window_id: i32, slots: Vec<String>, carried: String, state_id: i32, undecoded_component: Option<String>) {
    let st = play.containers.get(&window_id);
    let from = st.and_then(|s| s.from);
    let _ = ctx.tx.send(Update::Container(ContainerData {
        window_id,
        menu_type: st.map(|s| s.menu_type.clone()).unwrap_or_default(),
        title: st.map(|s| s.title.clone()).unwrap_or_default(),
        opened_from_x: from.map(|p| p.0),
        opened_from_y: from.map(|p| p.1),
        opened_from_z: from.map(|p| p.2),
        slots,
        carried,
        state_id,
        undecoded_component,
    }));
}

/// Vanilla `Direction` ordinals, which is what `writeEnum` puts on the wire for a block face.
fn face_ordinal(name: &str) -> i32 {
    match name {
        "down" => 0,
        "up" => 1,
        "north" => 2,
        "south" => 3,
        "west" => 4,
        "east" => 5,
        _ => 1, // "pick one": the top face, which is what you click on a chest
    }
}

/// Vanilla's own reach rule, so a request that the server would silently drop is rejected here with a
/// reason instead.
///
/// `ServerGamePacketListenerImpl.handleUseItemOn` calls `player.canInteractWithBlock(pos, 1.0)`:
/// `blockInteractionRange()` (4.5 by default) plus that 1.0 of padding, measured to the closest point
/// of the block rather than its centre. Being generous here is the wrong way to err -- an accepted
/// request that produces nothing looks like a decoder bug.
fn within_reach(bot: (f64, f64, f64), pos: (i32, i32, i32)) -> Option<f64> {
    let clamp = |v: f64, lo: f64, hi: f64| v.max(lo).min(hi);
    // the bot's EYES, not its feet: vanilla measures from the eye position
    let eye = (bot.0, bot.1 + 1.62, bot.2);
    let cx = clamp(eye.0, pos.0 as f64, pos.0 as f64 + 1.0);
    let cy = clamp(eye.1, pos.1 as f64, pos.1 as f64 + 1.0);
    let cz = clamp(eye.2, pos.2 as f64, pos.2 as f64 + 1.0);
    let d = ((eye.0 - cx).powi(2) + (eye.1 - cy).powi(2) + (eye.2 - cz).powi(2)).sqrt();
    if d <= 5.5 {
        Some(d)
    } else {
        None
    }
}

/// How often to step a walk. 50 ms is one Minecraft tick, which is the rate a real client sends
/// position packets at -- and the rate matters, because the server's "moved too quickly" check is
/// per-packet, so covering the same ground in fewer, larger steps is what gets a bot rubber-banded.
/// How long to wait before asking for a respawn again. The server answers with a `respawn` packet, so
/// one request is normally enough; this only covers the case where it is dropped.
const RESPAWN_RETRY: Duration = Duration::from_secs(5);

const WALK_TICK: Duration = Duration::from_millis(50);

/// Give up on a walk that has not covered `WALK_STALL_EPSILON` in this long.
const WALK_STALL: Duration = Duration::from_secs(2);

/// How far the bot must get for the walk to count as progressing.
const WALK_STALL_EPSILON: f64 = 0.2;

/// A walk that runs this long is abandoned whatever it is doing. A straight-line walker in a maze can
/// slide along a wall indefinitely without either arriving or stalling.
const WALK_DEADLINE: Duration = Duration::from_secs(120);

/// The mirrored world, viewed as passability. Bridges `World` (which knows state ids) and
/// `BlockStates` (which knows names), because `walk.rs` deliberately knows neither.
struct WorldBlocks<'a> {
    world: &'a World,
    states: &'a BlockStates,
}

impl crate::walk::Blocks for WorldBlocks<'_> {
    fn passable_at(&self, x: i32, y: i32, z: i32) -> Option<bool> {
        // `None` from `get_block` means the section is not mirrored, and `walk.rs` treats that as
        // blocking. Do NOT substitute air here: a chunk we have not been sent is exactly the place a
        // step would put the bot somewhere the server disagrees about.
        let id = self.world.get_block(x, y, z)?;
        if self.states.is_air(id) {
            return Some(true);
        }
        let name = self.states.name(id)?;
        // Hazards are never walked into, even where a client physically could -- and this is checked
        // BEFORE collision, because lava has no collision shape at all and would otherwise read as a
        // perfectly good place to walk.
        if crate::walk::hazardous(name) {
            return Some(false);
        }
        // The UNION of the authoritative collision shape and the name heuristic, and it has to be the
        // union rather than either alone:
        //   - collision-empty settles what the body can pass through, for mods included, which no list
        //     of names can ever cover;
        //   - but a LADDER has a real (thin) collision shape and is still somewhere a player stands, so
        //     collision alone would wall one off. The name list keeps those.
        // Both sources only ever ADD passability, so the union is no less safe than the stricter of them.
        let empty = self.states.collision_empty(id).unwrap_or(false);
        Some(empty || crate::walk::passable(name))
    }

    fn climbable_at(&self, x: i32, y: i32, z: i32) -> Option<bool> {
        let id = self.world.get_block(x, y, z)?;
        if self.states.is_air(id) {
            return Some(false);
        }
        Some(crate::walk::climbable(self.states.name(id)?))
    }
}

/// Step an in-progress walk, sending at most one position packet.
///
/// Split from `run_bot_command` because a walk is the only command that spans ticks: everything else
/// sends a packet and waits for a reply, while this one *is* a reply-less stream of packets and the
/// completion condition is geometric.
async fn advance_walk(ctx: &mut Ctx, play: &mut Play, wr: &mut RawWriteConnection) -> Result<()> {
    let Some(w) = &play.walking else { return Ok(()) };
    let (id, target, started) = (w.id, w.target, w.started);
    let blocks = WorldBlocks { world: &play.world, states: &ctx.block_states };
    // Gravity first: a bot in mid-air has no business taking a walking step, and until it is on the
    // ground `step_toward` would be judging footing from the wrong height.
    //
    // Deliberately ONLY while walking. Making the bot fall whenever unsupported would change what the
    // plain mirroring bot does -- this one joined standing on a two-block ledge, so it would drop off
    // on connect and land somewhere nobody chose. A walk is an explicit instruction to move; idling is
    // not.
    if let Some(ny) = crate::walk::fall_step(&blocks, play.bot_pos) {
        play.bot_pos.1 = ny;
        let mut p = Writer::with_id(0x1b);
        p.f64(play.bot_pos.0);
        p.f64(ny);
        p.f64(play.bot_pos.2);
        p.f32(play.bot_look.0);
        p.f32(play.bot_look.1);
        p.bool(false); // airborne
        write_raw(wr, p.into_inner()).await?;
        // Falling IS progress -- it is how the bot gets off a ledge -- so the stall timer is reset.
        if let Some(w) = play.walking.as_mut() {
            w.steps += 1;
            w.last_progress = Instant::now();
            w.progress_pos = play.bot_pos;
        }
        return Ok(());
    }
    // The fall limit tracks CURRENT health: a hurt bot takes shallower drops, because it cannot heal
    // and a dead one stops the mirror entirely.
    let max_fall = crate::walk::survivable_drop(play.health, crate::walk::FALL_RESERVE);
    let step = crate::walk::step_toward(&blocks, play.bot_pos, target, max_fall);
    let finish = |ctx: &mut Ctx, status: &str, detail: String| {
        let _ = ctx.tx.send(Update::CommandDone { id, status: status.into(), detail });
    };
    match step {
        crate::walk::Step::Arrived => {
            // A waypoint, not necessarily the destination: take the next leg if there is one.
            let w = play.walking.as_mut().expect("checked above");
            if let Some(next) = w.route.pop_front() {
                w.target = next;
                // Reaching a waypoint IS progress, even if the last few ticks crept.
                w.last_progress = Instant::now();
                w.progress_pos = play.bot_pos;
                return Ok(());
            }
            let w = play.walking.take().expect("checked above");
            info!("walk done: {:.1},{:.1},{:.1} in {} steps ({:.1}s)", play.bot_pos.0, play.bot_pos.1, play.bot_pos.2, w.steps, started.elapsed().as_secs_f64());
            let where_ = format!("{:.2},{:.2},{:.2}", play.bot_pos.0, play.bot_pos.1, play.bot_pos.2);
            if w.partial {
                // Honest about the difference: the route was followed to its end, and its end was not
                // where the caller asked. A caller retrying blindly would otherwise loop for ever.
                let (dx, dz) = (w.goal.0 as f64 - play.bot_pos.0, w.goal.2 as f64 - play.bot_pos.2);
                finish(ctx, "failed", format!("no route to {},{},{}; walked to the closest reachable point {where_} in {} steps, {:.1} blocks short", w.goal.0, w.goal.1, w.goal.2, w.steps, dx.hypot(dz)));
            } else {
                finish(ctx, "done", format!("arrived at {where_} in {} steps", w.steps));
            }
        }
        crate::walk::Step::Blocked(why) => {
            // Blocked is not the end of the walk by itself -- a door may be about to open, and a
            // chunk that has not arrived yet reads as blocked. The stall timer below decides.
            if w.last_progress.elapsed() <= WALK_STALL && started.elapsed() <= WALK_DEADLINE {
                return Ok(());
            }
            // Stalled. Before giving up, RE-ROUTE once from where we actually are: chunks load while
            // the bot walks, doors open and close, and the route was planned against a world that has
            // since changed. Re-planning is cheap next to reporting a failure the caller cannot act on.
            let goal = w.goal;
            let from = (play.bot_pos.0.floor() as i32, play.bot_pos.1.floor() as i32, play.bot_pos.2.floor() as i32);
            let replanned = crate::path::find(&blocks, from, goal, 1, crate::walk::survivable_drop(play.health, crate::walk::FALL_RESERVE))
                .map(|p| crate::path::simplify(from, &p))
                .filter(|p| !p.is_empty());
            if let Some(route) = replanned {
                let reached = *route.last().expect("non-empty");
                let still_partial = (reached.0 - goal.0).abs() > 1
                    || (reached.2 - goal.2).abs() > 1
                    // Height counts. A route ending six blocks under the goal reached a different
                    // floor, and reporting that as arrival is exactly what this missed the first time.
                    || (reached.1 - goal.1).abs() > 1;
                let mut legs: std::collections::VecDeque<(f64, f64, f64)> =
                    route.iter().map(|&(x, y, z)| (x as f64 + 0.5, y as f64, z as f64 + 0.5)).collect();
                let next = legs.pop_front().expect("non-empty");
                let w = play.walking.as_mut().expect("checked above");
                // Only worth continuing if the new plan is not the same stuck plan: if it still ends
                // where we already are, re-routing has nothing to add and we would loop until the
                // deadline.
                if (reached.0, reached.2) != (from.0, from.2) && started.elapsed() <= WALK_DEADLINE {
                    debug!("walk re-routed from {:.1},{:.1},{:.1}: {} waypoints", play.bot_pos.0, play.bot_pos.1, play.bot_pos.2, legs.len() + 1);
                    w.route = legs;
                    w.target = next;
                    w.partial = still_partial;
                    w.last_progress = Instant::now();
                    w.progress_pos = play.bot_pos;
                    return Ok(());
                }
            }
            let w = play.walking.take().expect("checked above");
            let (dx, dz) = (goal.0 as f64 - play.bot_pos.0, goal.2 as f64 - play.bot_pos.2);
            let left = dx.hypot(dz);
            warn!("walk gave up at {:.1},{:.1},{:.1}, {left:.1} blocks from the goal: {why}", play.bot_pos.0, play.bot_pos.1, play.bot_pos.2);
            finish(ctx, "failed", format!("{why}; stopped at {:.2},{:.2},{:.2} after {} steps, {left:.2} blocks from {},{},{} -- re-routing from there found nothing better", play.bot_pos.0, play.bot_pos.1, play.bot_pos.2, w.steps, goal.0, goal.1, goal.2));
        }
        crate::walk::Step::Move { x, y, z, on_ground } => {
            play.bot_pos = (x, y, z);
            // Face the way we are going. Cosmetic to the server, but the bot's yaw is mirrored and a
            // bot sliding sideways through a world looks like a bug in whatever is reading it.
            let (dx, dz) = (target.0 - x, target.2 - z);
            if dx.hypot(dz) > 0.5 {
                play.bot_look.0 = (-dx.atan2(dz)).to_degrees() as f32;
            }
            let mut p = Writer::with_id(0x1b); // serverbound move_player_pos_rot
            p.f64(x);
            p.f64(y);
            p.f64(z);
            p.f32(play.bot_look.0);
            p.f32(play.bot_look.1);
            p.bool(on_ground);
            write_raw(wr, p.into_inner()).await?;
            let w = play.walking.as_mut().expect("checked above");
            w.steps += 1;
            let moved = ((x - w.progress_pos.0).powi(2) + (y - w.progress_pos.1).powi(2) + (z - w.progress_pos.2).powi(2)).sqrt();
            if moved >= WALK_STALL_EPSILON {
                w.last_progress = Instant::now();
                w.progress_pos = (x, y, z);
            } else if w.last_progress.elapsed() > WALK_STALL {
                // Moving, but not getting anywhere -- sliding along a wall on the spot.
                let w = play.walking.take().expect("checked above");
                finish(ctx, "failed", format!("made no headway for {:.0}s at {:.2},{:.2},{:.2} after {} steps -- straight-line walker, no path round the obstacle", WALK_STALL.as_secs_f64(), x, y, z, w.steps));
            }
        }
    }
    Ok(())
}

/// Run one queued `bot_command`. THE ONLY PLACE THIS PROCESS ACTS ON THE WORLD.
///
/// Every outcome writes a terminal status back, including the refusals -- a request that just stops
/// being pending with no explanation is indistinguishable from the bot being dead.
async fn run_bot_command(ctx: &mut Ctx, play: &mut Play, wr: &mut RawWriteConnection, cmd: crate::publisher::BotCmd) -> Result<()> {
    let done = |status: &str, detail: String| {
        let _ = ctx.tx.send(Update::CommandDone { id: cmd.id, status: status.into(), detail });
    };
    match cmd.kind.as_str() {
        "open_container" => {
            let pos = (cmd.x, cmd.y, cmd.z);
            let Some(dist) = within_reach(play.bot_pos, pos) else {
                done("rejected", format!("block {},{},{} is out of reach from {:.1},{:.1},{:.1} (vanilla allows 5.5)", pos.0, pos.1, pos.2, play.bot_pos.0, play.bot_pos.1, play.bot_pos.2));
                return Ok(());
            };
            if play.pending_open.is_some() {
                done("rejected", "another open request is already in flight".into());
                return Ok(());
            }
            // A container already open would make the server close it and reopen, and the window id
            // bookkeeping gets ambiguous. Refuse rather than guess.
            if play.containers.keys().any(|&w| w != 0) {
                done("rejected", "a container is already open; close it first".into());
                return Ok(());
            }
            play.sequence += 1;
            let mut w = Writer::with_id(0x38); // serverbound use_item_on
            w.varint(0); // InteractionHand.MAIN_HAND
            w.i64(crate::wire::pack_block_pos(pos.0, pos.1, pos.2));
            w.varint(face_ordinal(&cmd.face));
            // Cursor position WITHIN the block, as a fraction. The server rejects anything more than
            // 1.0000001 outside, so the centre of the face is the safe choice.
            w.f32(0.5);
            w.f32(0.5);
            w.f32(0.5);
            w.bool(false); // isInside
            w.varint(play.sequence);
            write_raw(wr, w.into_inner()).await?;
            info!("sent use_item_on at {},{},{} (face {}, {:.1} blocks away, sequence {})", pos.0, pos.1, pos.2, cmd.face, dist, play.sequence);
            play.pending_open = Some(PendingOpen { id: cmd.id, pos, sent: Instant::now() });
        }
        "walk_to" => {
            // Unlike every other command this one does NOT finish here: it starts a walk that
            // `advance_walk` steps over the following ticks, and the terminal status is written when
            // the bot arrives or gives up.
            if play.walking.is_some() {
                done("rejected", "already walking; the bot takes one destination at a time".into());
                return Ok(());
            }
            // Walking away with a container open has the server close it out from under us, which
            // would leave the `container` row describing a window that no longer exists.
            if play.containers.keys().any(|&w| w != 0) {
                done("rejected", "a container is open; close it before walking".into());
                return Ok(());
            }
            let goal = (cmd.x, cmd.y, cmd.z);
            let blocks = WorldBlocks { world: &play.world, states: &ctx.block_states };
            let from = (play.bot_pos.0.floor() as i32, play.bot_pos.1.floor() as i32, play.bot_pos.2.floor() as i32);
            // ROUTE FIRST. The search uses the walker's own geometry, so a path it returns is one the
            // walker will follow -- and it is what lets the bot reach anything that is not in a straight
            // line, including a floor directly beneath it (which needs a step AWAY from the goal first).
            //
            // `goal_slack` of 1: a caller picking a block off a map usually picks the solid one it can
            // see rather than the air above it, and standing next to it is what was meant.
            let Some(route) = crate::path::find(&blocks, from, goal, 1, crate::walk::survivable_drop(play.health, crate::walk::FALL_RESERVE)) else {
                done("rejected", format!("no route from {},{},{} to {},{},{} -- nowhere to step, or the world around the bot is not mirrored yet", from.0, from.1, from.2, cmd.x, cmd.y, cmd.z));
                return Ok(());
            };
            let route = crate::path::simplify(from, &route);
            let reached = *route.last().expect("find returns a non-empty path");
            let partial = (reached.0 - goal.0).abs() > 1
                    || (reached.2 - goal.2).abs() > 1
                    // Height counts. A route ending six blocks under the goal reached a different
                    // floor, and reporting that as arrival is exactly what this missed the first time.
                    || (reached.1 - goal.1).abs() > 1;
            let mut legs: std::collections::VecDeque<(f64, f64, f64)> =
                route.iter().map(|&(x, y, z)| (x as f64 + 0.5, y as f64, z as f64 + 0.5)).collect();
            let target = legs.pop_front().expect("non-empty");
            info!(
                "walk to {},{},{} from {:.1},{:.1},{:.1}: {} waypoints{}",
                cmd.x, cmd.y, cmd.z, play.bot_pos.0, play.bot_pos.1, play.bot_pos.2,
                legs.len() + 1,
                if partial { format!(", closest reachable is {},{},{}", reached.0, reached.1, reached.2) } else { String::new() },
            );
            play.walking = Some(WalkTo {
                id: cmd.id,
                goal,
                route: legs,
                target,
                partial,
                last_progress: Instant::now(),
                progress_pos: play.bot_pos,
                started: Instant::now(),
                steps: 0,
            });
        }
        "drag" => {
            // QUICK_CRAFT: spread the carried stack over several slots. THE WHOLE DRAG IS SENT HERE, as
            // one unit, because it is a state machine on the server and a half-finished one leaves
            // `quickcraftStatus` set. (Vanilla self-heals -- `doClick` resets a dangling drag on the next
            // non-quick_craft click -- but relying on that would mean the next ordinary click silently
            // does nothing but clean up.)
            //
            // Encoding read from the deobfuscated jar, NOT from memory:
            //   getQuickcraftHeader(button) = button & 3        -> 0 begin, 1 add slot, 2 end
            //   getQuickcraftType(button)   = (button >> 2) & 3 -> 0 split evenly, 1 one each, 2 creative
            // so button = stage | (type << 2), and `isValidQuickcraftType` accepts 0 and 1 for any player
            // but 2 only with `hasInfiniteMaterials`, which this bot does not have.
            let Some(state) = play.containers.get(&cmd.window_id) else {
                done("rejected", format!("window {} is not open (open: {:?})", cmd.window_id, play.containers.keys().collect::<Vec<_>>()));
                return Ok(());
            };
            // The drag distributes the CURSOR. With nothing carried the server accepts every packet and
            // changes nothing, which is indistinguishable from success -- so refuse it here instead.
            if state.carried.is_empty() {
                done("rejected", "nothing is carried; a drag spreads the cursor stack, so pick something up first".into());
                return Ok(());
            }
            let drag_type = if cmd.button == 1 { 1 } else { 0 };
            let slots: Vec<i32> = cmd
                .slots
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .filter_map(|s| s.trim().parse::<i32>().ok())
                .collect();
            if slots.is_empty() {
                done("rejected", format!("no slots to drag across (slots = {:?})", cmd.slots));
                return Ok(());
            }
            let n = state.slots.len() as i32;
            if let Some(bad) = slots.iter().find(|&&s| s < 0 || s >= n) {
                done("rejected", format!("slot {bad} is outside window {} (0..{n})", cmd.window_id));
                return Ok(());
            }
            // `split evenly` over more slots than the stack has items leaves the remainder on the cursor,
            // which is fine, but one item per slot with fewer items than slots silently drops the tail --
            // vanilla's own `carried.getCount() > quickcraftSlots.size()` guard. Say so rather than let
            // the caller wonder which slots were skipped.
            let state_id = state.state_id;
            info!("drag {} slots (type {}) in window {}", slots.len(), drag_type, cmd.window_id);
            // begin, then one packet per slot, then end. Begin and end carry slot -999, which for
            // QUICK_CRAFT is the protocol's "no slot" and does NOT throw anything on the floor -- that is
            // -999 with PICKUP, which stays refused.
            for (stage, slot) in std::iter::once((0, -999))
                .chain(slots.iter().map(|&s| (1, s)))
                .chain(std::iter::once((2, -999)))
            {
                let mut w = Writer::with_id(0x0e); // serverbound container_click
                w.u8(cmd.window_id as u8);
                w.varint(state_id);
                w.i16(slot as i16);
                w.u8((stage | (drag_type << 2)) as u8);
                w.varint(5); // ClickType.QUICK_CRAFT
                w.varint(0); // changedSlots: predict nothing, let the server correct everything
                w.varint(0); // carriedItem: the empty stack (count 0) -- see the plain-click path for why
                write_raw(wr, w.into_inner()).await?;
            }
            // The cursor is spread across the slots now; claiming empty makes the ABSENCE of a correction
            // mean "empty" rather than "unchanged" -- the same reasoning as a plain click.
            if let Some(st) = play.containers.get_mut(&cmd.window_id) {
                st.carried = String::new();
            }
            done("done", format!("dragged the carried stack across {} slots ({})", slots.len(), if drag_type == 1 { "one each" } else { "split evenly" }));
        }
        "close_container" => {
            let open: Vec<i32> = play.containers.keys().copied().filter(|&w| w != 0).collect();
            if open.is_empty() {
                done("rejected", "nothing is open".into());
                return Ok(());
            }
            for w_id in open {
                let mut w = Writer::with_id(0x0f); // serverbound container_close
                w.u8(w_id as u8);
                write_raw(wr, w.into_inner()).await?;
                // The server does NOT echo a clientbound close for a client-initiated one, so the
                // row has to be dropped here or it would linger forever.
                play.containers.remove(&w_id);
                let _ = ctx.tx.send(Update::CloseContainer { window_id: w_id });
            }
            done("done", "closed".into());
        }
        "click_slot" => {
            // THE ONLY PACKET THIS PROJECT SENDS THAT MUTATES THE WORLD. Everything is validated here
            // rather than left to the server, because every one of these failures is SILENT on the
            // server side: a click for a window it does not have open, or for a slot index it
            // considers invalid, is logged at debug and dropped with no reply at all. An unvalidated
            // click therefore looks exactly like a working one that did nothing.
            let Some(state) = play.containers.get(&cmd.window_id) else {
                done("rejected", format!("window {} is not open (open: {:?})", cmd.window_id, play.containers.keys().collect::<Vec<_>>()));
                return Ok(());
            };
            // Which `ClickType` ordinals may be sent, and the button range each one allows. The button
            // is NOT a mouse button for every mode -- for SWAP it is the hotbar slot -- so validating it
            // against 0/1 across the board would silently refuse most legal swaps.
            //
            // THROW (4) drops items on the floor. It was refused here on a standing instruction that the
            // bot must never do that; the user lifted it explicitly on 2026-09-12 and asked for it
            // ungated, so it is a plain mode like the others. Callers own the consequences.
            //
            // QUICK_CRAFT (5) is not a `mode` here: it is a three-phase drag, so it has its own
            // `request_drag` reducer that sends the whole sequence. CLONE (3) is creative-mode only and
            // this bot is not in creative, so the server drops it silently.
            let button_range = match cmd.mode {
                // PICKUP: 0 left (whole stack), 1 right (half).
                0 => 0..=1,
                // QUICK_MOVE (shift-click): both buttons behave identically in vanilla.
                1 => 0..=1,
                // SWAP: the button is the HOTBAR SLOT, 0-8, plus 40 for the offhand (`F`).
                2 => 0..=40,
                // THROW: 0 drops ONE item from the slot, 1 drops the whole stack.
                4 => 0..=1,
                // PICKUP_ALL (double-click, collect-to-cursor): button 0 only.
                6 => 0..=0,
                _ => {
                    done("rejected", format!("mode {} is not implemented; 0 (pickup), 1 (quick_move), 2 (swap), 4 (throw) and 6 (pickup_all) are. QUICK_CRAFT (5) is a multi-packet drag, not a click -- use request_drag.", cmd.mode));
                    return Ok(());
                }
            };
            if !button_range.contains(&cmd.button) {
                let what = if cmd.mode == 2 { "hotbar slot (0-8, or 40 for the offhand)" } else { "button (0 left, 1 right)" };
                done("rejected", format!("{} {} is out of range for mode {}; expected {what}", what, cmd.button, cmd.mode));
                return Ok(());
            }
            // THROW is IGNORED BY THE SERVER unless the cursor is empty -- read from the jar:
            // `clickType == THROW && getCarried().isEmpty() && slotId >= 0`. With something carried it
            // returns without a reply, which is indistinguishable from a throw that did nothing, so it is
            // refused here instead.
            if cmd.mode == 4 && !state.carried.is_empty() {
                done("rejected", "something is carried; THROW only works with an empty cursor (the server ignores it otherwise) -- put the cursor down first".into());
                return Ok(());
            }
            // SWAP with the hotbar needs the player's own inventory, which is window 0. A swap addressed
            // to a container window moves between that container's slot and the player's hotbar, which is
            // legal -- but slot 40 (offhand) only exists on window 0.
            if cmd.mode == 2 && cmd.button == 40 && cmd.window_id != 0 {
                done("rejected", "the offhand (button 40) is only addressable through window 0".into());
                return Ok(());
            }
            // Vanilla's `isValidSlotIndex` also accepts -1 and -999 (-999 with pickup THROWS the
            // carried stack on the floor). Neither is offered: this refuses anything but a real slot
            // of the window we know about.
            if cmd.slot < 0 || cmd.slot as usize >= state.slots.len() {
                done("rejected", format!("slot {} is outside window {} ({} slots)", cmd.slot, cmd.window_id, state.slots.len()));
                return Ok(());
            }
            if play.pending_click.is_some() {
                done("rejected", "another click is still settling".into());
                return Ok(());
            }
            let before = state.slots[cmd.slot as usize].clone();
            let state_id = state.state_id;

            let mut w = Writer::with_id(0x0e); // serverbound container_click
            w.u8(cmd.window_id as u8);
            w.varint(state_id);
            w.i16(cmd.slot as i16);
            w.u8(cmd.button as u8);
            w.varint(cmd.mode);
            // PREDICT NOTHING. These two fields are what the client CLAIMS it now believes; the server
            // writes them into its model of us and then corrects every difference from the truth. An
            // empty map and an empty carried item therefore ask it to correct EVERYTHING that changed,
            // which is exactly the behaviour we want -- the alternative is maintaining a second copy of
            // vanilla's `doClick` and arguing with the server when the two disagree.
            w.varint(0); // changedSlots: empty
            w.varint(0); // carriedItem: the empty stack (count 0)
            write_raw(wr, w.into_inner()).await?;
            info!("sent container_click window {} slot {} button {} mode {} (state {})", cmd.window_id, cmd.slot, cmd.button, cmd.mode, state_id);
            // AND NOW ADOPT THE CLAIM WE JUST MADE. This is not prediction -- it is the other half of
            // "predict nothing", and leaving it out is a real desync that a two-click test finds:
            //
            // The server diffs reality against WHAT WE CLAIMED and only sends a correction where they
            // differ. Claiming an empty cursor is itself a statement, so when the click genuinely
            // leaves the cursor empty there is no correction at all -- and a local view still holding
            // the old cursor stack is then silently wrong. Measured: pick a stack up, put it back, and
            // `carried` stayed at the stack forever because the "it is empty now" correction was
            // never going to come.
            //
            // Mirroring the claim makes the ABSENCE of a correction informative: the cursor is empty
            // unless the server says otherwise. The slot list needs no equivalent because claiming an
            // empty `changedSlots` map says "unchanged", which is already what we hold.
            if let Some(st) = play.containers.get_mut(&cmd.window_id) {
                st.carried = String::new();
            }
            let _ = ctx.tx.send(Update::ContainerSlot { window_id: cmd.window_id, slot: -1, stack: String::new(), state_id });
            play.pending_click = Some(PendingClick { id: cmd.id, window_id: cmd.window_id, slot: cmd.slot, before, sent: Instant::now(), corrections: 0 });
        }
        other => done("rejected", format!("unknown command kind {other:?}")),
    }
    Ok(())
}

/// Remember every monitor panel's size in blocks as its block entity goes past.
///
/// Called from both block-entity paths (the chunk packet and the single-block update) because a
/// monitor can be built after its chunk arrived. Kept as a map rather than looked up on demand:
/// the client does not hold block entities, and the screen payload that needs this arrives on its
/// own schedule.
fn note_monitor_geometry(play: &mut Play, bes: &[BlockEntityData]) {
    for be in bes {
        if let Some(size) = computercraft::monitor_panel_size(&be.type_name, &be.nbt_json) {
            play.monitor_geom.insert((be.x, be.y, be.z), size);
        }
    }
}

fn log_monitor_stats(play: &Play) {
    let s = &play.monitor_stats;
    if s.payloads == 0 {
        return;
    }
    info!(
        "monitors: {} payloads over {} screens, {} unchanged, {} coalesced by the {}ms rate limit, {} sent, {} failed to decode; max row {} B, mean {} B",
        s.payloads,
        play.monitors.len(),
        s.unchanged,
        s.coalesced,
        MONITOR_MIN_INTERVAL.as_millis(),
        s.sent,
        s.failed,
        s.max_bytes,
        if s.sent == 0 { 0 } else { (s.bytes / s.sent) as usize }
    );
}

/// Send the blobs of every entity whose rate limit has elapsed.
///
/// Deliberately takes `ctx` and `play` rather than living in the packet handler: an entity whose
/// metadata went quiet still has to be flushed, and nothing is guaranteed to arrive for it again.
fn flush_entity_meta(ctx: &mut Ctx, play: &mut Play) {
    play.last_meta_flush = Instant::now();
    let now = Instant::now();
    let mut sent: Vec<i32> = Vec::new();
    for &eid in play.meta_dirty.iter() {
        let Some(slot) = play.entity_meta.get(&eid) else {
            sent.push(eid);
            continue;
        };
        if now.duration_since(slot.last_sent) < META_MIN_INTERVAL {
            continue;
        }
        let json = metadata::to_json(&slot.fields);
        let tname = play.entities.get(&eid).map(|e| e.type_name.clone()).unwrap_or_default();
        let st = play.meta_stats.by_type.entry(tname).or_insert((0, 0, 0, 0, String::new()));
        st.0 += 1;
        st.1 = st.1.max(slot.fields.len());
        if json.len() > st.2 {
            st.2 = json.len();
            st.4 = json.clone();
        }
        st.3 += json.len() as u64;
        play.meta_stats.sent += 1;
        let _ = ctx.tx.send(Update::EntityAppearance { id: eid, appearance: json });
        sent.push(eid);
    }
    for eid in sent {
        play.meta_dirty.remove(&eid);
        if let Some(slot) = play.entity_meta.get_mut(&eid) {
            slot.dirty = false;
            slot.last_sent = now;
        }
    }
}

/// Count the item-stack components this payload carried, and which of them had no codec.
///
/// The census exists because the size of the item-component job is not a property of the game, it is
/// a property of what is in this world: 327 component types are registered on this server and only a
/// handful appear on a dropped item. Guessing which ones to implement is how you write codecs nobody
/// needs and miss the one that matters.
fn note_stack_components(play: &mut Play, walked: &metadata::Walked) {
    for v in walked.fields.values() {
        let Some(obj) = v.as_object() else { continue };
        if !(obj.contains_key("count") && obj.contains_key("id")) {
            continue; // not a stack -- the walk emits objects for nothing else today, but say so
        }
        play.meta_stats.stacks += 1;
        if let Some(cs) = obj.get("components").and_then(|c| c.as_object()) {
            for name in cs.keys() {
                *play.meta_stats.components_seen.entry(name.clone()).or_default() += 1;
            }
        }
    }
    if let Some(name) = &walked.unknown_component {
        *play.meta_stats.components_seen.entry(name.clone()).or_default() += 1;
        *play.meta_stats.components_unknown.entry(name.clone()).or_default() += 1;
    }
}

/// What the appearance blob actually costs, measured rather than assumed.
///
/// The risk with putting metadata on `entity` is write volume, not correctness: the row is
/// rewritten every time the entity moves, so the blob rides along on traffic that already exists,
/// and a metadata packet that changes a carried field costs an extra reducer call on top. Both
/// numbers are counted so the trade-off is a measurement in the log rather than a guess.
fn log_meta_stats(play: &Play) {
    let s = &play.meta_stats;
    if s.packets == 0 {
        return;
    }
    let mut rows: Vec<(&String, &(u64, usize, usize, u64, String))> = s.by_type.iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    let total_bytes: u64 = s.by_type.values().map(|v| v.3).sum();
    let total_rows: u64 = s.by_type.values().map(|v| v.0).sum();
    info!(
        "entity metadata: {} packets, {} changed a carried field, {} sent ({:.2} reducer calls/s over {} entities), {} coalesced by the {}ms rate limit, {} truncated by an unskippable serializer; mean blob {} bytes",
        s.packets,
        s.changed,
        s.sent,
        s.sent as f64 / s.started.elapsed().as_secs_f64().max(1.0),
        play.entity_meta.len(),
        s.coalesced,
        META_MIN_INTERVAL.as_millis(),
        s.truncated,
        if total_rows == 0 { 0 } else { (total_bytes / total_rows) as usize }
    );
    for (name, (n, fields, max, sum, sample)) in rows.iter().take(8) {
        info!("  {name}: {n} updates, up to {fields} fields, max {max} B, mean {} B  e.g. {sample}", sum / n.max(&1));
    }
    if s.stacks > 0 || !s.components_seen.is_empty() {
        let mut seen: Vec<(&String, &u64)> = s.components_seen.iter().collect();
        seen.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let list: Vec<String> = seen.iter().map(|(k, v)| format!("{k}={v}")).collect();
        info!("item stacks: {} decoded; components seen: {}", s.stacks, if list.is_empty() { "(none)".into() } else { list.join(" ") });
        if !s.components_unknown.is_empty() {
            let mut unk: Vec<(&String, &u64)> = s.components_unknown.iter().collect();
            unk.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            // NOT a warning about the data -- a to-do list. Each name here is a codec that has to be
            // read out of a jar before that stack (and the rest of its packet) can be decoded.
            warn!("item stack components with NO codec: {}", unk.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" "));
        }
    }
}

async fn handle_play_packet(id: i32, r: &mut Reader<'_>, cfg: &Config, ctx: &mut Ctx, wr: &mut RawWriteConnection, play: &mut Play, payload_counts: &mut HashMap<String, u64>) -> Result<Option<PlayExit>> {
    match id {
        0x01 => {
            // add entity
            let eid = r.varint()?;
            let uuid = r.uuid()?;
            let type_id = r.varint()? as u32;
            let x = r.f64()?;
            let y = r.f64()?;
            let z = r.f64()?;
            let pitch = angle(r.i8()?);
            let yaw = angle(r.i8()?);
            let head_yaw = angle(r.i8()?);
            let data = r.varint()?;
            let vx = r.i16()? as f64 / 8000.0;
            let vy = r.i16()? as f64 / 8000.0;
            let vz = r.i16()? as f64 / 8000.0;
            let type_name = registry_name(ctx, "minecraft:entity_type", type_id);
            let info = EntityInfo { uuid: uuid.clone(), type_id, type_name, x, y, z, yaw, pitch, head_yaw };
            let ed = entity_data(eid, &info, data, (vx, vy, vz));
            if info.type_name == "minecraft:player" {
                if let Some(p) = play.players.get_mut(&uuid) {
                    p.entity_id = Some(eid);
                    p.pos = (x, y, z);
                    p.rot = (yaw, pitch);
                    let _ = ctx.tx.send(Update::Players(vec![player_data(&uuid, p)]));
                }
            }
            play.entities.insert(eid, info);
            let _ = ctx.tx.send(Update::Entities(vec![ed]));
        }
        0x07 => {
            // block entity data
            let (x, y, z) = r.block_pos()?;
            let type_id = r.varint()? as u32;
            let (nbt, raw) = r.nbt()?;
            let be = block_entity_data(ctx, play, x, y, z, type_id, &nbt, raw);
            note_monitor_geometry(play, std::slice::from_ref(&be));
            let _ = ctx.tx.send(Update::BlockEntities(vec![be]));
        }
        0x09 => {
            // block update
            let (x, y, z) = r.block_pos()?;
            let state = r.varint()? as u32;
            block_changed(ctx, play, &[(x, y, z, state)]);
        }
        0x16 => {
            let key = r.string()?;
            let mut w = Writer::with_id(0x11);
            w.string(&key);
            w.bool(false);
            write_raw(wr, w.into_inner()).await?;
        }
        0x19 => {
            let channel = r.string()?;
            *payload_counts.entry(channel.clone()).or_default() += 1;
            // Measurement hook, not a feature: `MCST_DUMP_PAYLOAD=<channel>` appends every raw
            // payload on that channel to data/payload-dump/<channel>.bin as
            // `<u32 length><bytes>`. A mod's private wire format cannot be looked up, so the only
            // honest way to write a decoder for one is to read its bytes first. Kept because the
            // next mod channel will need exactly this again.
            if let Ok(want) = std::env::var("MCST_DUMP_PAYLOAD") {
                if want == channel {
                    dump_payload(&channel, r.rest());
                }
            }
            if channel == crate::computercraft::MONITOR_CHANNEL {
                monitor_payload(ctx, play, r.rest());
            }
        }
        0x1d => {
            let raw = r.rest();
            let reason = component_to_text(raw);
            let mut cur = std::io::Cursor::new(raw);
            if let Ok(Some(tag)) = simdnbt::owned::read_optional_tag(&mut cur) {
                if let simdnbt::owned::NbtTag::Compound(c) = tag {
                    warn!("play disconnect reason JSON: {}", crate::wire::nbt_compound_to_json(&c));
                }
            }
            return Ok(Some(PlayExit::End(SessionEnd::Disconnected(format!("play: {reason}")))));
        }
        0x21 => {
            // forget level chunk: ChunkPos as long (x low, z high)
            let v = r.i64()?;
            let cx = v as i32;
            let cz = (v >> 32) as i32;
            if play.world.chunks.remove(&(cx, cz)).is_some() {
                forget_chunk_monitors(&mut play.monitors, &mut play.monitor_dirty, &mut play.monitor_geom, cx, cz);
                let _ = ctx.tx.send(Update::Unload { cx, cz });
            }
        }
        0x26 => {
            let v = r.i64()?;
            let mut w = Writer::with_id(0x18);
            w.i64(v);
            write_raw(wr, w.into_inner()).await?;
        }
        0x0c => {
            // CHUNK BATCH FINISHED -- and answering it is what makes the world arrive at all.
            //
            // Since 1.20.2 the server paces chunks by batch: it sends `chunk_batch_start` (0x0d,
            // no fields), the chunks, then `chunk_batch_finished` (this, one VarInt batch size),
            // and waits for the client to say how fast it can take them. A client that never
            // answers is throttled to the server's MAX_UNACKNOWLEDGED_BATCHES and then stops.
            //
            // Measured against the dev replica before this existed: NINE chunks, a 3x3 around the
            // bot, against a server view distance of 6 that should have sent ~169 -- and both batch
            // markers arriving exactly once each, which is the stall. That is the whole of "spacetime
            // version keeps unloading chunks and whatever its super buggy" (the user, 2026-09-11).
            //
            // Vanilla measures its own deserialisation time and converges on a rate. This is a
            // headless mirror with no rendering to keep up with, so it asks for the ceiling vanilla
            // clamps to (64/tick) and lets the server's own view distance be the limit.
            let _batch_size = r.varint()?;
            let mut w = Writer::with_id(0x08);
            w.f32(CHUNKS_PER_TICK);
            write_raw(wr, w.into_inner()).await?;
        }
        0x27 => {
            // Kept so the once-per-session garbage chunk can be DESCRIBED rather than just
            // discarded: `rest()` borrows the frame, not the reader, so this costs nothing.
            let head = r.rest();
            let cx = r.i32()?;
            let cz = r.i32()?;
            if cx.abs() > 1_000_000 || cz.abs() > 1_000_000 {
                // THE "GARBAGE COORDINATE" CHUNK IS NOT GARBAGE.
                //
                // Roughly once per session a chunk arrives at cx == cz == 1280064, and it was
                // assumed to be a mis-parse. It is not: the bytes after the coordinates are a
                // textbook heightmaps NBT (0a 0c 00 0f "MOTION_BLOCKING" 00 00 00 25 = 37 longs),
                // and saving the chunk out shows 4 sections of `aeronautics:white_envelope`, oak
                // fence and slab, a chest and a `simulated:altitude_sensor`. It is somebody's
                // AIRSHIP: the Aeronautics mod keeps its craft in a side level 20 million blocks
                // out and streams it over the ordinary chunk packet, with nothing on the wire to
                // say the chunk belongs to a different level than the player's.
                //
                // So the decoder is right and the mirror is wrong: this chunk lands in `chunk` /
                // `chunk_section` as if it were overworld terrain. Fixing that means giving the
                // mirror a notion of which level a chunk belongs to, which is a schema change; the
                // world download meanwhile refuses it on distance (see anvil::plausible).
                warn!("chunk at {cx},{cz} ({} bytes) is far outside the player's level -- likely an Aeronautics airship: {:02x?}", head.len(), &head[..head.len().min(48)]);
            }
            // Heightmaps were read and thrown away until the world download needed them: Anvil's
            // root `Heightmaps` is exactly this compound, so it is re-emitted verbatim.
            let (heightmaps, _) = r.nbt()?;
            let data = r.byte_array()?;
            let chunk = Chunk::parse(cx, cz, play.min_section, play.section_count, data, ctx.block_states.direct_bits(), play.biome_direct_bits)
                .with_context(|| format!("chunk {cx},{cz} ({} bytes, {} sections)", data.len(), play.section_count))?;
            let sections: Vec<SectionData> = chunk.sections.iter().enumerate().map(|(i, s)| section_data(s, play.min_section + i as i32)).collect();
            play.world.chunks.insert((cx, cz), chunk);
            let n = r.varint()?;
            let mut bes = Vec::with_capacity(n.max(0) as usize);
            for _ in 0..n {
                let packed = r.u8()?;
                let y = r.i16()? as i32;
                let type_id = r.varint()? as u32;
                let (nbt, raw) = r.nbt()?;
                let x = (cx << 4) + (packed >> 4) as i32;
                let z = (cz << 4) + (packed & 15) as i32;
                bes.push(block_entity_data(ctx, play, x, y, z, type_id, &nbt, raw));
            }
            // Light follows the block entities in this same packet -- see read_light.
            let mut sections = sections;
            // Light covers one section BELOW the world and one ABOVE it, which have no blocks and
            // so no SectionData. The mirror has nowhere to put those; the world download does
            // (vanilla writes them too), so they are kept aside rather than dropped.
            let mut edge_light: Vec<(i32, Vec<u8>, Vec<u8>)> = Vec::new();
            for (sy, sky, blk) in read_light(r, play.min_section).unwrap_or_default() {
                match sections.iter_mut().find(|d| d.sy == sy) {
                    Some(sd) => {
                        sd.sky_light = sky;
                        sd.block_light = blk;
                    }
                    None => edge_light.push((sy, sky, blk)),
                }
            }
            note_monitor_geometry(play, &bes);
            if ctx.saver.is_some() {
                save_chunk(ctx, play, cx, cz, heightmaps, &sections, &edge_light, &bes);
            }
            let _ = ctx.tx.send(Update::Chunk { dimension: play.dimension.clone(), cx, cz, min_section: play.min_section, sections, block_entities: bes });
        }
        0x2a => {
            // LIGHT UPDATE. Same light payload as the chunk packet, but the coordinates are VarInts
            // here rather than the i32s 0x27 uses -- reading them as i32 silently desynchronises the
            // whole rest of the packet. Sent on its own whenever light changes without blocks
            // changing, which is what a torch does.
            //
            // The id was FOUND, not looked up: 0x28 was the guess and nothing ever arrived on it.
            // Logging the ids this match ignores and placing a glowstone showed 0x2a carrying 2074
            // bytes at exactly that moment -- 2048 of light plus masks. Protocol tables for a modded
            // 1.21.1 server are worth exactly as much as an observation that disagrees with them.
            let cx = r.varint()?;
            let cz = r.varint()?;
            let sections = read_light(r, play.min_section)?;
            if !sections.is_empty() {
                let _ = ctx.tx.send(Update::Light { cx, cz, sections });
            }
        }
        0x2b => {
            // login
            let _player_id = r.i32()?;
            let _hardcore = r.bool()?;
            let n = r.varint()?;
            for _ in 0..n {
                r.string()?;
            }
            let _max_players = r.varint()?;
            let chunk_radius = r.varint()?;
            let _sim = r.varint()?;
            let _reduced = r.bool()?;
            let _death_screen = r.bool()?;
            let _limited_crafting = r.bool()?;
            let (dim_type, dimension) = read_spawn_info(r)?;
            play.view_distance = chunk_radius;
            play.world.chunks.clear();
            play.entities.clear();
            play.entity_meta.clear();
            play.meta_dirty.clear();
            // clear_world deletes the monitor rows too, so the client's idea of what it has
            // published has to go with them -- see forget_chunk_monitors for why that matters.
            play.monitors.clear();
            play.monitor_dirty.clear();
            play.monitor_geom.clear();
            // A container is SESSION state: it belongs to this player in this dimension and does not
            // survive a respawn or a reconnect. Leaving the row would show a chest that is no longer
            // open, with contents from before the change.
            play.containers.clear();
            play.pending_open = None;
            play.pending_click = None;
            play.equipment.clear();
            let _ = ctx.tx.send(Update::CloseAllContainers);
            let _ = ctx.tx.send(Update::ClearWorld);
            apply_dimension(play, dim_type, dimension);
            info!("joined the game (server view distance {chunk_radius})");
            status(ctx, cfg, "play", "joined", Some(play));
        }
        0x2e | 0x2f | 0x30 => {
            let eid = r.varint()?;
            let (dx, dy, dz) = if id != 0x30 { (r.i16()? as f64 / 4096.0, r.i16()? as f64 / 4096.0, r.i16()? as f64 / 4096.0) } else { (0.0, 0.0, 0.0) };
            let rot = if id != 0x2e { Some((angle(r.i8()?), angle(r.i8()?))) } else { None };
            if let Some(e) = play.entities.get_mut(&eid) {
                e.x += dx;
                e.y += dy;
                e.z += dz;
                if let Some((yaw, pitch)) = rot {
                    e.yaw = yaw;
                    e.pitch = pitch;
                }
                let m = EntityMove { id: eid, x: e.x, y: e.y, z: e.z, yaw: e.yaw, pitch: e.pitch, head_yaw: e.head_yaw };
                let _ = ctx.tx.send(Update::Moves(vec![m]));
                if e.type_name == "minecraft:player" {
                    if let Some(p) = play.players.get_mut(&e.uuid) {
                        p.pos = (e.x, e.y, e.z);
                        p.rot = (e.yaw, e.pitch);
                        let _ = ctx.tx.send(Update::Players(vec![player_data(&e.uuid, p)]));
                    }
                }
            }
        }
        0x35 => {
            let v = r.i32()?;
            let mut w = Writer::with_id(0x27);
            w.i32(v);
            write_raw(wr, w.into_inner()).await?;
        }
        0x3d => {
            let n = r.varint()?;
            let mut gone = vec![];
            for _ in 0..n {
                let u = r.uuid()?;
                play.players.remove(&u);
                gone.push(u);
            }
            let _ = ctx.tx.send(Update::RemovePlayers(gone));
        }
        0x3e => {
            let actions = r.u8()?;
            let n = r.varint()?;
            let mut out = vec![];
            for _ in 0..n {
                let uuid = r.uuid()?;
                let entry = play.players.entry(uuid.clone()).or_insert_with(|| PlayerInfo { name: String::new(), gamemode: 0, latency: 0, listed: true, entity_id: None, pos: (0.0, 0.0, 0.0), rot: (0.0, 0.0) });
                if actions & 0x01 != 0 {
                    entry.name = r.string()?;
                    let np = r.varint()?;
                    for _ in 0..np {
                        r.string()?;
                        r.string()?;
                        if r.bool()? {
                            r.string()?;
                        }
                    }
                }
                if actions & 0x02 != 0 {
                    if r.bool()? {
                        r.uuid()?;
                        r.i64()?;
                        r.byte_array()?;
                        r.byte_array()?;
                    }
                }
                if actions & 0x04 != 0 {
                    entry.gamemode = r.varint()?;
                }
                if actions & 0x08 != 0 {
                    entry.listed = r.bool()?;
                }
                if actions & 0x10 != 0 {
                    entry.latency = r.varint()?;
                }
                if actions & 0x20 != 0 {
                    if r.bool()? {
                        r.nbt()?;
                    }
                }
                // link to an already-known player entity
                if entry.entity_id.is_none() {
                    if let Some((eid, e)) = play.entities.iter().find(|(_, e)| e.uuid == uuid) {
                        entry.entity_id = Some(*eid);
                        entry.pos = (e.x, e.y, e.z);
                    }
                }
                out.push(player_data(&uuid, entry));
            }
            let _ = ctx.tx.send(Update::Players(out));
        }
        0x40 => {
            // player position: accept the teleport so the server considers us placed
            let x = r.f64()?;
            let y = r.f64()?;
            let z = r.f64()?;
            let yaw = r.f32()?;
            let pitch = r.f32()?;
            let flags = r.u8()?;
            let tid = r.varint()?;
            let (bx, by, bz) = play.bot_pos;
            play.bot_pos = (if flags & 1 != 0 { bx + x } else { x }, if flags & 2 != 0 { by + y } else { y }, if flags & 4 != 0 { bz + z } else { z });
            let mut w = Writer::with_id(0x00);
            w.varint(tid);
            write_raw(wr, w.into_inner()).await?;
            // Remember the facing the server placed us with, so a walk has something to start from
            // and so nothing has to invent one.
            play.bot_look = (yaw, pitch);
            // A teleport moved us, so any walk in progress is now measuring progress from the wrong
            // place. Let it re-measure rather than counting the jump as headway.
            if let Some(w) = play.walking.as_mut() {
                w.progress_pos = play.bot_pos;
                w.last_progress = Instant::now();
            }
            // NO POSITION ECHO HERE, AND THAT IS THE FIX FOR `moved too quickly!`.
            //
            // This used to follow the accept with a `move_player_pos_rot` stating the teleport target.
            // `handleAcceptTeleportPacket` is what clears `awaitingPositionFromClient` and sets
            // `lastGood{X,Y,Z}`, so the accept ALONE is what the server requires; the echo was
            // redundant. It was also actively harmful when teleports arrive in a burst -- a respawn
            // sends three within ~20 ms -- because the echo for teleport N is then measured against
            // teleport N+1's target, and the server logs the difference as a speed violation.
            //
            // Measured: two `Powback moved too quickly! -5.849,13.0,-1.999` at the exact second of
            // `respawn into minecraft:overworld`, the delta being precisely death-position -> spawn.
            // Zero during any actual walk, because a walk's packets are one tick apart by construction.
            //
            // The bot's own position comes from the teleport we just applied, so nothing is lost: the
            // next outbound position is whatever `advance_walk` sends when the bot really moves.
            info!("bot placed at {:.1} {:.1} {:.1}", play.bot_pos.0, play.bot_pos.1, play.bot_pos.2);
        }
        0x42 => {
            let n = r.varint()?;
            let mut ids = vec![];
            for _ in 0..n {
                let eid = r.varint()?;
                play.entities.remove(&eid);
                play.entity_meta.remove(&eid);
                play.meta_dirty.remove(&eid);
                ids.push(eid);
            }
            let _ = ctx.tx.send(Update::RemoveEntities(ids));
        }
        0x3c => {
            // player_combat_kill: THE BOT IS DEAD AND NOTHING WAS ASKING TO COME BACK.
            //
            // A dead player is sent NO CHUNKS, so the mirror silently stops updating and the world it
            // holds decays into whatever it had at the moment of death. Measured on the live server:
            // `play: 2480 packets, 1 chunks, 0 entities` with `data get entity Powback Health` reading
            // `0.0f`, while the process looked perfectly healthy and kept logging mod payloads.
            //
            // This is also almost certainly what the "the replica stops sending chunks, and a restart
            // fixes it" note in CLAUDE.md was really describing: a restart respawns the player. That
            // remedy is unavailable on the live server, which must never be restarted -- so the client
            // has to ask for a respawn itself.
            // varint playerId, then the death message as a Component. The message is not read: the
            // frame is length-delimited, so leaving bytes unconsumed is harmless, and a death reason is
            // not something the mirror carries.
            let _player_id = r.varint()?;
            warn!("the bot died; requesting respawn (a dead player is sent no chunks, so the mirror stops)");
            play.respawn_asked = Some(Instant::now());
            let mut w = Writer::with_id(0x09); // serverbound client_command
            w.varint(0); // Action.PERFORM_RESPAWN
            write_raw(wr, w.into_inner()).await?;
            // Any walk dies with the player: the route was planned from a position that no longer
            // exists, and the respawn will move the bot somewhere else entirely.
            if let Some(walk) = play.walking.take() {
                let _ = ctx.tx.send(Update::CommandDone {
                    id: walk.id,
                    status: "failed".into(),
                    detail: format!("the bot died after {} steps; respawn requested", walk.steps),
                });
            }
        }
        0x5d => {
            // set_health. Kept so a fall can be refused before it kills, rather than after -- the
            // walker will happily take an 8-block drop, and five of those is a dead bot.
            let health = r.f32()?;
            play.health = health;
            // ZERO HEALTH IS THE SIGNAL, NOT THE KILL PACKET. `player_combat_kill` only arrives for a
            // death that happens while we are connected; a bot that reconnects to a body it left behind
            // is simply told `set_health 0.0` and then sent nothing else at all. Measured: the process
            // rejoined, logged "health reached 0; expecting player_combat_kill", and waited for a packet
            // that was never coming while the mirror sat on one chunk.
            if health <= 0.0 {
                let ask = play.respawn_asked.map_or(true, |t| t.elapsed() > RESPAWN_RETRY);
                if ask {
                    play.respawn_asked = Some(Instant::now());
                    warn!("health is 0 -- requesting respawn (a dead player receives no chunks)");
                    let mut w = Writer::with_id(0x09); // serverbound client_command
                    w.varint(0); // Action.PERFORM_RESPAWN
                    write_raw(wr, w.into_inner()).await?;
                }
            } else {
                play.respawn_asked = None;
            }
        }
        0x47 => {
            let (dim_type, dimension) = read_spawn_info(r)?;
            let _flags = r.u8()?;
            info!("respawn into {dimension}");
            play.world.chunks.clear();
            play.entities.clear();
            play.entity_meta.clear();
            play.meta_dirty.clear();
            // clear_world deletes the monitor rows too, so the client's idea of what it has
            // published has to go with them -- see forget_chunk_monitors for why that matters.
            play.monitors.clear();
            play.monitor_dirty.clear();
            play.monitor_geom.clear();
            // A container is SESSION state: it belongs to this player in this dimension and does not
            // survive a respawn or a reconnect. Leaving the row would show a chest that is no longer
            // open, with contents from before the change.
            play.containers.clear();
            play.pending_open = None;
            play.pending_click = None;
            play.equipment.clear();
            let _ = ctx.tx.send(Update::CloseAllContainers);
            let _ = ctx.tx.send(Update::ClearWorld);
            apply_dimension(play, dim_type, dimension);
        }
        0x48 => {
            let eid = r.varint()?;
            let hy = angle(r.i8()?);
            if let Some(e) = play.entities.get_mut(&eid) {
                e.head_yaw = hy;
                let m = EntityMove { id: eid, x: e.x, y: e.y, z: e.z, yaw: e.yaw, pitch: e.pitch, head_yaw: e.head_yaw };
                let _ = ctx.tx.send(Update::Moves(vec![m]));
            }
        }
        0x49 => {
            let (sx, sy, sz) = unpack_section_pos(r.i64()?);
            let n = r.varint()?;
            let mut changes = Vec::with_capacity(n.max(0) as usize);
            for _ in 0..n {
                let v = r.varlong()?;
                let state = (v >> 12) as u32;
                let lx = ((v >> 8) & 0xF) as i32;
                let lz = ((v >> 4) & 0xF) as i32;
                let ly = (v & 0xF) as i32;
                changes.push(((sx << 4) + lx, (sy << 4) + ly, (sz << 4) + lz, state));
            }
            block_changed(ctx, play, &changes);
        }
        0x58 => {
            // SET ENTITY DATA. Walked as data, never as meaning -- see metadata.rs for why the
            // proxy does not learn that index 17 is sheep colour.
            let eid = r.varint()?;
            let walked = metadata::walk(
                r,
                itemstack::Env {
                    items: ctx.registries.get("minecraft:item"),
                    components: ctx.registries.get("minecraft:data_component_type"),
                    enchantments: Some(&play.enchantment_names),
                    potions: ctx.registries.get("minecraft:potion"),
                    mob_effects: ctx.registries.get("minecraft:mob_effect"),
                    banner_patterns: Some(&play.banner_pattern_names),
                },
            );
            play.meta_stats.packets += 1;
            if walked.truncated {
                play.meta_stats.truncated += 1;
            }
            note_stack_components(play, &walked);
            if let Some(name) = walked.custom_name {
                let _ = ctx.tx.send(Update::EntityName { id: eid, name });
            }
            if !walked.fields.is_empty() {
                // Deltas, merged here rather than in the module: the client already tracks every
                // entity, and merging locally means a packet that only re-states what we already
                // hold costs NOTHING -- no reducer call, no row rewrite. On a table this hot that
                // suppression is the difference between carrying appearance and tripling writes.
                let now = Instant::now();
                let slot = play.entity_meta.entry(eid).or_insert_with(|| MetaState {
                    fields: Default::default(),
                    // Far enough in the past that the first packet for an entity always sends
                    // immediately -- that is the one carrying its appearance.
                    last_sent: now - META_MIN_INTERVAL,
                    dirty: false,
                });
                let mut changed = false;
                for (k, v) in walked.fields {
                    if slot.fields.get(&k) != Some(&v) {
                        slot.fields.insert(k, v);
                        changed = true;
                    }
                }
                if changed {
                    play.meta_stats.changed += 1;
                    if slot.dirty {
                        play.meta_stats.coalesced += 1;
                    }
                    slot.dirty = true;
                    play.meta_dirty.insert(eid);
                }
            }
        }
        0x5b => {
            // SET EQUIPMENT. A different packet from entity metadata and the cheapest source of real
            // item stacks: every armoured mob and everything anyone is holding.
            //
            // Repeated `(i8 flags, ItemStack)` where `flags & 0x7f` is the EquipmentSlot ordinal and
            // the top bit says another entry follows. The ordinals are `EquipmentSlot.values()`:
            // mainhand, offhand, feet, legs, chest, head, body -- BODY is new in 1.21 (horse armour),
            // so a 1.20 table would name the wrong slot for it.
            let eid = r.varint()?;
            let env = itemstack::Env {
                items: ctx.registries.get("minecraft:item"),
                components: ctx.registries.get("minecraft:data_component_type"),
                enchantments: Some(&play.enchantment_names),
                potions: ctx.registries.get("minecraft:potion"),
                mob_effects: ctx.registries.get("minecraft:mob_effect"),
                banner_patterns: Some(&play.banner_pattern_names),
            };
            let mut slots: Vec<(&'static str, Value)> = Vec::new();
            let mut stopped: Option<String> = None;
            let mut census = StackCensus::default();
            loop {
                let flags = r.i8()?;
                let name = equipment_slot_name(flags & 0x7f);
                let d = itemstack::decode(r, env);
                census.note(&d);
                let v = match &d.stack {
                    Some(st) => st.to_json(),
                    None => Value::Null,
                };
                slots.push((name, v));
                if let Some(c) = d.stopped_at {
                    // The buffer is dead from here: keep what we read and stop, exactly as the
                    // metadata walk does.
                    stopped = Some(c);
                    break;
                }
                if flags & -128i8 == 0 {
                    break;
                }
            }
            census.apply(play);
            let _ = &stopped;
            let slot = play.equipment.entry(eid).or_default();
            let mut changed = false;
            for (k, v) in slots {
                if slot.get(k) != Some(&v) {
                    slot.insert(k, v);
                    changed = true;
                }
            }
            if changed {
                let json = Value::Object(slot.iter().map(|(k, v)| (k.to_string(), v.clone())).collect());
                let _ = ctx.tx.send(Update::EntityEquipment { id: eid, equipment: json.to_string() });
            }
        }
        0x33 => {
            // OPEN SCREEN -- a container became available. NOTE the container id is a VARINT here and
            // a single BYTE in container_set_content / container_set_slot / container_close. Reading
            // it the same way in all four silently mismatches windows above 127.
            let window_id = r.varint()?;
            let menu_id = r.varint()? as u32;
            let menu_type = registry_name(ctx, "minecraft:menu", menu_id);
            let title = match r.nbt_tag() {
                Ok(Some(tag)) => crate::wire::component_tag_to_text(&tag),
                _ => String::new(),
            };
            let from = play.pending_open.as_ref().map(|p| p.pos);
            info!("container {window_id} opened: {menu_type} \"{title}\"");
            play.containers.insert(window_id, ContainerState::new(menu_type, title, from));
            // The content packet follows; publish the shell now so a subscriber sees the open
            // immediately rather than only once the slots arrive.
            publish_container(ctx, play, window_id, Vec::new(), String::new(), 0, None);
            if let Some(pending) = play.pending_open.take() {
                let _ = ctx.tx.send(Update::CommandDone { id: pending.id, status: "done".into(), detail: format!("window {window_id}") });
            }
        }
        0x13 => {
            // CONTAINER SET CONTENT: u8 window, varint stateId, list<ItemStack>, ItemStack carried.
            let window_id = r.u8()? as i32;
            let state_id = r.varint()?;
            let n = r.varint()?;
            if !(0..=2048).contains(&n) {
                bail!("implausible container slot count {n}");
            }
            let env = itemstack::Env {
                items: ctx.registries.get("minecraft:item"),
                components: ctx.registries.get("minecraft:data_component_type"),
                enchantments: Some(&play.enchantment_names),
                potions: ctx.registries.get("minecraft:potion"),
                mob_effects: ctx.registries.get("minecraft:mob_effect"),
                banner_patterns: Some(&play.banner_pattern_names),
            };
            // `env` borrows `play` immutably, so the census names are collected here and folded into
            // the stats after that borrow ends.
            let mut slots: Vec<String> = Vec::with_capacity(n as usize);
            let mut seen = StackCensus::default();
            let mut stopped: Option<String> = None;
            for _ in 0..n {
                let d = itemstack::decode(r, env);
                slots.push(match &d.stack {
                    Some(st) => st.to_json().to_string(),
                    None => String::new(),
                });
                seen.note(&d);
                if let Some(c) = d.stopped_at {
                    // One bad slot ends the LIST -- every slot after it is unreadable. They are left
                    // as "" and `undecoded_component` says why, so a consumer can tell "empty" from
                    // "we could not read this chest".
                    stopped = Some(c);
                    break;
                }
            }
            slots.resize(n as usize, String::new());
            let carried = if stopped.is_some() {
                String::new()
            } else {
                let d = itemstack::decode(r, env);
                seen.note(&d);
                match &d.stack {
                    Some(st) => st.to_json().to_string(),
                    None => String::new(),
                }
            };
            seen.apply(play);
            // Window 0 arrives with no `open_screen`, so its session entry is created here.
            play.containers.entry(window_id).or_insert_with(|| ContainerState::new(String::new(), String::new(), None));
            if let Some(st) = play.containers.get_mut(&window_id) {
                st.set_content(state_id, &slots, &carried);
            }
            publish_container(ctx, play, window_id, slots, carried, state_id, stopped);
        }
        0x15 => {
            // CONTAINER SET SLOT: i8 window, varint stateId, i16 slot, ItemStack. Slot -1 is the
            // CARRIED item, not slot index -1.
            //
            // AND THE CURSOR IS A PSEUDO-WINDOW: `ContainerSynchronizer.sendCarriedChange` sends
            // `(containerId = -1, slot = -1)`, so the correction that tells us what the bot is now
            // holding does NOT arrive on the window it came from. Measured the hard way -- a pickup
            // click emptied a chest slot and the mirror showed the stack going nowhere, because the
            // window -1 packet was being looked up as a container and silently dropped. The cursor
            // belongs to the PLAYER, and only one menu is open at a time, so it is attributed to the
            // open container (or to window 0 when that is all there is).
            let raw_window = r.i8()? as i32;
            let window_id = if raw_window == -1 {
                play.containers.keys().copied().filter(|&w| w != 0).max().unwrap_or(0)
            } else {
                raw_window
            };
            let state_id = r.varint()?;
            let slot = r.i16()? as i32;
            let env = itemstack::Env {
                items: ctx.registries.get("minecraft:item"),
                components: ctx.registries.get("minecraft:data_component_type"),
                enchantments: Some(&play.enchantment_names),
                potions: ctx.registries.get("minecraft:potion"),
                mob_effects: ctx.registries.get("minecraft:mob_effect"),
                banner_patterns: Some(&play.banner_pattern_names),
            };
            let d = itemstack::decode(r, env);
            let mut seen = StackCensus::default();
            seen.note(&d);
            let stack = match &d.stack {
                Some(st) => st.to_json().to_string(),
                None => String::new(),
            };
            seen.apply(play);
            if let Some(st) = play.containers.get_mut(&window_id) {
                st.apply_correction(state_id, slot, &stack);
            }
            if let Some(pc) = &mut play.pending_click {
                if pc.window_id == window_id {
                    pc.corrections += 1;
                }
            }
            let _ = ctx.tx.send(Update::ContainerSlot { window_id, slot, stack, state_id });
        }
        0x12 => {
            // CONTAINER CLOSE. Authoritative: the row goes, it is never marked closed. A stale open
            // container shows a chest's contents from ten minutes ago as if they were live.
            let window_id = r.u8()? as i32;
            play.containers.remove(&window_id);
            let _ = ctx.tx.send(Update::CloseContainer { window_id });
        }
        0x64 => {
            let game_time = r.i64()?;
            let day_time = r.i64()?;
            let _ = ctx.tx.send(Update::Time { dimension: play.dimension.clone(), game_time, day_time });
        }
        0x69 => {
            info!("server requested reconfiguration");
            write_raw(wr, Writer::with_id(0x0c).into_inner()).await?;
            return Ok(Some(PlayExit::Reconfigure));
        }
        0x70 => {
            let eid = r.varint()?;
            let x = r.f64()?;
            let y = r.f64()?;
            let z = r.f64()?;
            let yaw = angle(r.i8()?);
            let pitch = angle(r.i8()?);
            if let Some(e) = play.entities.get_mut(&eid) {
                e.x = x;
                e.y = y;
                e.z = z;
                e.yaw = yaw;
                e.pitch = pitch;
                let m = EntityMove { id: eid, x, y, z, yaw, pitch, head_yaw: e.head_yaw };
                let _ = ctx.tx.send(Update::Moves(vec![m]));
                if e.type_name == "minecraft:player" {
                    if let Some(p) = play.players.get_mut(&e.uuid) {
                        p.pos = (x, y, z);
                        p.rot = (yaw, pitch);
                        let _ = ctx.tx.send(Update::Players(vec![player_data(&e.uuid, p)]));
                    }
                }
            }
        }
        0x46 => {
            let uuid_hi = r.u64()?;
            let uuid_lo = r.u64()?;
            for st in [3i32, 0i32] {
                let mut w = Writer::with_id(0x2b);
                w.i64(uuid_hi as i64);
                w.i64(uuid_lo as i64);
                w.varint(st);
                write_raw(wr, w.into_inner()).await?;
            }
        }
        other => {
            // Name the ids we ignore, on request. This is how 0x2a was found: the protocol table said
            // one thing, nothing ever arrived on it, and logging the ignored ids while placing a
            // glowstone showed 2074 bytes turning up on 0x2a at exactly that moment.
            if std::env::var("MCST_TRACE_UNHANDLED").is_ok() {
                tracing::info!("unhandled play packet 0x{other:02x} ({} bytes)", r.remaining());
            }
        }
    }
    Ok(None)
}

/// Apply block changes to the local world and publish the affected sections + deltas.
fn block_changed(ctx: &mut Ctx, play: &mut Play, changes: &[(i32, i32, i32, u32)]) {
    let mut by_chunk: HashMap<(i32, i32), (Vec<i32>, Vec<BlockChangeData>)> = HashMap::new();
    for &(x, y, z, state) in changes {
        let is_air = |id: u32| ctx.block_states.is_air(id);
        if let Some((old, sy)) = play.world.set_block(x, y, z, state, is_air) {
            let e = by_chunk.entry((x >> 4, z >> 4)).or_default();
            if !e.0.contains(&sy) {
                e.0.push(sy);
            }
            e.1.push(BlockChangeData { x, y, z, old_state_id: old, new_state_id: state });
            // a block entity cannot survive a change of block
            let old_name = ctx.block_states.name(old);
            let new_name = ctx.block_states.name(state);
            if old_name != new_name {
                let _ = ctx.tx.send(Update::RemoveBlockEntities(vec![block_key(x, y, z)]));
            }
        } else {
            trace!("block change outside loaded chunks at {x},{y},{z}");
        }
    }
    for ((cx, cz), (sys, deltas)) in by_chunk {
        let chunk = match play.world.chunks.get(&(cx, cz)) {
            Some(c) => c,
            None => continue,
        };
        let sections: Vec<SectionData> = sys.iter().filter_map(|&sy| chunk.section(sy).map(|s| section_data(s, sy))).collect();
        let _ = ctx.tx.send(Update::Sections { cx, cz, sections, changes: deltas });
    }
}

pub fn block_key(x: i32, y: i32, z: i32) -> i64 {
    (((x as i64) & 0x3FF_FFFF) << 38) | (((z as i64) & 0x3FF_FFFF) << 12) | ((y as i64) & 0xFFF)
}

// keep the `error` import used when tracing levels are filtered
#[allow(dead_code)]
fn _unused() {
    error!("");
}

#[cfg(test)]
mod play_tests {
    use super::*;

    fn screen(lines: &[&str]) -> MonitorData {
        MonitorData {
            x: 71, y: 67, z: 33,
            facing: "west".into(),
            block_width: 3, block_height: 1,
            term_width: lines.first().map(|l| l.len() as u32).unwrap_or(0),
            term_height: lines.len() as u32,
            colour: true,
            cursor_x: 1, cursor_y: 1, cursor_blink: false, cursor_fg: 0, cursor_bg: 15,
            has_screen: true,
            lines: lines.iter().map(|s| s.to_string()).collect(),
            fg: lines.iter().map(|s| "0".repeat(s.len())).collect(),
            bg: lines.iter().map(|s| "f".repeat(s.len())).collect(),
            palette: vec!["#f0f0f0".into(); 16],
        }
    }

    /// The suppression that does the actual work: CC re-sends the whole screen even when nothing
    /// on it changed, and 70% of the payloads measured on the dev replica were exactly that.
    #[test]
    fn an_identical_screen_is_not_a_change() {
        let a = screen(&["hello", "world"]);
        assert!(monitor_same(Some(&a), &screen(&["hello", "world"])));
        assert!(!monitor_same(Some(&a), &screen(&["hello", "worlds"])));
        assert!(!monitor_same(None, &a), "a screen never seen before is always a change");
    }

    /// Colour alone is a change. CC writes text and colour into separate buffers, so a program that
    /// only recolours a status line sends the identical `lines` -- comparing text alone would make
    /// a red alert on a green dashboard invisible to the mirror.
    #[test]
    fn a_recolour_with_no_text_change_is_still_a_change() {
        let a = screen(&["ALERT"]);
        let mut b = screen(&["ALERT"]);
        b.fg = vec!["eeeee".into()];
        assert!(!monitor_same(Some(&a), &b));

        let mut c = screen(&["ALERT"]);
        c.palette[14] = "#ff0000".into();
        assert!(!monitor_same(Some(&a), &c), "a redefined palette entry repaints the screen");
    }

    fn stack(id: &str, n: i32) -> String {
        format!("{{\"count\":{n},\"id\":\"{id}\"}}")
    }

    /// THE DESYNC RULE. The server is authoritative and a correction overwrites, unconditionally and
    /// without merging — because a correction is what arrives when the server disagreed with a click,
    /// which is the normal case rather than the exceptional one.
    #[test]
    fn a_server_correction_overwrites_whatever_was_there() {
        let mut c = ContainerState::new("minecraft:generic_9x3".into(), "Chest".into(), None);
        c.set_content(4, &[stack("minecraft:cobblestone", 64), String::new(), stack("minecraft:dirt", 8)], "");
        assert_eq!(c.state_id, 4);

        c.apply_correction(5, 0, "");
        assert_eq!(c.slots[0], "", "a correction to empty must empty the slot, not leave the old stack");
        assert_eq!(c.state_id, 5, "the state id follows the server");

        c.apply_correction(6, 2, &stack("minecraft:diamond", 3));
        assert_eq!(c.slots[2], stack("minecraft:diamond", 3));
        assert_eq!(c.slots[1], "", "other slots are untouched");
    }

    /// Slot -1 is the CARRIED item, not index -1. Treating it as a slot index would panic or, worse,
    /// write the cursor stack into slot 0.
    #[test]
    fn slot_minus_one_is_the_cursor_not_a_slot() {
        let mut c = ContainerState::new(String::new(), String::new(), None);
        c.set_content(1, &[stack("minecraft:stone", 1)], "");
        c.apply_correction(2, -1, &stack("minecraft:diamond_sword", 1));
        assert_eq!(c.carried, stack("minecraft:diamond_sword", 1));
        assert_eq!(c.slots[0], stack("minecraft:stone", 1), "slot 0 must not have been touched");
    }

    /// A correction for a slot beyond what the content packet described GROWS the view rather than
    /// being dropped — a dropped correction reads as an empty slot, i.e. "the chest looks empty".
    #[test]
    fn a_correction_past_the_known_end_grows_the_view() {
        let mut c = ContainerState::new(String::new(), String::new(), None);
        c.set_content(1, &[String::new()], "");
        c.apply_correction(2, 3, &stack("minecraft:emerald", 5));
        assert_eq!(c.slots.len(), 4);
        assert_eq!(c.slots[3], stack("minecraft:emerald", 5));
        assert_eq!(c.slots[1], "");
    }

    /// THE POINT OF THE WHOLE DESIGN: a click predicts NOTHING, so between sending it and the server
    /// answering, the local view still holds the server's last word rather than a guess. There is
    /// nothing to be wrong about, and nothing to reconcile.
    ///
    /// Written as a sequence because that is the failure being excluded: the classic bug is a client
    /// that optimistically empties the slot it clicked, then either flickers or — if the server
    /// disagreed — keeps showing an item that is not there.
    #[test]
    fn a_click_leaves_the_local_view_at_the_servers_last_word() {
        let mut c = ContainerState::new("minecraft:generic_9x3".into(), "Chest".into(), None);
        c.set_content(7, &[stack("minecraft:cobblestone", 64), String::new()], "");
        let before: Vec<String> = c.slots.clone();

        // ... a click on slot 0 is sent here. Nothing in the click path touches the session; the only
        // mutators are `set_content` and `apply_correction`, both of which take server data.
        assert_eq!(c.slots, before, "sending a click must not move the local view");
        assert_eq!(c.carried, "", "nor the cursor");

        // The server's answer: the stack moved to the cursor.
        c.apply_correction(8, 0, "");
        c.apply_correction(8, -1, &stack("minecraft:cobblestone", 64));
        assert_eq!(c.slots[0], "");
        assert_eq!(c.carried, stack("minecraft:cobblestone", 64));
        assert_eq!(c.state_id, 8);
    }

    /// The subtlety that a single click cannot show and two clicks can.
    ///
    /// The click claims an empty cursor, and the server only corrects where reality differs from the
    /// claim — so when the cursor really does end up empty, NO correction arrives. Adopting the claim
    /// is what makes that silence mean "empty" instead of "unchanged". Without it, `carried` keeps the
    /// stack from the previous click forever: pick up, put back, and the cursor never empties.
    #[test]
    fn a_click_adopts_the_empty_cursor_it_claimed() {
        let mut c = ContainerState::new(String::new(), String::new(), None);
        c.set_content(1, &[String::new()], &stack("minecraft:cobblestone", 64));
        assert_eq!(c.carried, stack("minecraft:cobblestone", 64));

        // Sending a click claims `carriedItem = EMPTY`; the session adopts that claim.
        c.carried = String::new();

        // The server put the stack into slot 0 and the cursor really is empty, so it corrects the slot
        // and says NOTHING about the cursor.
        c.apply_correction(2, 0, &stack("minecraft:cobblestone", 64));
        assert_eq!(c.slots[0], stack("minecraft:cobblestone", 64));
        assert_eq!(c.carried, "", "silence about the cursor must mean empty, because empty is what we claimed");
    }

    /// Unloading a chunk deletes the monitor rows in it, so the client must stop believing it has
    /// published them -- otherwise the identical screen arriving after the chunk reloads compares
    /// equal to the deleted row and is suppressed, and the monitor never gets a row back.
    #[test]
    fn forgetting_a_chunk_drops_everything_it_knew_about_that_chunk() {
        let mut monitors: HashMap<(i32, i32, i32), MonitorSlot> = HashMap::new();
        let mut dirty: std::collections::HashSet<(i32, i32, i32)> = std::collections::HashSet::new();
        let mut geom: HashMap<(i32, i32, i32), (u32, u32)> = HashMap::new();
        // 71,67,33 is chunk 4,2; 57,67,30 is chunk 3,1 and must survive
        for pos in [(71, 67, 33), (57, 67, 30)] {
            monitors.insert(pos, MonitorSlot { pending: None, published: Some(screen(&["x"])), last_sent: Instant::now() });
            dirty.insert(pos);
            geom.insert(pos, (3, 1));
        }
        forget_chunk_monitors(&mut monitors, &mut dirty, &mut geom, 4, 2);
        assert!(!monitors.contains_key(&(71, 67, 33)));
        assert!(!dirty.contains(&(71, 67, 33)));
        assert!(!geom.contains_key(&(71, 67, 33)));
        assert!(monitors.contains_key(&(57, 67, 30)), "a monitor in another chunk is untouched");
        assert!(geom.contains_key(&(57, 67, 30)));
    }
}

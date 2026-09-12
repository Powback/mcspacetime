//! mcspacetime bot: a headless Minecraft 1.21.1 client (native protocol, NeoForge-aware) that
//! mirrors what it sees into SpacetimeDB.

mod anvil;
mod blockstates;
mod computercraft;
mod itemstack;
mod metadata;
mod module_bindings;
mod neoforge;
mod path;
mod protocol;
mod publisher;
mod scope;
mod walk;
mod wire;
mod world;

use anyhow::{Context, Result};
use protocol::{Auth, Config, Ctx, SessionEnd};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{error, info, warn};

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let data_dir = PathBuf::from(env("DATA_DIR", "data"));
    let cfg = Config {
        addr: env("MC_ADDR", "127.0.0.1:25565"),
        username: env("MC_USERNAME", "spacetime"),
        auth: match env("MC_AUTH", "offline").as_str() {
            "microsoft" => Auth::Microsoft { email: env("MC_MS_EMAIL", ""), cache_file: data_dir.join("ms-auth.json") },
            _ => Auth::Offline,
        },
        channels_path: PathBuf::from(env("NEOFORGE_CHANNELS_JSON", &data_dir.join("neoforge_channels.json").to_string_lossy())),
        view_distance: env("MC_VIEW_DISTANCE", "12").parse().unwrap_or(12),
    };
    let stdb_uri = env("STDB_URI", "http://127.0.0.1:3200");
    // Default the database to the SERVER, so two servers can never share a mirror by accident --
    // see scope.rs for why a per-row world column would not have been enough. STDB_MODULE still
    // overrides, which is how you deliberately point two addresses at one database.
    let stdb_db = env("STDB_MODULE", &scope::database_name(&cfg.addr));
    let blockstates_path = PathBuf::from(env("BLOCKSTATES_JSON", &data_dir.join("blockstates.json").to_string_lossy()));

    let mut block_states = blockstates::BlockStates::load(&blockstates_path)?;
    // Authoritative passability for every state in the pack, including mods. Optional: without it the
    // walker falls back to guessing from block names, which treats every unrecognised modded block as
    // solid and so quietly walls off routes the bot could really walk.
    block_states.load_collision(&PathBuf::from(env("COLLISION_JSON", &data_dir.join("collision.json").to_string_lossy())));
    let channels = neoforge::Channels::load(&cfg.channels_path);

    info!("connecting to SpacetimeDB {stdb_uri} database {stdb_db}");
    let conn = publisher::connect(&stdb_uri, &stdb_db).context("SpacetimeDB")?;
    let (tx, rx) = std::sync::mpsc::channel();
    // Commands travel the OTHER way: SpacetimeDB -> packet loop. Unbounded is fine, a human or a
    // browser produces these one at a time.
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new().name("stdb-publisher".into()).spawn(move || publisher::run(conn, rx, cmd_tx))?;

    // World download: off unless asked for, so the bot's normal job is unaffected. Its own thread
    // behind a BOUNDED channel (publisher.rs's unbounded one can absorb a slow reducer; a slow
    // disk must never push back into the packet loop) -- see anvil.rs.
    let saver = match env("WORLD_SAVE", "").as_str() {
        "1" | "true" | "yes" | "on" => {
            // Region files are updated IN PLACE and only the dimension is in the path, so a
            // second server would interleave its chunks into the first one's .mca files. Scope the
            // root by server: a corrupted backup is worse than a duplicated one.
            let default_dir = data_dir.join("worlds").join(scope::server_slug(&cfg.addr));
            let dir = PathBuf::from(env("WORLD_SAVE_DIR", &default_dir.to_string_lossy()));
            let (stx, srx) = std::sync::mpsc::sync_channel(512);
            std::thread::Builder::new().name("anvil-saver".into()).spawn(move || anvil::run(dir, srx))?;
            Some(stx)
        }
        _ => None,
    };

    let mut ctx = Ctx { tx, block_states, channels, block_states_published: false, registries: Default::default(), saver, save_dropped: 0, commands: cmd_rx };

    let mut stuck_rounds = 0;
    let max_rounds: u32 = env("NEGOTIATION_MAX_ROUNDS", "16").parse().unwrap_or(16);
    let mut rounds = 0u32;
    loop {
        let end = protocol::run_session(&cfg, &mut ctx).await;
        match end {
            SessionEnd::NegotiationProgress(summary) => {
                rounds += 1;
                stuck_rounds = 0;
                info!("negotiation round {rounds}: learned from {} reasons; reconnecting", summary.len());
                if rounds > max_rounds {
                    error!("giving up after {rounds} negotiation rounds");
                    std::process::exit(2);
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            SessionEnd::NegotiationStuck(summary) => {
                stuck_rounds += 1;
                error!("negotiation failed and nothing new could be learned ({} reasons):", summary.len());
                for s in &summary {
                    error!("  {s}");
                }
                if stuck_rounds == 1 {
                    // maybe the server's CONFIGURATION channels changed; re-learn from scratch
                    warn!("resetting the learned NeoForge channel list and retrying");
                    ctx.channels = neoforge::Channels::with_builtins();
                } else if stuck_rounds >= 3 {
                    error!("stuck; exiting so the failure is visible");
                    std::process::exit(2);
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            SessionEnd::Disconnected(reason) => {
                warn!("disconnected: {reason}");
                // A container belongs to a LIVE session. Clearing it only on the next connect would
                // leave a chest's contents sitting in the mirror, looking open, for as long as the
                // bot stays down -- which is the exact stale-state the table is designed to refuse.
                let _ = ctx.tx.send(publisher::Update::CloseAllContainers);
                let _ = ctx.tx.send(publisher::Update::BotStatus {
                    server_addr: cfg.addr.clone(),
                    username: cfg.username.clone(),
                    state: "disconnected".into(),
                    detail: reason.clone(),
                    dimension: String::new(),
                    pos: (0.0, 0.0, 0.0),
                    view_distance: 0,
                    chunks_loaded: 0,
                    packets_seen: 0,
                });
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            SessionEnd::Error(e) => {
                error!("session error: {e:#}");
                let _ = ctx.tx.send(publisher::Update::CloseAllContainers);
                let _ = ctx.tx.send(publisher::Update::BotStatus {
                    server_addr: cfg.addr.clone(),
                    username: cfg.username.clone(),
                    state: "disconnected".into(),
                    detail: format!("{e:#}"),
                    dimension: String::new(),
                    pos: (0.0, 0.0, 0.0),
                    view_distance: 0,
                    chunks_loaded: 0,
                    packets_seen: 0,
                });
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

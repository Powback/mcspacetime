//! NeoForge 21.1 network negotiation, implemented client-side from the NeoForge sources
//! (see README "NeoForge handshake").
//!
//! Summary of the mechanism:
//!  * Right after LoginAcknowledged the server sends `minecraft:unregister`, `minecraft:register`,
//!    a `neoforge:register` QUERY (empty map) and a `minecraft:ping(0)`, then waits.
//!  * If the client answers the query with a `neoforge:register` payload listing its channels
//!    (per protocol: id, version, optional flow, optional flag) BEFORE the pong, the connection
//!    becomes ConnectionType.NEOFORGE and `NetworkComponentNegotiator` matches the lists.
//!  * Every non-optional server channel must be present with the exact version string and flow.
//!    Failures are reported to the client in `neoforge:modded_network_setup_failed` (a map of
//!    channel id -> translatable Component) just before the disconnect, which is what lets this
//!    module DISCOVER the server's channel list by probing: connect with what we know, read the
//!    failure reasons, fix the list, reconnect. Usually 3 rounds for CONFIGURATION and 3 for PLAY.
//!  * After negotiation the configuration phase runs NeoForge's tasks: frozen registry sync
//!    (id<->name maps for block/entity_type/... — we WANT these), known data maps (reply empty),
//!    extensible enums (ack), feature flags (ack). Each blocks until the client's reply.

use crate::wire::{component_translate_args, Reader, Writer};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use tracing::{info, warn};

pub const CH_QUERY: &str = "neoforge:register";
pub const CH_NETWORK: &str = "neoforge:network";
pub const CH_SETUP_FAILED: &str = "neoforge:modded_network_setup_failed";
pub const CH_FROZEN_START: &str = "neoforge:frozen_registry_sync_start";
pub const CH_FROZEN: &str = "neoforge:frozen_registry";
pub const CH_FROZEN_DONE: &str = "neoforge:frozen_registry_sync_completed";
pub const CH_DATAMAPS: &str = "neoforge:known_registry_data_maps";
pub const CH_DATAMAPS_REPLY: &str = "neoforge:known_registry_data_maps_reply";
pub const CH_ENUMS: &str = "neoforge:extensible_enum_data";
pub const CH_ENUMS_ACK: &str = "neoforge:extensible_enum_ack";
pub const CH_FLAGS: &str = "neoforge:feature_flags";
pub const CH_FLAGS_ACK: &str = "neoforge:feature_flags_ack";

/// ConnectionProtocol ordinals (vanilla enum order: HANDSHAKING, PLAY, STATUS, LOGIN, CONFIGURATION)
pub const PROTO_PLAY: i32 = 1;
pub const PROTO_CONFIG: i32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Flow {
    Serverbound,
    Clientbound,
}

impl Flow {
    fn ordinal(self) -> i32 {
        match self {
            Flow::Serverbound => 0,
            Flow::Clientbound => 1,
        }
    }
    fn from_name(s: &str) -> Option<Flow> {
        match s.trim().to_ascii_uppercase().as_str() {
            "SERVERBOUND" => Some(Flow::Serverbound),
            "CLIENTBOUND" => Some(Flow::Clientbound),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Channel {
    pub version: String,
    pub flow: Option<Flow>,
    pub optional: bool,
}

/// Learned channel lists, persisted between runs (data/neoforge_channels.json).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Channels {
    pub configuration: BTreeMap<String, Channel>,
    pub play: BTreeMap<String, Channel>,
    /// set once a full join succeeded with this list
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub server_version: String,
    /// the CONFIGURATION list has been accepted by the server at least once (so a later failure
    /// batch of unknown ids is about PLAY)
    #[serde(default)]
    pub configuration_complete: bool,
    /// Channel ids announced with a plain `minecraft:register` right after our query reply, so
    /// NeoForge's `hasChannel(id)` is true for them via its ad-hoc channel set. Needed for
    /// Fabric-API-style configuration tasks (forgified-fabric-api mods such as spell_engine kick
    /// with "Network configuration task not supported: <id>" when `canSend` is false). Learned
    /// from that kick message.
    #[serde(default)]
    pub adhoc: Vec<String>,
    /// per-connection: set once the server accepted our CONFIGURATION list (so further unknown
    /// failures refer to PLAY channels). Not persisted.
    #[serde(skip)]
    pub config_passed_hint: bool,
}

impl Channels {
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(b) => match serde_json::from_slice::<Channels>(&b) {
                Ok(mut c) => {
                    c.add_builtins();
                    info!("loaded {} config / {} play NeoForge channels from {}", c.configuration.len(), c.play.len(), path.display());
                    c
                }
                Err(e) => {
                    warn!("ignoring unreadable {}: {e}", path.display());
                    Self::with_builtins()
                }
            },
            Err(_) => Self::with_builtins(),
        }
    }

    pub fn save(&self, path: &Path) {
        // Discovery/CI can freeze the channel file so an external loop owns it.
        if std::env::var("MCSPACETIME_CHANNELS_READONLY").as_deref() == Ok("1") {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(self).unwrap()) {
            warn!("could not save {}: {e}", path.display());
        }
    }

    pub fn with_builtins() -> Self {
        let mut c = Channels::default();
        c.add_builtins();
        c
    }

    /// NeoForge's own optional channels we want to take part in (all registered by NeoForge with
    /// version "1"). Optional => if the server lacks one, it is silently dropped.
    fn add_builtins(&mut self) {
        let b = |flow: Option<Flow>| Channel { version: "1".into(), flow, optional: true };
        for (id, flow) in [
            (CH_FROZEN_START, Some(Flow::Clientbound)),
            (CH_FROZEN, Some(Flow::Clientbound)),
            (CH_FROZEN_DONE, None),
            (CH_DATAMAPS, Some(Flow::Clientbound)),
            (CH_DATAMAPS_REPLY, Some(Flow::Serverbound)),
            (CH_ENUMS, Some(Flow::Clientbound)),
            (CH_ENUMS_ACK, Some(Flow::Serverbound)),
            (CH_FLAGS, Some(Flow::Clientbound)),
            (CH_FLAGS_ACK, Some(Flow::Serverbound)),
        ] {
            self.configuration.entry(id.to_string()).or_insert_with(|| b(flow));
        }
    }

    /// Serverbound `neoforge:register` (ModdedNetworkQueryPayload) body.
    pub fn encode_query(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.varint(2);
        for (proto, list) in [(PROTO_CONFIG, &self.configuration), (PROTO_PLAY, &self.play)] {
            w.varint(proto);
            w.varint(list.len() as i32);
            for (id, ch) in list {
                w.string(id);
                w.string(&ch.version);
                match ch.flow {
                    Some(f) => {
                        w.bool(true);
                        w.varint(f.ordinal());
                    }
                    None => {
                        w.bool(false);
                    }
                }
                w.bool(ch.optional);
            }
        }
        w.into_inner()
    }

    /// Apply `neoforge:modded_network_setup_failed` reasons. Returns a human-readable summary and
    /// whether anything changed (if nothing changed we cannot make progress).
    ///
    /// NeoForge negotiates CONFIGURATION first and aborts on the first failing protocol, so one
    /// failure batch is about exactly one protocol. Which one is inferred: a batch mentioning a
    /// channel we already list for configuration is a configuration batch — unless the reason is
    /// "server has it, client doesn't", which for an id we DO list means the same id is also a
    /// PLAY channel (mods often register one id for both). A batch of only-unknown ids is a PLAY
    /// batch once our configuration list holds settled (non-optional, known-version) channels.
    pub fn learn_from_failure(&mut self, body: &[u8]) -> Result<(Vec<String>, bool)> {
        let mut r = Reader::new(body);
        let count = r.varint()?;
        let mut reasons: Vec<(String, String, Vec<String>)> = vec![];
        for _ in 0..count {
            let id = r.string()?;
            let tag = simdnbt::owned::read_optional_tag(&mut r.cur).map_err(|_| anyhow::anyhow!("reason nbt: unparseable"))?;
            let mut keys = vec![];
            let mut args = vec![];
            if let Some(t) = &tag {
                component_translate_args(t, &mut keys, &mut args);
            }
            let key = keys.iter().find(|k| k.contains("negotiation.failure") && !k.ends_with(".mod")).cloned().unwrap_or_default();
            reasons.push((id, key, args));
        }
        let summary: Vec<String> = reasons.iter().map(|(id, k, a)| format!("{id}: {k} {a:?}")).collect();

        let all_missing_sc = reasons.iter().all(|(_, k, _)| k.ends_with("missing.server.client"));
        let config_settled = self.configuration.values().any(|c| !c.optional && c.version != "?");
        let none_in_play = reasons.iter().all(|(id, _, _)| !self.play.contains_key(id));
        let batch_is_play = self.config_passed_hint || (all_missing_sc && config_settled && none_in_play);
        if batch_is_play {
            self.config_passed_hint = true;
            self.configuration_complete = true;
        }
        info!("negotiation failure batch of {} reasons classified as {}", reasons.len(), if batch_is_play { "PLAY" } else { "CONFIGURATION" });

        let mut changed = false;
        for (id, key, args) in reasons {
            let k = key.as_str();
            let (target, other) = if batch_is_play { (&mut self.play, &self.configuration) } else { (&mut self.configuration, &self.play) };
            if k.ends_with("missing.server.client") {
                if !target.contains_key(&id) {
                    // guess: same version/flow the other protocol uses for this id, else "1"
                    let guess = other.get(&id).cloned().unwrap_or(Channel { version: "1".into(), flow: None, optional: false });
                    target.insert(id.clone(), Channel { optional: false, ..guess });
                    changed = true;
                }
            } else if k.ends_with("missing.client.server") {
                if target.remove(&id).is_some() {
                    changed = true;
                }
            } else if k.contains("flow.client.missing") || k.contains("flow.client.mismatch") {
                // args: [server flow] or [server flow, client flow] (possibly prefixed by a mod name)
                let n = args.len();
                let server_flow = if k.contains("mismatch") { args.get(n.wrapping_sub(2)) } else { args.last() };
                if let Some(f) = server_flow.and_then(|s| Flow::from_name(s)) {
                    if let Some(ch) = target.get_mut(&id) {
                        if ch.flow != Some(f) {
                            ch.flow = Some(f);
                            changed = true;
                        }
                    }
                }
            } else if k.contains("flow.server.missing") {
                if let Some(ch) = target.get_mut(&id) {
                    if ch.flow.is_some() {
                        ch.flow = None;
                        changed = true;
                    }
                }
            } else if k.contains("version.mismatch") {
                // validateComponent(server, client, "client") runs first => args [server, client]
                let n = args.len();
                if let (Some(server_v), Some(ch)) = (args.get(n.wrapping_sub(2)), target.get_mut(&id)) {
                    if &ch.version != server_v {
                        ch.version = server_v.clone();
                        changed = true;
                    }
                }
            } else {
                warn!("unhandled negotiation failure for {id}: {key} {args:?}");
            }
        }
        Ok((summary, changed))
    }

    /// Parse the server's `neoforge:network` (ModdedNetworkPayload) — the negotiated setup —
    /// and note which protocol lists are now confirmed.
    pub fn parse_network(&mut self, body: &[u8]) -> Result<BTreeMap<i32, BTreeMap<String, String>>> {
        let mut r = Reader::new(body);
        let mut out = BTreeMap::new();
        let n = r.varint()?;
        for _ in 0..n {
            let proto = r.varint()?;
            let m = r.varint()?;
            let mut chans = BTreeMap::new();
            for _ in 0..m {
                let _key = r.string()?;
                let id = r.string()?;
                let version = r.string()?;
                chans.insert(id, version);
            }
            out.insert(proto, chans);
        }
        self.config_passed_hint = true;
        self.configuration_complete = true;
        Ok(out)
    }
}

// serde skips this; it is per-connection state set by the session
impl Channels {
    pub fn set_config_passed(&mut self, v: bool) {
        self.config_passed_hint = v;
    }
}

/// Body of `minecraft:register`: NUL-separated ids, no length prefix.
pub fn encode_minecraft_register(ids: &[String]) -> Vec<u8> {
    let mut v = Vec::new();
    for id in ids {
        v.extend_from_slice(id.as_bytes());
        v.push(0);
    }
    v
}

/// Ad-hoc channel candidates to register for a Fabric-API configuration TASK id. The kick names
/// the task id (e.g. spell_engine:config); the payload the task sends is <task>_sync by Fabric
/// convention, and the client replies on <ns>:ack.
pub fn adhoc_candidates(task_id: &str) -> Vec<String> {
    let mut v = vec![task_id.to_string(), format!("{task_id}_sync")];
    if let Some((ns, _)) = task_id.split_once(':') {
        v.push(format!("{ns}:ack"));
    }
    v
}

/// "Network configuration task not supported: <id>" (Fabric API wording, via forgified-fabric-api)
pub fn unsupported_task_channel(reason: &str) -> Option<String> {
    let marker = "Network configuration task not supported: ";
    reason.find(marker).map(|i| reason[i + marker.len()..].split_whitespace().next().unwrap_or("").trim_end_matches(')').to_string()).filter(|s| s.contains(':'))
}

/// Parsed `neoforge:frozen_registry` payload: registry name and id -> key
pub fn parse_frozen_registry(body: &[u8]) -> Result<(String, Vec<(u32, String)>)> {
    let mut r = Reader::new(body);
    let name = r.string()?;
    let n = r.varint()?;
    let mut ids = Vec::with_capacity(n.max(0) as usize);
    for _ in 0..n {
        let id = r.varint()? as u32;
        let key = r.string()?;
        ids.push((id, key));
    }
    // aliases follow; not needed
    Ok((name, ids))
}

pub fn parse_frozen_start(body: &[u8]) -> Result<Vec<String>> {
    let mut r = Reader::new(body);
    let n = r.varint()?;
    let mut v = vec![];
    for _ in 0..n {
        v.push(r.string()?);
    }
    Ok(v)
}

//! The one table a protocol client cannot derive from the wire: global block-state id ->
//! block + properties. NeoForge assigns state ids by flattening (blocks in registry-id order) x
//! (each block's possible states, in the block's own property order); the second factor is
//! compiled Java, so it comes from a dump made on a server running the same mod jars
//! (tools/kubejs_dump_registries.js). At connect time the live server's block registry (from the
//! NeoForge frozen-registry sync) is used to re-flatten, so a different registry order on the live
//! server is handled; only a block missing from the dump is unrecoverable.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Deserialize)]
struct Dump {
    count: usize,
    states: Vec<String>,
}

/// `collision.json` from `tools/kubejs_dump_collision.js`: which states have an EMPTY collision shape,
/// as inclusive id ranges over the dump's own ordering.
///
/// `f64` because KubeJS's `JsonIO.write` emits every number as a float (`[0.0, 0.0]`), so an integer
/// type here fails to parse the file the tool actually produces.
#[derive(Deserialize)]
struct CollisionDump {
    count: usize,
    empty: Vec<(f64, f64)>,
}

pub struct BlockStates {
    /// per block name, its states' property strings in order ("" for a property-less block)
    pub per_block: Vec<(String, Vec<String>)>,
    pub by_name: HashMap<String, usize>,
    /// flattened table currently in force: id -> (name, props). Behind an `Arc` so the world
    /// downloader's thread can share it without copying 344 003 pairs of strings.
    pub table: Arc<Vec<(String, String)>>,
    pub air: HashSet<u32>,
    /// Per block, per state: does this state have an EMPTY collision shape?
    ///
    /// Stored parallel to `per_block` rather than as a flat id table ON PURPOSE. `flatten` renumbers
    /// every state id when the live registry order differs from the dump's -- which happens on this
    /// pack -- so a table keyed by the DUMP's ids would silently point at the wrong blocks. Keyed by
    /// (block, state index) it survives the renumbering, because that is the pairing the dump and the
    /// live server actually agree on.
    per_block_no_collision: Vec<Vec<bool>>,
    /// The flattened version of the above, rebuilt by `flatten` beside `table`. Empty when no
    /// collision dump was loaded, which every reader must treat as "unknown", never as "solid".
    no_collision: Arc<Vec<bool>>,
}

impl BlockStates {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let dump: Dump = serde_json::from_slice(&raw).context("parsing blockstates.json")?;
        let mut per_block: Vec<(String, Vec<String>)> = vec![];
        let mut by_name: HashMap<String, usize> = HashMap::new();
        for s in &dump.states {
            let (name, props) = match s.find('[') {
                Some(i) => (&s[..i], s[i + 1..s.len() - 1].to_string()),
                None => (s.as_str(), String::new()),
            };
            match by_name.get(name) {
                Some(&idx) if idx + 1 == per_block.len() => per_block[idx].1.push(props),
                Some(_) => anyhow::bail!("block {name} appears twice non-contiguously in the dump"),
                None => {
                    by_name.insert(name.to_string(), per_block.len());
                    per_block.push((name.to_string(), vec![props]));
                }
            }
        }
        info!("block-state dump: {} states, {} blocks", dump.count, per_block.len());
        let mut bs = BlockStates {
            per_block,
            by_name,
            table: Arc::new(vec![]),
            air: HashSet::new(),
            per_block_no_collision: vec![],
            no_collision: Arc::new(vec![]),
        };
        bs.flatten(None);
        Ok(bs)
    }

    /// Rebuild the id table. With a live registry (id -> block name, dense from 0) the blocks are
    /// re-flattened in the live order; without, the dump order is used.
    pub fn flatten(&mut self, live_registry: Option<&[(u32, String)]>) {
        let mut table = vec![];
        // Indices into `per_block`, in the order the ids will be emitted, so the collision table can be
        // rebuilt in lockstep below. `None` is a live block missing from the dump (assumed 1 state).
        let mut order_idx: Vec<Option<usize>> = vec![];
        let order: Vec<&(String, Vec<String>)> = match live_registry {
            Some(reg) => {
                let mut sorted = reg.to_vec();
                sorted.sort_by_key(|(id, _)| *id);
                let mut v = vec![];
                let mut missing = 0;
                for (id, name) in &sorted {
                    if *id as usize != v.len() {
                        warn!("block registry is not dense at id {id} ({name}); state ids may be off");
                    }
                    match self.by_name.get(name) {
                        Some(&i) => {
                            order_idx.push(Some(i));
                            v.push(&self.per_block[i]);
                        }
                        None => {
                            order_idx.push(None);
                            missing += 1;
                            warn!("block {name} (id {id}) is not in the dump — assuming 1 state; re-run the dump");
                        }
                    }
                }
                if missing > 0 {
                    warn!("{missing} live blocks missing from the dump");
                }
                v
            }
            None => {
                order_idx = (0..self.per_block.len()).map(Some).collect();
                self.per_block.iter().collect()
            }
        };
        for (name, props) in order {
            for p in props {
                table.push((name.clone(), p.clone()));
            }
        }
        self.air.clear();
        for (i, (name, _)) in table.iter().enumerate() {
            if name == "minecraft:air" || name == "minecraft:cave_air" || name == "minecraft:void_air" {
                self.air.insert(i as u32);
            }
        }
        self.table = Arc::new(table);
        // Rebuild the collision table through the SAME ordering, so id N in `table` and id N here are
        // the same state. A block with no dumped collision contributes `false` (unknown -> not known to
        // be empty), which keeps the name heuristic in charge for it.
        if !self.per_block_no_collision.is_empty() {
            let mut flat = Vec::with_capacity(self.table.len());
            for slot in &order_idx {
                match slot.and_then(|i| self.per_block_no_collision.get(i)) {
                    Some(v) => flat.extend_from_slice(v),
                    // Missing from the dump: one state, and nothing known about its shape.
                    None => flat.push(false),
                }
            }
            flat.resize(self.table.len(), false);
            self.no_collision = Arc::new(flat);
        }
    }

    /// Attach a collision dump. Never fatal: without it `collision_empty` simply answers `None` and the
    /// caller falls back to the name heuristic, which is how this worked before the dump existed.
    pub fn load_collision(&mut self, path: &Path) {
        let raw = match std::fs::read(path) {
            Ok(r) => r,
            Err(e) => {
                warn!("no collision dump at {} ({e}); falling back to block-name passability, which treats every unrecognised modded block as solid", path.display());
                return;
            }
        };
        let dump: CollisionDump = match serde_json::from_slice(&raw) {
            Ok(d) => d,
            Err(e) => {
                warn!("collision dump at {} is unreadable ({e}); falling back to block-name passability", path.display());
                return;
            }
        };
        let total: usize = self.per_block.iter().map(|(_, s)| s.len()).sum();
        if dump.count != total {
            // A dump from different jars would be keyed to a different flattening, and applying it
            // would mark arbitrary blocks walk-through -- the one direction that desyncs the bot.
            warn!("collision dump has {} states but the block-state dump has {total}; ignoring it (re-run BOTH dumps from the same jars)", dump.count);
            return;
        }
        // Expand the ranges into a flat lookup over the DUMP's ordering, then split it per block so it
        // survives `flatten` renumbering the ids.
        let mut empty = vec![false; dump.count];
        for (a, b) in &dump.empty {
            let (a, b) = (*a as usize, *b as usize);
            for slot in empty.iter_mut().take((b + 1).min(dump.count)).skip(a) {
                *slot = true;
            }
        }
        let mut per_block_no_collision = Vec::with_capacity(self.per_block.len());
        let mut at = 0usize;
        for (_, states) in &self.per_block {
            per_block_no_collision.push(empty[at..at + states.len()].to_vec());
            at += states.len();
        }
        let count = empty.iter().filter(|b| **b).count();
        info!("collision dump: {count} of {} states have an empty collision shape", dump.count);
        self.per_block_no_collision = per_block_no_collision;
        // Re-run the current flattening so `no_collision` is populated for the ids in force.
        self.flatten(None);
    }

    /// Does this state have an empty collision shape -- can a player's body occupy it?
    ///
    /// `None` means "no collision dump loaded", which is NOT "solid". Callers combine this with the
    /// name heuristic rather than replacing it: collision-empty is authoritative for things you can walk
    /// THROUGH, but a ladder has a real (thin) collision shape and is still somewhere a player stands,
    /// so collision alone would wrongly wall one off. Verified against the live table: `grass_block` is
    /// correctly non-empty, and so is `ladder`.
    pub fn collision_empty(&self, id: u32) -> Option<bool> {
        if self.no_collision.is_empty() {
            return None;
        }
        self.no_collision.get(id as usize).copied()
    }

    /// Does the dump's implied registry order equal the live one?
    pub fn matches_registry(&self, live: &[(u32, String)]) -> bool {
        let mut sorted = live.to_vec();
        sorted.sort_by_key(|(id, _)| *id);
        sorted.len() == self.per_block.len() && sorted.iter().zip(self.per_block.iter()).all(|((_, a), (b, _))| a == b)
    }

    pub fn count(&self) -> usize {
        self.table.len()
    }

    /// ceil(log2(count)) — the width NeoForge uses for direct block palettes on this server
    pub fn direct_bits(&self) -> u8 {
        let n = self.table.len().max(2);
        (usize::BITS - (n - 1).leading_zeros()) as u8
    }

    pub fn is_air(&self, id: u32) -> bool {
        self.air.contains(&id)
    }

    pub fn name(&self, id: u32) -> Option<&str> {
        self.table.get(id as usize).map(|(n, _)| n.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a pair of dumps to a temp dir and load them.
    fn fixture(dir: &Path, states: &[&str], empty: &[(f64, f64)]) -> BlockStates {
        let bs_path = dir.join("blockstates.json");
        let col_path = dir.join("collision.json");
        std::fs::write(
            &bs_path,
            serde_json::to_vec(&serde_json::json!({ "count": states.len(), "states": states })).unwrap(),
        )
        .unwrap();
        std::fs::write(
            &col_path,
            serde_json::to_vec(&serde_json::json!({ "count": states.len(), "empty": empty })).unwrap(),
        )
        .unwrap();
        let mut bs = BlockStates::load(&bs_path).unwrap();
        bs.load_collision(&col_path);
        bs
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mcst-blockstates-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn COLLISION_SURVIVES_THE_LIVE_REGISTRY_REORDERING() {
        // THE HAZARD THIS FEATURE LIVES OR DIES ON. `flatten` renumbers every state id when the live
        // block registry order differs from the dump's -- and it DOES on this pack ("live block registry
        // differs from the dump (9740 live vs 9740 dumped blocks); re-flattening state ids in live
        // order"). A collision table keyed by the dump's ids would then describe the wrong blocks, and
        // the failure is silent: arbitrary blocks become walk-through and the bot desyncs from the
        // server with nothing in the log tying it to the dump.
        //
        // Dump order: solid(1 state), plant(2 states), stone(1 state). Only the plant's states are empty.
        let dir = temp_dir("reorder");
        let bs = fixture(
            &dir,
            &["m:solid", "m:plant[age=0]", "m:plant[age=1]", "m:stone"],
            &[(1.0, 2.0)],
        );
        assert_eq!(bs.collision_empty(0), Some(false), "solid");
        assert_eq!(bs.collision_empty(1), Some(true), "plant age=0");
        assert_eq!(bs.collision_empty(2), Some(true), "plant age=1");
        assert_eq!(bs.collision_empty(3), Some(false), "stone");

        // Now the live server presents the SAME blocks in a different order: stone, plant, solid.
        let mut bs = bs;
        let live = [(0u32, "m:stone".to_string()), (1, "m:plant".to_string()), (2, "m:solid".to_string())];
        bs.flatten(Some(&live));
        // Ids are now stone=0, plant=1..2, solid=3 -- and collision must have followed the BLOCKS.
        assert_eq!(bs.name(0), Some("m:stone"));
        assert_eq!(bs.name(1), Some("m:plant"));
        assert_eq!(bs.name(3), Some("m:solid"));
        assert_eq!(bs.collision_empty(0), Some(false), "stone must not become passable");
        assert_eq!(bs.collision_empty(1), Some(true), "plant age=0 after reorder");
        assert_eq!(bs.collision_empty(2), Some(true), "plant age=1 after reorder");
        assert_eq!(bs.collision_empty(3), Some(false), "solid must not become passable");
    }

    #[test]
    fn a_mismatched_collision_dump_is_refused_rather_than_applied() {
        // A dump from different jars is keyed to a different flattening. Applying it would mark
        // arbitrary blocks walk-through, so the only safe answer is to ignore it and fall back to names.
        let dir = temp_dir("mismatch");
        let bs_path = dir.join("blockstates.json");
        let col_path = dir.join("collision.json");
        std::fs::write(&bs_path, serde_json::to_vec(&serde_json::json!({ "count": 2, "states": ["m:a", "m:b"] })).unwrap()).unwrap();
        std::fs::write(&col_path, serde_json::to_vec(&serde_json::json!({ "count": 999, "empty": [(0.0, 900.0)] })).unwrap()).unwrap();
        let mut bs = BlockStates::load(&bs_path).unwrap();
        bs.load_collision(&col_path);
        assert_eq!(bs.collision_empty(0), None, "a mismatched dump must not be believed");
    }

    #[test]
    fn no_dump_means_unknown_and_never_solid() {
        // The pre-dump behaviour has to remain reachable: `None` tells the caller to use the name
        // heuristic. Answering `Some(false)` here would wall the bot in everywhere instead.
        let dir = temp_dir("absent");
        let bs_path = dir.join("blockstates.json");
        std::fs::write(&bs_path, serde_json::to_vec(&serde_json::json!({ "count": 1, "states": ["m:a"] })).unwrap()).unwrap();
        let mut bs = BlockStates::load(&bs_path).unwrap();
        bs.load_collision(&dir.join("does-not-exist.json"));
        assert_eq!(bs.collision_empty(0), None);
    }
}

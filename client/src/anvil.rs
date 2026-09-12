//! World download: everything the bot walks past, written out as standard Anvil `.mca` region
//! files so it is a real Minecraft backup rather than a bespoke dump.
//!
//! The target reader is McWebViewer (`src/core/region.ts` + `src/core/chunk.ts`), which reads
//! save files directly, so a downloaded world is immediately viewable. That also makes it the
//! acceptance test: if `npm run scan` over the output directory inventories the blocks, the file
//! is a real region file.
//!
//! Runs on its OWN THREAD behind a `sync_channel`, exactly like `publisher.rs`, because the
//! protocol loop must never block on disk I/O -- a stalled write there would throttle the chunk
//! stream and the SpacetimeDB mirror along with it. The channel is BOUNDED and the sender uses
//! `try_send`: if the saver ever falls behind, chunks are dropped (and counted) rather than
//! applying backpressure to the packet reader. Losing a chunk from a backup is recoverable by
//! revisiting it; stalling the mirror is not.

use anyhow::{Context, Result};
use flate2::write::ZlibEncoder;
use simdnbt::owned::{BaseNbt, NbtCompound, NbtList, NbtTag};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// Minecraft 1.21.1. Written verbatim into every chunk so a loader knows which data fixers (if
/// any) apply. Lying about this is how a "backup" quietly becomes unloadable.
pub const DATA_VERSION: i32 = 3955;

/// One Anvil sector.
const SECTOR: usize = 4096;
/// The two header sectors: 1024 location entries, then 1024 timestamps.
const HEADER_SECTORS: usize = 2;

/// A chunk whose payload needs more than 255 sectors has to go in an external `c.<x>.<z>.mcc`
/// file, because the sector count in the header is a single byte. We do not write those (see
/// `write_chunk`); a ~1 MiB *compressed* chunk does not occur in practice.
const MAX_SECTORS: usize = 255;

/// Vanilla's world border caps a chunk coordinate at 29_999_984 >> 4.
const MAX_CHUNK_COORD: i32 = 1_875_000;

/// How far from the bot a chunk may be and still belong to the world we are downloading.
///
/// The once-per-session chunk at `cx == cz == 1280064` is NOT a mis-parse -- see the note in
/// protocol.rs's 0x27 handler. It is a real, well-formed chunk of the Aeronautics mod's airship
/// level, which the server streams over the ordinary chunk packet without ever saying it belongs
/// to a different level. It is 20 million blocks out but still *inside* the world border, so a
/// border check alone would file someone's airship into `r.40002.40002.mca` next to the overworld.
///
/// Distance from the bot is what actually separates the two: a proxy only receives chunks the
/// server is streaming to its player, and the vanilla maximum view distance is 32. 512 is enormous
/// next to that on purpose -- this is here to keep another level out, not to second-guess the
/// server about its own.
const MAX_CHUNK_DISTANCE: i32 = 512;

/// Is this chunk coordinate somewhere the bot could actually be looking?
pub fn plausible(cx: i32, cz: i32, bot_cx: i32, bot_cz: i32) -> bool {
    cx.abs() <= MAX_CHUNK_COORD
        && cz.abs() <= MAX_CHUNK_COORD
        && cx.saturating_sub(bot_cx).saturating_abs() <= MAX_CHUNK_DISTANCE
        && cz.saturating_sub(bot_cz).saturating_abs() <= MAX_CHUNK_DISTANCE
}

/// How many region files to keep open. A view distance of 12 spans at most 4 regions, but the bot
/// moves; this is just a bound on file descriptors.
const MAX_OPEN_REGIONS: usize = 16;

pub type Tx = SyncSender<Msg>;

pub enum Msg {
    /// The id tables the saver needs to turn wire ids into names. Sent once configuration is
    /// finished, i.e. before any chunk can arrive.
    Tables {
        /// block state id -> (block name, property string such as `facing=north,half=bottom`)
        blocks: Arc<Vec<(String, String)>>,
        /// biome id -> name, in the order the server sent `minecraft:worldgen/biome`
        biomes: Arc<Vec<String>>,
    },
    Chunk(Box<ChunkSave>),
}

/// One section's paletted containers, straight out of `world::PalettedContainer::export()`:
/// `bits == 0` means "single value, in `palette[0]`"; an empty palette with `bits > 0` means the
/// container is direct (data holds global ids).
#[derive(Default)]
pub struct Packed {
    pub bits: u8,
    pub palette: Vec<u32>,
    pub data: Vec<u64>,
}

#[derive(Default)]
pub struct SectionSave {
    pub sy: i32,
    /// Absent for the light-only sections one below and one above the world, which the light
    /// packet carries and vanilla also writes.
    pub blocks: Option<Packed>,
    pub biomes: Option<Packed>,
    /// 2048 bytes each, or empty for "the server said nothing about this section".
    pub sky_light: Vec<u8>,
    pub block_light: Vec<u8>,
}

pub struct BlockEntitySave {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// registry name, e.g. `minecraft:chest` -- Anvil stores it in the block entity's own `id`
    /// field, whereas the wire carries it as a numeric type id outside the NBT.
    pub id: String,
    /// the block entity's NBT exactly as it arrived (nameless compound root, type byte included)
    pub raw: Vec<u8>,
}

pub struct ChunkSave {
    pub dimension: String,
    pub cx: i32,
    pub cz: i32,
    /// index of the lowest block section: Anvil's `yPos`
    pub min_section: i32,
    /// the packet's heightmaps compound, re-emitted as the root `Heightmaps`
    pub heightmaps: NbtCompound,
    pub sections: Vec<SectionSave>,
    pub block_entities: Vec<BlockEntitySave>,
}

// ───────────────────────────── palette maths ─────────────────────────────

/// Smallest n with 2^n >= v -- vanilla's `Mth.ceillog2`. `ceil_log2(1) == 0`.
fn ceil_log2(v: usize) -> u8 {
    let v = v.max(1);
    (usize::BITS - (v - 1).leading_zeros()) as u8
}

/// The bit width Anvil uses for a section's block-state indices.
///
/// This is NOT simply `max(4, ceillog2(len))`. Vanilla's `PalettedContainer.Strategy` promotes a
/// section with more than 256 distinct states to the GLOBAL palette, whose width comes from the
/// block-state registry size, not from the palette -- and its reader recomputes the width the same
/// way, so a file written with the naive rule would be read back wrong by vanilla. We follow
/// vanilla exactly. (McWebViewer's reader uses the naive rule and so would misread such a section,
/// but it would misread a genuine vanilla save identically; that is its bug to fix, not a reason
/// to write a non-standard file.)
fn block_bits(palette_len: usize, registry_len: usize) -> u8 {
    match ceil_log2(palette_len) {
        0 => 0,
        1..=4 => 4,
        b @ 5..=8 => b,
        _ => ceil_log2(registry_len),
    }
}

/// Same for the 4x4x4 biome container, whose linear palette only goes to 3 bits.
fn biome_bits(palette_len: usize, registry_len: usize) -> u8 {
    match ceil_log2(palette_len) {
        0 => 0,
        b @ 1..=3 => b,
        _ => ceil_log2(registry_len),
    }
}

/// Unpack a wire container into `n` global ids.
fn decode(p: &Packed, n: usize) -> Vec<u32> {
    if p.bits == 0 {
        return vec![p.palette.first().copied().unwrap_or(0); n];
    }
    let bits = p.bits as usize;
    let per_long = 64 / bits;
    let mask = (1u64 << bits) - 1;
    (0..n)
        .map(|i| {
            let word = p.data.get(i / per_long).copied().unwrap_or(0);
            let raw = ((word >> ((i % per_long) * bits)) & mask) as u32;
            if p.palette.is_empty() {
                raw // direct container: the value IS the global id
            } else {
                p.palette.get(raw as usize).copied().unwrap_or(0)
            }
        })
        .collect()
}

/// Distinct values in first-appearance order, plus each cell's index into them.
fn intern(values: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let mut palette: Vec<u32> = Vec::new();
    let mut seen: HashMap<u32, u32> = HashMap::new();
    let mut idx = Vec::with_capacity(values.len());
    for &v in values {
        let i = *seen.entry(v).or_insert_with(|| {
            palette.push(v);
            (palette.len() - 1) as u32
        });
        idx.push(i);
    }
    (palette, idx)
}

/// Bit-pack indices, `floor(64/bits)` per long with the top bits wasted -- entries have not
/// straddled a long boundary since 1.16, on the wire or on disk.
fn pack(values: &[u32], bits: u8) -> Vec<i64> {
    if bits == 0 {
        return Vec::new();
    }
    let bits = bits as usize;
    let per_long = 64 / bits;
    let mut out = vec![0i64; values.len().div_ceil(per_long)];
    for (i, &v) in values.iter().enumerate() {
        out[i / per_long] |= ((v as u64) << ((i % per_long) * bits)) as i64;
    }
    out
}

/// `minecraft:oak_stairs` + `facing=north,half=bottom` -> the `{Name, Properties}` compound Anvil
/// stores. Property values are enums, booleans or small ints, so splitting on `,` and `=` is
/// exact -- none of them can contain either character.
fn state_tag(name: &str, props: &str) -> NbtCompound {
    let mut c = NbtCompound::new();
    c.insert("Name", name);
    if !props.is_empty() {
        let mut p = NbtCompound::new();
        for kv in props.split(',') {
            if let Some((k, v)) = kv.split_once('=') {
                p.insert(k, v);
            }
        }
        if !p.is_empty() {
            c.insert("Properties", NbtTag::Compound(p));
        }
    }
    c
}

// ───────────────────────────── chunk NBT ─────────────────────────────

struct Tables {
    blocks: Arc<Vec<(String, String)>>,
    biomes: Arc<Vec<String>>,
}

impl Tables {
    fn block(&self, id: u32) -> NbtCompound {
        match self.blocks.get(id as usize) {
            Some((name, props)) => state_tag(name, props),
            // An unmapped id would silently become air, which is worse in a backup than an
            // obviously-wrong block: keep the id visible so a bad dump is diagnosable.
            None => state_tag(&format!("minecraft:unknown_state_{id}"), ""),
        }
    }
    fn biome(&self, id: u32) -> String {
        self.biomes.get(id as usize).cloned().unwrap_or_else(|| "minecraft:plains".to_string())
    }
}

fn section_tag(s: &SectionSave, t: &Tables, wide_palettes: &mut u32) -> NbtCompound {
    let mut c = NbtCompound::new();
    c.insert("Y", s.sy as i8);

    if let Some(p) = &s.blocks {
        let (palette, idx) = intern(&decode(p, 4096));
        if palette.len() > 256 {
            *wide_palettes += 1;
        }
        let bits = block_bits(palette.len(), t.blocks.len());
        let mut bs = NbtCompound::new();
        bs.insert("palette", NbtList::Compound(palette.iter().map(|&id| t.block(id)).collect()));
        // A single-state section carries no `data` at all -- that is how the reader knows every
        // cell is palette[0], and writing an all-zero array instead would be wrong at 0 bits.
        if bits > 0 {
            bs.insert("data", NbtTag::LongArray(pack(&idx, bits)));
        }
        c.insert("block_states", NbtTag::Compound(bs));
    }

    if let Some(p) = &s.biomes {
        let (palette, idx) = intern(&decode(p, 64));
        let bits = biome_bits(palette.len(), t.biomes.len().max(1));
        let mut b = NbtCompound::new();
        b.insert("palette", NbtList::String(palette.iter().map(|&id| t.biome(id).into()).collect()));
        if bits > 0 {
            b.insert("data", NbtTag::LongArray(pack(&idx, bits)));
        }
        c.insert("biomes", NbtTag::Compound(b));
    }

    // Absent means UNKNOWN, not dark -- so an empty array is written as nothing at all, and a
    // section the server said is genuinely dark arrives here as 2048 zero bytes and is written.
    if s.sky_light.len() == 2048 {
        c.insert("SkyLight", NbtTag::ByteArray(s.sky_light.clone()));
    }
    if s.block_light.len() == 2048 {
        c.insert("BlockLight", NbtTag::ByteArray(s.block_light.clone()));
    }
    c
}

fn block_entity_tag(be: &BlockEntitySave) -> NbtCompound {
    // The wire puts the position and type OUTSIDE the NBT (packed byte, short y, varint type id);
    // Anvil puts them inside it. Re-parsing the raw bytes here rather than cloning the parsed
    // compound on the protocol thread keeps that work off the packet loop.
    let mut c = match simdnbt::owned::read_unnamed(&mut std::io::Cursor::new(&be.raw[..])) {
        Ok(simdnbt::owned::Nbt::Some(base)) => base.into_inner(),
        _ => NbtCompound::new(),
    };
    c.insert("id", be.id.as_str());
    c.insert("x", be.x);
    c.insert("y", be.y);
    c.insert("z", be.z);
    c
}

/// The chunk root, in the 1.18+ layout (no `Level` wrapper).
fn chunk_tag(c: &ChunkSave, t: &Tables, wide_palettes: &mut u32) -> NbtCompound {
    let sections: Vec<NbtCompound> = c.sections.iter().map(|s| section_tag(s, t, wide_palettes)).collect();
    let block_entities: Vec<NbtCompound> = c.block_entities.iter().map(block_entity_tag).collect();

    // Only claim the light is authoritative if every block section actually carried some. With
    // this at 0 vanilla relights the chunk on load, which is the right answer for a partial
    // download; with it wrongly at 1 the chunk would stay dark forever.
    let light_on = c.sections.iter().all(|s| s.blocks.is_none() || s.sky_light.len() == 2048 || s.block_light.len() == 2048);

    let mut root = NbtCompound::new();
    root.insert("DataVersion", DATA_VERSION);
    root.insert("xPos", c.cx);
    root.insert("yPos", c.min_section);
    root.insert("zPos", c.cz);
    root.insert("Status", "minecraft:full");
    root.insert("LastUpdate", now_secs() as i64 * 20);
    root.insert("InhabitedTime", 0i64);
    root.insert("isLightOn", if light_on { 1i8 } else { 0i8 });
    root.insert("sections", NbtList::Compound(sections));
    root.insert("block_entities", NbtList::Compound(block_entities));
    root.insert("Heightmaps", NbtTag::Compound(c.heightmaps.clone()));
    // A proxy sees none of these; vanilla treats them all as optional, but writing the empty
    // containers keeps the chunk shaped like one vanilla produced.
    root.insert("block_ticks", NbtList::Empty);
    root.insert("fluid_ticks", NbtList::Empty);
    root.insert("PostProcessing", NbtList::Empty);
    root.insert("structures", NbtTag::Compound(NbtCompound::new()));
    root
}

fn now_secs() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0)
}

// ───────────────────────────── region file ─────────────────────────────

/// One `.mca`, updated IN PLACE. A proxy revisits regions constantly, so "write a fresh file"
/// would throw away every chunk of that region it is not currently looking at.
struct Region {
    file: File,
    /// 1024 packed header entries: `(sector offset << 8) | sector count`
    loc: Vec<u32>,
    /// 1024 epoch-second timestamps
    ts: Vec<u32>,
    /// sector allocation map; index 0 and 1 are the header
    used: Vec<bool>,
    last_touched: u64,
}

impl Region {
    fn open(path: &Path) -> Result<Region> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).with_context(|| format!("opening {}", path.display()))?;
        let len = file.metadata()?.len();
        let mut loc = vec![0u32; 1024];
        let mut ts = vec![0u32; 1024];
        if len >= (SECTOR * HEADER_SECTORS) as u64 {
            let mut hdr = vec![0u8; SECTOR * HEADER_SECTORS];
            file.seek(SeekFrom::Start(0))?;
            file.read_exact(&mut hdr)?;
            for i in 0..1024 {
                loc[i] = u32::from_be_bytes(hdr[i * 4..i * 4 + 4].try_into().unwrap());
                ts[i] = u32::from_be_bytes(hdr[SECTOR + i * 4..SECTOR + i * 4 + 4].try_into().unwrap());
            }
        } else {
            // A partial or empty file: start over with a zeroed header. `set_len` zero-fills.
            file.set_len((SECTOR * HEADER_SECTORS) as u64)?;
        }
        let sectors = (file.metadata()?.len() as usize).div_ceil(SECTOR).max(HEADER_SECTORS);
        let mut used = vec![false; sectors];
        used[0] = true;
        used[1] = true;
        for i in 0..1024 {
            let (off, cnt) = ((loc[i] >> 8) as usize, (loc[i] & 0xff) as usize);
            if off < HEADER_SECTORS || cnt == 0 {
                continue;
            }
            if off + cnt > used.len() {
                used.resize(off + cnt, false);
            }
            used[off..off + cnt].fill(true);
        }
        Ok(Region { file, loc, ts, used, last_touched: 0 })
    }

    /// First-fit over the free sectors, appending if nothing fits.
    fn alloc(&mut self, need: usize) -> usize {
        let mut run = 0usize;
        for s in HEADER_SECTORS..self.used.len() {
            if self.used[s] {
                run = 0;
                continue;
            }
            run += 1;
            if run == need {
                let start = s + 1 - need;
                self.used[start..start + need].fill(true);
                return start;
            }
        }
        let start = self.used.len();
        self.used.resize(start + need, true);
        start
    }

    fn write_chunk(&mut self, lx: usize, lz: usize, payload: &[u8]) -> Result<()> {
        let idx = lx + lz * 32;
        // 4-byte length + 1-byte compression scheme, then the payload, padded to a whole sector.
        let need = (payload.len() + 5).div_ceil(SECTOR);
        if need > MAX_SECTORS {
            anyhow::bail!("chunk payload {} bytes needs {need} sectors; external .mcc files are not written", payload.len());
        }
        let (old_off, old_cnt) = ((self.loc[idx] >> 8) as usize, (self.loc[idx] & 0xff) as usize);
        let off = if old_off >= HEADER_SECTORS && old_cnt >= need {
            // Fits where it already is; release the tail so a later chunk can use it.
            for s in old_off + need..old_off + old_cnt {
                self.used[s] = false;
            }
            old_off
        } else {
            if old_off >= HEADER_SECTORS && old_cnt > 0 {
                for s in old_off..old_off + old_cnt {
                    self.used[s] = false;
                }
            }
            self.alloc(need)
        };

        let mut buf = vec![0u8; need * SECTOR];
        buf[0..4].copy_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
        buf[4] = 2; // zlib
        buf[5..5 + payload.len()].copy_from_slice(payload);
        self.file.seek(SeekFrom::Start((off * SECTOR) as u64))?;
        self.file.write_all(&buf)?;

        self.loc[idx] = ((off as u32) << 8) | need as u32;
        self.ts[idx] = now_secs();
        // Header LAST, and on every chunk. The proxy is a long-running process that gets killed
        // rather than shut down, so the file has to be openable at any instant; a header that
        // pointed at a half-written payload would be exactly the corruption this avoids.
        self.flush_header()
    }

    fn flush_header(&mut self) -> Result<()> {
        let mut hdr = vec![0u8; SECTOR * HEADER_SECTORS];
        for i in 0..1024 {
            hdr[i * 4..i * 4 + 4].copy_from_slice(&self.loc[i].to_be_bytes());
            hdr[SECTOR + i * 4..SECTOR + i * 4 + 4].copy_from_slice(&self.ts[i].to_be_bytes());
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&hdr)?;
        self.file.flush()?;
        Ok(())
    }
}

// ───────────────────────────── the saver thread ─────────────────────────────

/// `minecraft:overworld` -> `minecraft_overworld`. Dimension ids are namespaced and a namespace
/// separator is not a directory the caller asked for.
fn dim_dir(dimension: &str) -> String {
    let s: String = dimension.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect();
    if s.is_empty() {
        "unknown".into()
    } else {
        s
    }
}

pub struct Saver {
    root: PathBuf,
    tables: Option<Tables>,
    regions: HashMap<(String, i32, i32), Region>,
    clock: u64,
    written: u64,
    skipped_no_tables: u64,
    wide_palettes: u32,
    bad_coords: u64,
    last_log: Instant,
}

impl Saver {
    pub fn new(root: PathBuf) -> Self {
        Saver { root, tables: None, regions: HashMap::new(), clock: 0, written: 0, skipped_no_tables: 0, wide_palettes: 0, bad_coords: 0, last_log: Instant::now() }
    }

    fn region(&mut self, dimension: &str, rx: i32, rz: i32) -> Result<&mut Region> {
        let dir = dim_dir(dimension);
        let key = (dir.clone(), rx, rz);
        self.clock += 1;
        if !self.regions.contains_key(&key) {
            if self.regions.len() >= MAX_OPEN_REGIONS {
                if let Some(victim) = self.regions.iter().min_by_key(|(_, r)| r.last_touched).map(|(k, _)| k.clone()) {
                    self.regions.remove(&victim);
                }
            }
            let path = self.root.join(&dir).join("region").join(format!("r.{rx}.{rz}.mca"));
            let region = Region::open(&path)?;
            info!("world download: region {}", path.display());
            self.regions.insert(key.clone(), region);
        }
        let r = self.regions.get_mut(&key).expect("just inserted");
        r.last_touched = self.clock;
        Ok(r)
    }

    pub fn save(&mut self, c: &ChunkSave) -> Result<()> {
        if c.cx.abs() > MAX_CHUNK_COORD || c.cz.abs() > MAX_CHUNK_COORD {
            self.bad_coords += 1;
            return Ok(());
        }
        let tables = match &self.tables {
            Some(t) => t,
            None => {
                self.skipped_no_tables += 1;
                return Ok(());
            }
        };
        let mut wide = self.wide_palettes;
        let root = chunk_tag(c, tables, &mut wide);
        self.wide_palettes = wide;

        let mut nbt = Vec::with_capacity(64 * 1024);
        // Anvil's root tag is NAMED (empty name), unlike network NBT since 1.20.2 which dropped
        // the name. Writing the network shape here produces a file every reader rejects.
        BaseNbt::new("", root).write(&mut nbt);

        let mut enc = ZlibEncoder::new(Vec::with_capacity(nbt.len() / 4), flate2::Compression::default());
        enc.write_all(&nbt)?;
        let payload = enc.finish()?;

        let (rx, rz) = (c.cx >> 5, c.cz >> 5);
        let (lx, lz) = ((c.cx & 31) as usize, (c.cz & 31) as usize);
        self.region(&c.dimension, rx, rz)?.write_chunk(lx, lz, &payload)?;
        self.written += 1;
        Ok(())
    }
}

/// Blocking loop; run on its own thread.
pub fn run(root: PathBuf, rx: Receiver<Msg>) {
    info!("world download enabled -> {}", root.display());
    let mut s = Saver::new(root);
    loop {
        let msg = match rx.recv() {
            Ok(m) => m,
            Err(_) => {
                info!("world download: channel closed; {} chunks written", s.written);
                return;
            }
        };
        match msg {
            Msg::Tables { blocks, biomes } => {
                info!("world download: {} block states, {} biomes", blocks.len(), biomes.len());
                s.tables = Some(Tables { blocks, biomes });
            }
            Msg::Chunk(c) => {
                if let Err(e) = s.save(&c) {
                    warn!("world download: chunk {},{} in {}: {e:#}", c.cx, c.cz, c.dimension);
                }
            }
        }
        if s.last_log.elapsed() > std::time::Duration::from_secs(30) {
            s.last_log = Instant::now();
            info!(
                "world download: {} chunks in {} regions (skipped {} before the id tables arrived, {} with absurd coordinates, {} sections needed a global palette)",
                s.written,
                s.regions.len(),
                s.skipped_no_tables,
                s.bad_coords,
                s.wide_palettes
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::ZlibDecoder;

    fn tables() -> Tables {
        Tables {
            blocks: Arc::new(vec![
                ("minecraft:air".into(), "".into()),
                ("minecraft:stone".into(), "".into()),
                ("minecraft:oak_stairs".into(), "facing=north,half=bottom".into()),
            ]),
            biomes: Arc::new(vec!["minecraft:plains".into(), "minecraft:desert".into()]),
        }
    }

    /// The Aeronautics airship-level chunk is inside the world border, so only the distance
    /// check keeps it out of the overworld's region files.
    #[test]
    fn the_known_bogus_chunk_is_rejected() {
        assert!(plausible(5, -3, 0, 0));
        assert!(plausible(-1000, 1000, -1200, 1200), "a chunk near the bot, far from the origin");
        assert!(!plausible(1280064, 1280064, 4, 2), "the once-per-session side-level chunk");
        assert!(!plausible(2_000_000, 0, 1_999_900, 0), "past the world border even though it is near the bot");
        assert!(!plausible(0, 0, i32::MIN, 0), "no overflow panic on an absurd bot position");
    }

    #[test]
    fn ceil_log2_matches_vanilla() {
        for (v, want) in [(0, 0), (1, 0), (2, 1), (3, 2), (4, 2), (16, 4), (17, 5), (256, 8), (257, 9)] {
            assert_eq!(ceil_log2(v), want, "ceil_log2({v})");
        }
    }

    #[test]
    fn bit_widths_follow_the_strategy() {
        assert_eq!(block_bits(1, 1000), 0); // single state: no data array at all
        assert_eq!(block_bits(2, 1000), 4); // linear palette is always 4 bits
        assert_eq!(block_bits(16, 1000), 4);
        assert_eq!(block_bits(17, 1000), 5);
        assert_eq!(block_bits(256, 1000), 8);
        assert_eq!(block_bits(257, 344003), ceil_log2(344003)); // global palette width
        assert_eq!(biome_bits(1, 100), 0);
        assert_eq!(biome_bits(2, 100), 1);
        assert_eq!(biome_bits(8, 100), 3);
        assert_eq!(biome_bits(9, 100), ceil_log2(100));
    }

    /// Pack then unpack with the reader's own rule; entries must not straddle a long.
    #[test]
    fn pack_round_trips_at_every_width() {
        for bits in 1u8..=12 {
            let n = 4096usize;
            let values: Vec<u32> = (0..n).map(|i| (i as u32) % (1 << bits)).collect();
            let longs = pack(&values, bits);
            let per_long = 64 / bits as usize;
            assert_eq!(longs.len(), n.div_ceil(per_long), "long count at {bits} bits");
            let p = Packed { bits, palette: vec![], data: longs.iter().map(|&l| l as u64).collect() };
            assert_eq!(decode(&p, n), values, "round trip at {bits} bits");
        }
    }

    #[test]
    fn single_value_container_expands() {
        // `export()` of a bits==0 container: the value lives in palette[0] and data is empty.
        let p = Packed { bits: 0, palette: vec![7], data: vec![] };
        assert_eq!(decode(&p, 64), vec![7u32; 64]);
    }

    #[test]
    fn direct_container_holds_global_ids() {
        // bits > 8 with an empty palette: the packed value IS the block-state id.
        let values: Vec<u32> = (0..4096).map(|i| 300000 + (i as u32 % 3)).collect();
        let p = Packed { bits: 19, palette: vec![], data: pack(&values, 19).iter().map(|&l| l as u64).collect() };
        assert_eq!(decode(&p, 4096), values);
    }

    #[test]
    fn properties_become_a_compound() {
        let c = state_tag("minecraft:oak_stairs", "facing=north,half=bottom");
        assert_eq!(c.string("Name").unwrap().to_str(), "minecraft:oak_stairs");
        let p = c.compound("Properties").unwrap();
        assert_eq!(p.string("facing").unwrap().to_str(), "north");
        assert_eq!(p.string("half").unwrap().to_str(), "bottom");
        assert!(state_tag("minecraft:stone", "").compound("Properties").is_none());
    }

    fn sample_chunk(cx: i32, cz: i32, fill: u32) -> ChunkSave {
        let mut blocks = vec![0u32; 4096];
        // half stone, half air, plus one stair so a Properties compound is exercised
        for b in blocks.iter_mut().take(2048) {
            *b = fill;
        }
        blocks[3000] = 2;
        let packed = Packed { bits: 4, palette: vec![0, fill, 2], data: pack(&blocks.iter().map(|&v| if v == 0 { 0 } else if v == 2 { 2 } else { 1 }).collect::<Vec<_>>(), 4).iter().map(|&l| l as u64).collect() };
        let mut hm = NbtCompound::new();
        hm.insert("MOTION_BLOCKING", NbtTag::LongArray(vec![0i64; 37]));
        ChunkSave {
            dimension: "minecraft:overworld".into(),
            cx,
            cz,
            min_section: -4,
            heightmaps: hm,
            sections: vec![SectionSave {
                sy: 0,
                blocks: Some(packed),
                biomes: Some(Packed { bits: 0, palette: vec![1], data: vec![] }),
                sky_light: vec![0xff; 2048],
                block_light: vec![0u8; 2048],
            }],
            block_entities: vec![],
        }
    }

    /// Write region files the way the saver does, then read them back with an independent
    /// implementation of the Anvil header/sector rules.
    fn read_back(path: &Path, lx: usize, lz: usize) -> Option<NbtCompound> {
        let data = std::fs::read(path).unwrap();
        assert!(data.len() >= SECTOR * 2, "header must be two full sectors");
        assert_eq!(data.len() % SECTOR, 0, "region files are sector-aligned");
        let idx = lx + lz * 32;
        let loc = u32::from_be_bytes(data[idx * 4..idx * 4 + 4].try_into().unwrap());
        let (off, cnt) = ((loc >> 8) as usize, (loc & 0xff) as usize);
        if off == 0 || cnt == 0 {
            return None;
        }
        let base = off * SECTOR;
        let len = u32::from_be_bytes(data[base..base + 4].try_into().unwrap()) as usize;
        assert_eq!(data[base + 4], 2, "compression scheme must be zlib");
        assert!(len + 4 <= cnt * SECTOR, "payload must fit its declared sectors");
        let mut out = Vec::new();
        ZlibDecoder::new(&data[base + 5..base + 4 + len]).read_to_end(&mut out).unwrap();
        match simdnbt::owned::read(&mut std::io::Cursor::new(&out[..])).unwrap() {
            simdnbt::owned::Nbt::Some(b) => Some(b.into_inner()),
            _ => None,
        }
    }

    #[test]
    fn writes_a_region_a_reader_can_open() {
        let dir = std::env::temp_dir().join(format!("mcst-anvil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Saver::new(dir.clone());
        s.tables = Some(tables());
        // Two chunks in one region and one in a neighbouring region, to prove the region split.
        s.save(&sample_chunk(3, 5, 1)).unwrap();
        s.save(&sample_chunk(4, 5, 1)).unwrap();
        s.save(&sample_chunk(40, 5, 1)).unwrap();
        // Past the world border: no region file, no matter what the packet said.
        s.save(&sample_chunk(2_000_000, 2_000_000, 1)).unwrap();
        assert_eq!(s.bad_coords, 1);
        drop(s);

        let region = dir.join("minecraft_overworld/region/r.0.0.mca");
        let root = read_back(&region, 3, 5).expect("chunk 3,5 present");
        assert_eq!(root.int("DataVersion"), Some(DATA_VERSION));
        assert_eq!(root.int("xPos"), Some(3));
        assert_eq!(root.int("zPos"), Some(5));
        assert_eq!(root.int("yPos"), Some(-4));
        assert_eq!(root.string("Status").unwrap().to_str(), "minecraft:full");
        assert!(root.compound("Heightmaps").unwrap().long_array("MOTION_BLOCKING").is_some());

        let sections = match root.list("sections").unwrap() {
            NbtList::Compound(v) => v.clone(),
            other => panic!("sections is {other:?}"),
        };
        assert_eq!(sections.len(), 1);
        let sec = &sections[0];
        assert_eq!(sec.byte("Y"), Some(0));
        assert_eq!(sec.byte_array("SkyLight").unwrap().len(), 2048);
        let bs = sec.compound("block_states").unwrap();
        let palette = match bs.list("palette").unwrap() {
            NbtList::Compound(v) => v.clone(),
            other => panic!("palette is {other:?}"),
        };
        assert_eq!(palette.len(), 3);
        assert_eq!(palette[0].string("Name").unwrap().to_str(), "minecraft:stone");
        let data = bs.long_array("data").unwrap();
        assert_eq!(data.len(), 4096 / (64 / 4));
        // Decode the way the reader does and check a cell we know.
        let p = Packed { bits: 4, palette: vec![], data: data.iter().map(|&l| l as u64).collect() };
        let idx = decode(&p, 4096);
        assert_eq!(palette[idx[0] as usize].string("Name").unwrap().to_str(), "minecraft:stone");
        assert_eq!(palette[idx[3000] as usize].string("Name").unwrap().to_str(), "minecraft:oak_stairs");
        assert_eq!(palette[idx[4095] as usize].string("Name").unwrap().to_str(), "minecraft:air");

        assert!(read_back(&region, 4, 5).is_some(), "second chunk in the same region");
        assert!(read_back(&region, 0, 0).is_none(), "untouched slots stay empty");
        assert!(dir.join("minecraft_overworld/region/r.1.0.mca").exists(), "chunk 40,5 goes to r.1.0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A proxy revisits regions constantly, so the second write of a chunk must land in the SAME
    /// file, must not corrupt its neighbours, and must relocate when it outgrows its sectors.
    #[test]
    fn updates_an_existing_region_in_place() {
        let dir = std::env::temp_dir().join(format!("mcst-anvil-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let region = dir.join("minecraft_overworld/region/r.0.0.mca");

        {
            let mut s = Saver::new(dir.clone());
            s.tables = Some(tables());
            s.save(&sample_chunk(0, 0, 1)).unwrap();
            s.save(&sample_chunk(1, 0, 1)).unwrap();
        } // files closed: the next Saver has to recover the allocation from the header alone

        let before = std::fs::metadata(&region).unwrap().len();
        assert!(read_back(&region, 1, 0).is_some());

        {
            // A much bigger chunk 0,0: 24 sections of light, which will not fit in place.
            let mut big = sample_chunk(0, 0, 1);
            // Incompressible light, or zlib squeezes 24 constant-filled sections back into one
            // sector and the chunk never has to move -- which would leave the relocation path,
            // the whole point of this test, untested.
            let mut seed = 0x2545_f491_4f6c_dd1du64;
            let mut noise = || {
                (0..2048)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed as u8
                    })
                    .collect::<Vec<u8>>()
            };
            for sy in 1..24 {
                big.sections.push(SectionSave { sy, blocks: None, biomes: None, sky_light: noise(), block_light: noise() });
            }
            let mut s = Saver::new(dir.clone());
            s.tables = Some(tables());
            s.save(&big).unwrap();
        }

        let grown = read_back(&region, 0, 0).expect("rewritten chunk");
        match grown.list("sections").unwrap() {
            NbtList::Compound(v) => assert_eq!(v.len(), 24),
            other => panic!("sections is {other:?}"),
        }
        // The neighbour written by the FIRST Saver must still be readable: that is the whole
        // point of reading the header back instead of starting a fresh file.
        assert!(read_back(&region, 1, 0).is_some(), "neighbour survived the in-place update");
        assert!(std::fs::metadata(&region).unwrap().len() > before, "file grew rather than clobbering");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn block_entities_gain_their_id_and_position() {
        // A nameless-root compound { Items: [] , Lock: "" } as it would arrive on the wire.
        let mut inner = NbtCompound::new();
        inner.insert("Lock", "key");
        let mut raw = Vec::new();
        BaseNbt::new("", inner).write_unnamed(&mut raw);

        let t = tables();
        let c = ChunkSave {
            dimension: "minecraft:overworld".into(),
            cx: 0,
            cz: 0,
            min_section: -4,
            heightmaps: NbtCompound::new(),
            sections: vec![],
            block_entities: vec![BlockEntitySave { x: 5, y: 70, z: -3, id: "minecraft:chest".into(), raw }],
        };
        let mut wide = 0;
        let root = chunk_tag(&c, &t, &mut wide);
        let bes = match root.list("block_entities").unwrap() {
            NbtList::Compound(v) => v.clone(),
            other => panic!("block_entities is {other:?}"),
        };
        assert_eq!(bes.len(), 1);
        assert_eq!(bes[0].string("id").unwrap().to_str(), "minecraft:chest");
        assert_eq!(bes[0].int("x"), Some(5));
        assert_eq!(bes[0].int("y"), Some(70));
        assert_eq!(bes[0].int("z"), Some(-3));
        assert_eq!(bes[0].string("Lock").unwrap().to_str(), "key", "the original NBT survives");
    }
}

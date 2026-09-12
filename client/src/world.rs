//! Client-side chunk store: paletted containers in the vanilla wire layout, so a section can be
//! re-encoded for SpacetimeDB without the module ever unpacking bits.

use crate::wire::Reader;
use anyhow::{bail, Result};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Block,
    Biome,
}

#[derive(Clone, Debug)]
pub struct PalettedContainer {
    pub kind: Kind,
    pub bits: u8,
    /// indirect palette (global ids). Empty when bits == 0 (see `single`) or direct.
    pub palette: Vec<u32>,
    /// single value when bits == 0
    pub single: u32,
    pub data: Vec<u64>,
    /// bit width the server uses for direct encoding (ceil(log2(registry size)))
    pub direct_bits: u8,
}

impl PalettedContainer {
    pub fn entries(kind: Kind) -> usize {
        match kind {
            Kind::Block => 4096,
            Kind::Biome => 64,
        }
    }
    fn max_indirect_bits(kind: Kind) -> u8 {
        match kind {
            Kind::Block => 8,
            Kind::Biome => 3,
        }
    }
    fn min_indirect_bits(kind: Kind) -> u8 {
        match kind {
            Kind::Block => 4,
            Kind::Biome => 1,
        }
    }

    pub fn read(r: &mut Reader, kind: Kind, direct_bits_hint: u8) -> Result<Self> {
        let bits = r.u8()?;
        let n = Self::entries(kind);
        let mut pc = PalettedContainer { kind, bits, palette: vec![], single: 0, data: vec![], direct_bits: direct_bits_hint };
        if bits == 0 {
            pc.single = r.varint()? as u32;
            let len = r.varint()?;
            // vanilla writes 0 longs here; tolerate anything
            for _ in 0..len {
                r.u64()?;
            }
            return Ok(pc);
        }
        if bits <= Self::max_indirect_bits(kind) {
            let len = r.varint()?;
            if len < 0 || len > 4096 {
                bail!("bad palette length {len}");
            }
            for _ in 0..len {
                pc.palette.push(r.varint()? as u32);
            }
        } else {
            // direct: the wire tells us the width the server uses
            pc.direct_bits = bits;
        }
        let len = r.varint()?;
        if len < 0 {
            bail!("bad data length");
        }
        let vpl = 64 / bits as usize;
        let expected = (n + vpl - 1) / vpl;
        if len as usize != expected {
            bail!("data length {len} != expected {expected} for bits {bits}");
        }
        pc.data.reserve(len as usize);
        for _ in 0..len {
            pc.data.push(r.u64()?);
        }
        Ok(pc)
    }

    fn is_direct(&self) -> bool {
        self.bits > Self::max_indirect_bits(self.kind)
    }

    fn raw_get(&self, i: usize) -> u32 {
        let bits = self.bits as usize;
        let vpl = 64 / bits;
        let word = self.data[i / vpl];
        let off = (i % vpl) * bits;
        ((word >> off) & ((1u64 << bits) - 1)) as u32
    }

    fn raw_set(&mut self, i: usize, v: u32) {
        let bits = self.bits as usize;
        let vpl = 64 / bits;
        let off = (i % vpl) * bits;
        let mask = ((1u64 << bits) - 1) << off;
        let w = &mut self.data[i / vpl];
        *w = (*w & !mask) | (((v as u64) << off) & mask);
    }

    pub fn get(&self, i: usize) -> u32 {
        if self.bits == 0 {
            return self.single;
        }
        let raw = self.raw_get(i);
        if self.is_direct() {
            raw
        } else {
            self.palette.get(raw as usize).copied().unwrap_or(0)
        }
    }

    /// Returns the previous value.
    pub fn set(&mut self, i: usize, v: u32) -> u32 {
        let old = self.get(i);
        if old == v {
            return old;
        }
        if self.bits == 0 {
            // grow to the smallest indirect palette
            let single = self.single;
            self.bits = Self::min_indirect_bits(self.kind);
            self.palette = vec![single];
            let vpl = 64 / self.bits as usize;
            let n = Self::entries(self.kind);
            self.data = vec![0u64; (n + vpl - 1) / vpl];
        }
        if self.is_direct() {
            self.raw_set(i, v);
            return old;
        }
        let idx = match self.palette.iter().position(|&p| p == v) {
            Some(idx) => idx,
            None => {
                if self.palette.len() >= (1usize << self.bits) {
                    self.grow();
                    if self.is_direct() {
                        self.raw_set(i, v);
                        return old;
                    }
                }
                self.palette.push(v);
                self.palette.len() - 1
            }
        };
        self.raw_set(i, idx as u32);
        old
    }

    /// Re-pack with one more bit (or switch to direct).
    fn grow(&mut self) {
        let n = Self::entries(self.kind);
        let values: Vec<u32> = (0..n).map(|i| self.get(i)).collect();
        let new_bits = self.bits + 1;
        if new_bits > Self::max_indirect_bits(self.kind) {
            self.bits = self.direct_bits.max(new_bits);
            self.palette.clear();
        } else {
            self.bits = new_bits;
        }
        let vpl = 64 / self.bits as usize;
        self.data = vec![0u64; (n + vpl - 1) / vpl];
        for (i, v) in values.into_iter().enumerate() {
            if self.is_direct() {
                self.raw_set(i, v);
            } else {
                let idx = self.palette.iter().position(|&p| p == v).unwrap_or_else(|| {
                    self.palette.push(v);
                    self.palette.len() - 1
                });
                self.raw_set(i, idx as u32);
            }
        }
    }

    /// (bits, palette-or-single, data) in the representation the module documents.
    pub fn export(&self) -> (u8, Vec<u32>, Vec<u64>) {
        if self.bits == 0 {
            (0, vec![self.single], vec![])
        } else {
            (self.bits, self.palette.clone(), self.data.clone())
        }
    }
}

#[derive(Clone, Debug)]
pub struct Section {
    pub non_air: u16,
    pub blocks: PalettedContainer,
    pub biomes: PalettedContainer,
}

impl Section {
    pub fn read(r: &mut Reader, block_direct_bits: u8, biome_direct_bits: u8) -> Result<Self> {
        let non_air = r.i16()? as u16;
        let blocks = PalettedContainer::read(r, Kind::Block, block_direct_bits)?;
        let biomes = PalettedContainer::read(r, Kind::Biome, biome_direct_bits)?;
        Ok(Section { non_air, blocks, biomes })
    }
    pub fn index(x: i32, y: i32, z: i32) -> usize {
        (((y & 15) << 8) | ((z & 15) << 4) | (x & 15)) as usize
    }
}

pub struct Chunk {
    pub cx: i32,
    pub cz: i32,
    pub min_section: i32,
    pub sections: Vec<Section>,
}

impl Chunk {
    pub fn parse(cx: i32, cz: i32, min_section: i32, section_count: usize, data: &[u8], block_direct_bits: u8, biome_direct_bits: u8) -> Result<Self> {
        let mut r = Reader::new(data);
        let mut sections = Vec::with_capacity(section_count);
        for _ in 0..section_count {
            sections.push(Section::read(&mut r, block_direct_bits, biome_direct_bits)?);
        }
        Ok(Chunk { cx, cz, min_section, sections })
    }

    pub fn section_mut(&mut self, sy: i32) -> Option<&mut Section> {
        let i = sy - self.min_section;
        if i < 0 {
            return None;
        }
        self.sections.get_mut(i as usize)
    }

    pub fn section(&self, sy: i32) -> Option<&Section> {
        let i = sy - self.min_section;
        if i < 0 {
            return None;
        }
        self.sections.get(i as usize)
    }
}

pub struct World {
    pub chunks: HashMap<(i32, i32), Chunk>,
}

impl World {
    pub fn new() -> Self {
        Self { chunks: HashMap::new() }
    }
    pub fn get_block(&self, x: i32, y: i32, z: i32) -> Option<u32> {
        let c = self.chunks.get(&(x >> 4, z >> 4))?;
        let s = c.section(y >> 4)?;
        Some(s.blocks.get(Section::index(x, y, z)))
    }
    /// Set a block; returns (old, section_y) if the chunk is loaded.
    pub fn set_block(&mut self, x: i32, y: i32, z: i32, state: u32, is_air: impl Fn(u32) -> bool) -> Option<(u32, i32)> {
        let c = self.chunks.get_mut(&(x >> 4, z >> 4))?;
        let sy = y >> 4;
        let s = c.section_mut(sy)?;
        let old = s.blocks.set(Section::index(x, y, z), state);
        if old != state {
            let was_air = is_air(old);
            let now_air = is_air(state);
            if was_air && !now_air {
                s.non_air = s.non_air.saturating_add(1);
            } else if !was_air && now_air {
                s.non_air = s.non_air.saturating_sub(1);
            }
        }
        Some((old, sy))
    }
}

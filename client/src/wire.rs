//! Minimal Minecraft wire-format reader/writer over raw packet frames, plus NBT helpers.
//!
//! We deliberately do NOT use azalea's typed packet structs for PLAY packets: on a modded
//! (NeoForge) server every registry-backed field (entity type, block entity type, block state,
//! dimension type, ...) carries ids beyond vanilla's tables and azalea's decoders reject them.
//! Reading the handful of packets we need by hand keeps the client registry-agnostic.

use anyhow::{anyhow, bail, Result};
use std::io::Cursor;

pub struct Reader<'a> {
    pub cur: Cursor<&'a [u8]>,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { cur: Cursor::new(data) }
    }
    pub fn remaining(&self) -> usize {
        self.cur.get_ref().len() - self.cur.position() as usize
    }
    pub fn rest(&self) -> &'a [u8] {
        &self.cur.get_ref()[self.cur.position() as usize..]
    }
    pub fn skip(&mut self, n: usize) -> Result<()> {
        if self.remaining() < n {
            bail!("skip past end");
        }
        self.cur.set_position(self.cur.position() + n as u64);
        Ok(())
    }
    pub fn u8(&mut self) -> Result<u8> {
        let b = self.bytes(1)?;
        Ok(b[0])
    }
    pub fn i8(&mut self) -> Result<i8> {
        Ok(self.u8()? as i8)
    }
    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }
    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.bytes(2)?.try_into()?))
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.bytes(2)?.try_into()?))
    }
    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.bytes(4)?.try_into()?))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.bytes(8)?.try_into()?))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.bytes(8)?.try_into()?))
    }
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_be_bytes(self.bytes(4)?.try_into()?))
    }
    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_be_bytes(self.bytes(8)?.try_into()?))
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            bail!("read past end (want {n}, have {})", self.remaining());
        }
        let p = self.cur.position() as usize;
        self.cur.set_position((p + n) as u64);
        Ok(&self.cur.get_ref()[p..p + n])
    }
    pub fn varint(&mut self) -> Result<i32> {
        let mut v: u32 = 0;
        for i in 0..5 {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u32) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v as i32);
            }
        }
        bail!("varint too long")
    }
    pub fn varlong(&mut self) -> Result<i64> {
        let mut v: u64 = 0;
        for i in 0..10 {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v as i64);
            }
        }
        bail!("varlong too long")
    }
    pub fn string(&mut self) -> Result<String> {
        let n = self.varint()?;
        if n < 0 {
            bail!("negative string length");
        }
        Ok(String::from_utf8_lossy(self.bytes(n as usize)?).into_owned())
    }
    pub fn byte_array(&mut self) -> Result<&'a [u8]> {
        let n = self.varint()?;
        if n < 0 {
            bail!("negative array length");
        }
        self.bytes(n as usize)
    }
    pub fn uuid(&mut self) -> Result<String> {
        let hi = self.u64()?;
        let lo = self.u64()?;
        Ok(uuid::Uuid::from_u64_pair(hi, lo).to_string())
    }
    /// BlockPos.asLong() -> (x, y, z)
    pub fn block_pos(&mut self) -> Result<(i32, i32, i32)> {
        Ok(unpack_block_pos(self.i64()?))
    }
    /// Network NBT (nameless root). Returns the owned compound; also the raw bytes consumed.
    /// The root MUST be a compound (TAG_Compound) — used where the protocol guarantees that
    /// (block entities, heightmaps, chat components). For registry data, whose modded entries
    /// can have any root type, use [`Reader::nbt_any`].
    pub fn nbt(&mut self) -> Result<(simdnbt::owned::Nbt, &'a [u8])> {
        let start = self.cur.position() as usize;
        let nbt = simdnbt::owned::read_unnamed(&mut self.cur).map_err(|e| anyhow!("nbt: {e}"))?;
        let end = self.cur.position() as usize;
        Ok((nbt, &self.cur.get_ref()[start..end]))
    }

    /// One network NBT tag with a nameless root of ANY type, as an owned tag.
    ///
    /// [`Reader::nbt`] demands a COMPOUND root, and that is wrong for anything carrying a chat
    /// component: `ComponentSerialization.STREAM_CODEC` writes a component through `NbtOps`, and a
    /// component that is just text serialises as a bare **TAG_String**, not a compound. So
    /// `{"text":"Digger"}` arrives as a compound but a plain `"Digger"` arrives as a string, and a
    /// compound-only reader fails on exactly the simpler of the two. Measured: an item stack with
    /// `custom_name` set was reported as an undecodable component until this existed.
    pub fn nbt_tag(&mut self) -> Result<Option<simdnbt::owned::NbtTag>> {
        simdnbt::owned::read_optional_tag(&mut self.cur).map_err(|_| anyhow!("nbt tag: malformed"))
    }

    /// Network NBT with a nameless root of ANY tag type (1.20.2+). simdnbt only reads compound
    /// roots, but modded dynamic-registry entries (e.g. spell_engine's `minecraft:spell`) use a
    /// nameless TAG_Byte_Array-of-JSON root. Consumes exactly the tag and returns it as JSON.
    pub fn nbt_any(&mut self) -> Result<serde_json::Value> {
        let ty = self.u8()?;
        if ty == 0 {
            return Ok(serde_json::Value::Null);
        }
        self.nbt_payload(ty)
    }

    fn nbt_payload(&mut self, ty: u8) -> Result<serde_json::Value> {
        use serde_json::{json, Value};
        Ok(match ty {
            1 => json!(self.i8()?),
            2 => json!(self.i16()?),
            3 => json!(self.i32()?),
            4 => json!(self.i64()?),
            5 => json!(self.f32()?),
            6 => json!(self.f64()?),
            7 => {
                let n = self.i32()?;
                let b = self.bytes(n.max(0) as usize)?;
                Value::Array(b.iter().map(|x| json!(*x as i8)).collect())
            }
            8 => {
                let n = self.u16()? as usize;
                Value::String(String::from_utf8_lossy(self.bytes(n)?).into_owned())
            }
            9 => {
                let et = self.u8()?;
                let n = self.i32()?.max(0);
                let mut v = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    v.push(if et == 0 { Value::Null } else { self.nbt_payload(et)? });
                }
                Value::Array(v)
            }
            10 => {
                let mut m = serde_json::Map::new();
                loop {
                    let t = self.u8()?;
                    if t == 0 {
                        break;
                    }
                    let nlen = self.u16()? as usize;
                    let name = String::from_utf8_lossy(self.bytes(nlen)?).into_owned();
                    m.insert(name, self.nbt_payload(t)?);
                }
                Value::Object(m)
            }
            11 => {
                let n = self.i32()?.max(0);
                let mut v = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    v.push(json!(self.i32()?));
                }
                Value::Array(v)
            }
            12 => {
                let n = self.i32()?.max(0);
                let mut v = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    v.push(json!(self.i64()?));
                }
                Value::Array(v)
            }
            other => bail!("unknown NBT tag type {other}"),
        })
    }
}

/// The inverse of [`unpack_block_pos`]: `BlockPos.asLong()`, x 26 bits, z 26 bits, y 12 bits.
pub fn pack_block_pos(x: i32, y: i32, z: i32) -> i64 {
    (((x as i64) & 0x3FF_FFFF) << 38) | (((z as i64) & 0x3FF_FFFF) << 12) | ((y as i64) & 0xFFF)
}

pub fn unpack_block_pos(v: i64) -> (i32, i32, i32) {
    let x = (v >> 38) as i32;
    let y = ((v << 52) >> 52) as i32;
    let z = ((v << 26) >> 38) as i32;
    (x, y, z)
}

/// ChunkSectionPos.asLong(): x 22 bits, z 22 bits, y 20 bits
pub fn unpack_section_pos(v: i64) -> (i32, i32, i32) {
    let x = (v >> 42) as i32;
    let y = ((v << 44) >> 44) as i32;
    let z = ((v << 22) >> 42) as i32;
    (x, y, z)
}

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_id(id: i32) -> Self {
        let mut w = Self::default();
        w.varint(id);
        w
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }
    pub fn i16(&mut self, v: i16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn f64(&mut self, v: f64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn varint(&mut self, v: i32) -> &mut Self {
        let mut v = v as u32;
        loop {
            if v & !0x7f == 0 {
                self.buf.push(v as u8);
                return self;
            }
            self.buf.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
    }
    pub fn string(&mut self, s: &str) -> &mut Self {
        self.varint(s.len() as i32);
        self.buf.extend_from_slice(s.as_bytes());
        self
    }
    pub fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }
    pub fn byte_array(&mut self, b: &[u8]) -> &mut Self {
        self.varint(b.len() as i32);
        self.raw(b)
    }
    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

// ───────────────────────────── NBT helpers ─────────────────────────────

use simdnbt::owned::{NbtCompound, NbtList, NbtTag};

pub fn nbt_tag_to_json(t: &NbtTag) -> serde_json::Value {
    use serde_json::{json, Value};
    match t {
        NbtTag::Byte(v) => json!(v),
        NbtTag::Short(v) => json!(v),
        NbtTag::Int(v) => json!(v),
        NbtTag::Long(v) => json!(v),
        NbtTag::Float(v) => json!(v),
        NbtTag::Double(v) => json!(v),
        NbtTag::ByteArray(v) => Value::Array(v.iter().map(|b| json!(*b as i8)).collect()),
        NbtTag::String(s) => Value::String(s.to_str().into_owned()),
        NbtTag::List(l) => nbt_list_to_json(l),
        NbtTag::Compound(c) => nbt_compound_to_json(c),
        NbtTag::IntArray(v) => json!(v),
        NbtTag::LongArray(v) => json!(v),
    }
}

pub fn nbt_list_to_json(l: &NbtList) -> serde_json::Value {
    use serde_json::{json, Value};
    match l {
        NbtList::Empty => Value::Array(vec![]),
        NbtList::Byte(v) => json!(v),
        NbtList::Short(v) => json!(v),
        NbtList::Int(v) => json!(v),
        NbtList::Long(v) => json!(v),
        NbtList::Float(v) => json!(v),
        NbtList::Double(v) => json!(v),
        NbtList::ByteArray(v) => json!(v),
        NbtList::String(v) => Value::Array(v.iter().map(|s| Value::String(s.to_str().into_owned())).collect()),
        NbtList::List(v) => Value::Array(v.iter().map(nbt_list_to_json).collect()),
        NbtList::Compound(v) => Value::Array(v.iter().map(nbt_compound_to_json).collect()),
        NbtList::IntArray(v) => json!(v),
        NbtList::LongArray(v) => json!(v),
    }
}

pub fn nbt_compound_to_json(c: &NbtCompound) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (k, v) in c.iter() {
        m.insert(k.to_str().into_owned(), nbt_tag_to_json(v));
    }
    serde_json::Value::Object(m)
}

/// Flatten a network text Component (NBT form) into a readable string, keeping translation
/// keys and their arguments: `key(arg1, arg2)`. Used to read NeoForge's negotiation failures.
pub fn component_to_text(data: &[u8]) -> String {
    let mut cur = Cursor::new(data);
    match simdnbt::owned::read_optional_tag(&mut cur) {
        Ok(Some(tag)) => component_tag_to_text(&tag),
        Ok(None) => String::new(),
        Err(_) => "<unparseable component>".to_string(),
    }
}

pub fn component_tag_to_text(tag: &NbtTag) -> String {
    match tag {
        NbtTag::String(s) => s.to_str().into_owned(),
        NbtTag::Compound(c) => {
            let mut out = String::new();
            if let Some(t) = c.string("text") {
                out.push_str(&t.to_str());
            }
            if let Some(k) = c.string("translate") {
                out.push_str(&k.to_str());
                if let Some(NbtList::Compound(args)) = c.list("with") {
                    let parts: Vec<String> = args.iter().map(|a| component_tag_to_text(&NbtTag::Compound(a.clone()))).collect();
                    out.push_str(&format!("({})", parts.join(", ")));
                } else if let Some(NbtList::String(args)) = c.list("with") {
                    let parts: Vec<String> = args.iter().map(|a| a.to_str().into_owned()).collect();
                    out.push_str(&format!("({})", parts.join(", ")));
                } else if let Some(NbtList::List(_)) = c.list("with") {
                    out.push_str("(...)");
                }
            }
            if let Some(NbtList::Compound(extra)) = c.list("extra") {
                for e in extra {
                    out.push_str(&component_tag_to_text(&NbtTag::Compound(e.clone())));
                }
            }
            out
        }
        other => format!("{other:?}"),
    }
}

/// Extract the string arguments of a translatable component, recursively (nested components'
/// own args are appended in order). Returns (translation keys seen, string args).
pub fn component_translate_args(tag: &NbtTag, keys: &mut Vec<String>, args: &mut Vec<String>) {
    if let NbtTag::Compound(c) = tag {
        if let Some(k) = c.string("translate") {
            keys.push(k.to_str().into_owned());
        }
        match c.list("with") {
            Some(NbtList::Compound(list)) => {
                for a in list {
                    // a plain {"text": "..."} argument is a string arg
                    if let Some(t) = a.string("text") {
                        if a.get("translate").is_none() {
                            args.push(t.to_str().into_owned());
                            continue;
                        }
                    }
                    component_translate_args(&NbtTag::Compound(a.clone()), keys, args);
                }
            }
            Some(NbtList::String(list)) => {
                for a in list {
                    args.push(a.to_str().into_owned());
                }
            }
            _ => {}
        }
    } else if let NbtTag::String(s) = tag {
        args.push(s.to_str().into_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bot now SENDS block positions (use_item_on), so packing has to be exactly the inverse of
    /// unpacking -- a sign-extension slip would aim it at a block millions away and the server would
    /// just ignore the packet, which looks like nothing happening.
    #[test]
    fn block_pos_round_trips_including_negatives() {
        for p in [(0, 0, 0), (64, 66, 32), (-1200, 71, 340), (-1, -1, -1), (33_554_431, 2047, -33_554_432), (1280064, 63, 1280064)] {
            assert_eq!(unpack_block_pos(pack_block_pos(p.0, p.1, p.2)), p, "{p:?}");
        }
    }
}

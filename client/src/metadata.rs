//! SynchedEntityData (the `set_entity_data` packet) walked as DATA, not as meaning.
//!
//! The mirror had position, type and custom name for every entity and nothing that says a sheep is
//! black, a wolf is a woods variant or a villager is a farmer, so every consumer had to render the
//! default skin. That information is all in entity metadata, which is a flat list of
//! `(index, serializer type, value)` triples.
//!
//! **This file deliberately does not know what a sheep is.** "Index 17 is sheep colour" is true of
//! exactly one Minecraft version and untrue of any mod that adds a field to `Sheep`; encoding it in
//! the proxy would make the proxy wrong every time a pack changes. What is stable is the
//! *serializer table* -- the wire needs it to be, because the client has to be able to skip a field
//! it does not understand -- so the walk is driven by that alone and the values come out as
//! numbers, booleans and strings under their own index. The rules that turn index 17 into "black"
//! live in the viewer, next to the rest of the per-mob rendering rules.
//!
//! Serializer ids are the 1.21 table. They are read only for skipping and for deciding whether a
//! value is small enough to carry; a modded serializer at an unknown id ends the walk (see
//! `walk`), which is the same thing the vanilla client would do.

use crate::itemstack::{self, Env};
use crate::wire::Reader;
use serde_json::Value;
use std::collections::BTreeMap;

/// The index of `CUSTOM_NAME` on `Entity` -- the ONE index this file knows, because the mirror has
/// carried `custom_name` as its own column since before this file existed and the walk has to hand
/// it back to that column rather than bury it in the blob.
const CUSTOM_NAME_INDEX: u8 = 2;
const OPTIONAL_COMPONENT: i32 = 6;

/// A string field is carried, but not without limit: this is a column on a row that is rewritten
/// every time the entity moves.
const MAX_STRING: usize = 64;

/// What one walked field turned into.
enum Field {
    /// carried: a scalar small enough to sit on a hot row
    Value(Value),
    /// understood and skipped: structured or bulk (NBT, chat components, positions)
    Skipped,
    /// an item stack that decoded only partially, with the name of the component that stopped it.
    /// The value is kept (everything before the unknown component is correct) but the walk must end.
    StackStopped(Value, String),
}

pub struct Walked {
    /// index -> value, for the fields worth carrying. A `BTreeMap` so the JSON is emitted in a
    /// stable order -- without that, two identical metadata sets could serialise differently and
    /// the "did anything actually change?" check below would fire on every packet.
    pub fields: BTreeMap<u8, Value>,
    /// `Some(name)` if the packet carried index 2; `Some(None)` means the name was cleared.
    pub custom_name: Option<Option<String>>,
    /// true if the walk hit a serializer it could not skip and stopped early. The fields before it
    /// are still valid -- metadata is a stream of self-delimiting items, so everything read before
    /// the unknown one was read correctly.
    pub truncated: bool,
    /// `Some(name)` if an item stack in this payload carried a data component with no codec. The
    /// walk stopped there (see itemstack.rs: the position after such a component is unknowable), and
    /// the name is the one thing worth reporting -- it names the codec that needs writing.
    pub unknown_component: Option<String>,
}

/// Read one metadata value. `Ok(None)` means "this serializer cannot be skipped generically";
/// the caller must stop, because from here on the buffer position is unknown.
fn read_value(r: &mut Reader, ty: i32, env: Env) -> Option<Field> {
    let f = |v: Value| Some(Field::Value(v));
    match ty {
        0 => f(Value::from(r.i8().ok()?)),                                    // Byte
        1 => f(Value::from(r.varint().ok()?)),                                // VarInt
        2 => f(Value::from(r.varlong().ok()?)),                               // VarLong
        3 => f(Value::from(r.f32().ok()?)),                                   // Float
        4 => {
            // String. Truncated by CHARACTERS, not bytes, so the result is always valid UTF-8.
            let s = r.string().ok()?;
            f(Value::from(if s.chars().count() > MAX_STRING { s.chars().take(MAX_STRING).collect::<String>() } else { s }))
        }
        5 => {
            r.nbt().ok()?; // Component: a whole chat tree. Bulk.
            Some(Field::Skipped)
        }
        6 => {
            // Optional Component. The caller pulls index 2 out of this before we get here.
            if r.bool().ok()? {
                r.nbt().ok()?;
            }
            Some(Field::Skipped)
        }
        // ItemStack. NOT generic -- a data-component patch is decoded by per-component codecs and
        // cannot even be skipped, so this is the one serializer whose handling lives in its own
        // module (itemstack.rs). It CAN still stop the walk, when a stack carries a component whose
        // codec is not implemented; `Field::Stack` carries that outcome up so the name gets logged
        // rather than swallowed as a generic truncation.
        7 => {
            let d = itemstack::decode(r, env);
            let value = match &d.stack {
                Some(s) => s.to_json(),
                // An empty stack is a real value -- an item frame with nothing in it -- so it is
                // carried as null rather than left absent, which would read as "unknown".
                None => Value::Null,
            };
            match d.stopped_at {
                Some(name) => Some(Field::StackStopped(value, name)),
                None => f(value),
            }
        }
        8 => f(Value::from(r.bool().ok()?)),  // Boolean
        9 => {
            // Rotations (3 floats). Armour-stand limb poses are appearance, but they are 3 floats
            // of churn on a hot row and no mob variant uses them; skipped, not unsupported.
            r.f32().ok()?;
            r.f32().ok()?;
            r.f32().ok()?;
            Some(Field::Skipped)
        }
        10 => {
            r.i64().ok()?; // BlockPos
            Some(Field::Skipped)
        }
        11 => {
            if r.bool().ok()? {
                r.i64().ok()?; // Optional BlockPos
            }
            Some(Field::Skipped)
        }
        12 => f(Value::from(r.varint().ok()?)), // Direction
        13 => {
            if r.bool().ok()? {
                r.u64().ok()?;
                r.u64().ok()?; // Optional UUID
            }
            Some(Field::Skipped)
        }
        14 => f(Value::from(r.varint().ok()?)), // BlockState -- falling blocks, minecart displays
        15 => f(Value::from(r.varint().ok()?)), // Optional BlockState (0 = none)
        16 => {
            r.nbt().ok()?; // CompoundTag. Bulk.
            Some(Field::Skipped)
        }
        // Particle / Particles: a varint particle id followed by data only that particle's codec
        // knows the length of. Unskippable, same as ItemStack.
        17 | 18 => None,
        19 => {
            // VillagerData: type, profession, level -- three varints, and the profession is the
            // whole reason a villager looks like a farmer rather than a nitwit.
            let t = r.varint().ok()?;
            let p = r.varint().ok()?;
            let l = r.varint().ok()?;
            f(Value::from(vec![t, p, l]))
        }
        20 => f(Value::from(r.varint().ok()?)), // Optional VarInt (0 = absent)
        21 => f(Value::from(r.varint().ok()?)), // Pose
        22 | 23 | 24 => f(Value::from(r.varint().ok()?)), // cat / wolf / frog variant
        25 => {
            if r.bool().ok()? {
                r.string().ok()?;
                r.i64().ok()?; // Optional GlobalPos
            }
            Some(Field::Skipped)
        }
        26 | 27 | 28 => f(Value::from(r.varint().ok()?)), // painting variant / sniffer / armadillo
        29 => {
            r.f32().ok()?;
            r.f32().ok()?;
            r.f32().ok()?; // Vector3
            Some(Field::Skipped)
        }
        30 => {
            r.f32().ok()?;
            r.f32().ok()?;
            r.f32().ok()?;
            r.f32().ok()?; // Quaternion
            Some(Field::Skipped)
        }
        _ => None, // a modded serializer: unknown length, so the walk has to stop
    }
}

/// Walk the metadata items of a `set_entity_data` payload. The reader must be positioned just
/// after the entity id.
pub fn walk(r: &mut Reader, env: Env) -> Walked {
    let mut out = Walked { fields: BTreeMap::new(), custom_name: None, truncated: false, unknown_component: None };
    loop {
        let idx = match r.u8() {
            Ok(i) => i,
            Err(_) => {
                out.truncated = true;
                return out;
            }
        };
        if idx == 0xff {
            return out; // end of list
        }
        let ty = match r.varint() {
            Ok(t) => t,
            Err(_) => {
                out.truncated = true;
                return out;
            }
        };
        if idx == CUSTOM_NAME_INDEX && ty == OPTIONAL_COMPONENT {
            let has = match r.bool() {
                Ok(b) => b,
                Err(_) => {
                    out.truncated = true;
                    return out;
                }
            };
            if !has {
                out.custom_name = Some(None);
                continue;
            }
            // ANY root type, not just a compound: a name that is plain text serialises as a bare
            // TAG_String and a compound-only read fails on it -- which used to end the whole walk,
            // losing every field after the name on exactly the simplest entities.
            match r.nbt_tag() {
                Ok(Some(tag)) => out.custom_name = Some(Some(crate::wire::component_tag_to_text(&tag))),
                Ok(None) => out.custom_name = Some(Some(String::new())),
                Err(_) => {
                    out.truncated = true;
                    return out;
                }
            }
            continue;
        }
        match read_value(r, ty, env) {
            Some(Field::Value(v)) => {
                out.fields.insert(idx, v);
            }
            Some(Field::Skipped) => {}
            Some(Field::StackStopped(v, name)) => {
                out.fields.insert(idx, v);
                out.unknown_component = Some(name);
                out.truncated = true;
                return out;
            }
            None => {
                out.truncated = true;
                return out;
            }
        }
    }
}

/// Compact JSON for a merged field map: `{"0":0,"17":3}`.
pub fn to_json(fields: &BTreeMap<u8, Value>) -> String {
    let mut s = String::with_capacity(fields.len() * 10 + 2);
    s.push('{');
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        s.push_str(&k.to_string());
        s.push_str("\":");
        s.push_str(&v.to_string());
    }
    s.push('}');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Writer;

    /// Build a metadata payload the way the server does, so the tests exercise the real walk
    /// rather than a hand-written byte string nobody can check.
    struct Meta(Vec<u8>);
    impl Meta {
        fn new() -> Self {
            Meta(Vec::new())
        }
        fn item(mut self, idx: u8, ty: i32, body: &[u8]) -> Self {
            self.0.push(idx);
            let mut w = Writer::new();
            w.varint(ty);
            self.0.extend_from_slice(&w.into_inner());
            self.0.extend_from_slice(body);
            self
        }
        fn end(mut self) -> Vec<u8> {
            self.0.push(0xff);
            self.0
        }
    }
    fn varint(v: i32) -> Vec<u8> {
        let mut w = Writer::new();
        w.varint(v);
        w.into_inner()
    }

    #[test]
    fn walks_a_sheep_and_keeps_the_scalars() {
        // Entity flags (byte), air (varint), silent (bool), pose (varint), health (float), and a
        // species byte at 17 -- the shape a sheep actually arrives in.
        let buf = Meta::new()
            .item(0, 0, &[0x00])
            .item(1, 1, &varint(300))
            .item(4, 8, &[0x00])
            .item(6, 21, &varint(0))
            .item(9, 3, &8.0f32.to_be_bytes())
            .item(17, 0, &[0x0b])
            .end();
        let w = walk(&mut Reader::new(&buf), Env::default());
        assert!(!w.truncated);
        assert_eq!(w.fields.len(), 6);
        assert_eq!(w.fields[&17], Value::from(11)); // black sheep, uninterpreted
        assert_eq!(w.fields[&1], Value::from(300));
        assert_eq!(w.fields[&4], Value::from(false));
        assert_eq!(w.fields[&9], Value::from(8.0f32));
        assert_eq!(to_json(&w.fields), r#"{"0":0,"1":300,"4":false,"6":0,"9":8.0,"17":11}"#);
    }

    #[test]
    fn villager_data_survives_as_three_numbers() {
        let mut body = varint(0); // type
        body.extend(varint(5)); // profession: farmer, in whatever this server's registry says
        body.extend(varint(2)); // level
        let buf = Meta::new().item(18, 19, &body).item(0, 0, &[0x00]).end();
        let w = walk(&mut Reader::new(&buf), Env::default());
        assert!(!w.truncated, "villager data must be skippable AND carryable");
        assert_eq!(w.fields[&18], Value::from(vec![0, 5, 2]));
    }

    /// The custom name keeps its own column; it must come out of the walk, not the blob.
    #[test]
    fn custom_name_is_lifted_out_of_the_blob() {
        let mut name = Vec::new();
        let mut c = simdnbt::owned::NbtCompound::new();
        c.insert("text", "Bessie");
        let mut raw = Vec::new();
        simdnbt::owned::BaseNbt::new("", c).write_unnamed(&mut raw);
        name.push(0x01); // present
        name.extend_from_slice(&raw);
        let buf = Meta::new().item(0, 0, &[0x20]).item(2, 6, &name).item(17, 0, &[0x04]).end();
        let w = walk(&mut Reader::new(&buf), Env::default());
        assert_eq!(w.custom_name, Some(Some("Bessie".to_string())));
        assert!(!w.fields.contains_key(&2), "the name must not also be duplicated into the blob");
        assert_eq!(w.fields[&17], Value::from(4), "fields after the name are still read");
    }

    /// Bulk fields are walked past, not carried: the row is rewritten on every move.
    #[test]
    fn bulk_fields_are_skipped_without_stopping_the_walk() {
        let mut nbt = Vec::new();
        let mut c = simdnbt::owned::NbtCompound::new();
        c.insert("Lots", "of data");
        simdnbt::owned::BaseNbt::new("", c).write_unnamed(&mut nbt);
        let buf = Meta::new()
            .item(5, 16, &nbt) // CompoundTag
            .item(6, 10, &0i64.to_be_bytes()) // BlockPos
            .item(7, 9, &[0u8; 12]) // Rotations
            .item(8, 13, &[0x00]) // Optional UUID, absent
            .item(19, 1, &varint(2)) // the one we want
            .end();
        let w = walk(&mut Reader::new(&buf), Env::default());
        assert!(!w.truncated);
        assert_eq!(w.fields.keys().copied().collect::<Vec<_>>(), vec![19]);
    }

    /// An item stack used to end the walk unconditionally; now it is decoded, and the walk carries
    /// straight on to the fields after it. This is the hole in `entity.appearance` closing: index 8
    /// on a dropped item is the stack itself.
    #[test]
    fn an_item_stack_no_longer_stops_the_walk() {
        let items: std::collections::HashMap<u32, String> = [(9u32, "minecraft:cobblestone".to_string())].into_iter().collect();
        let env = Env { items: Some(&items), components: None, enchantments: None , ..Env::default() };
        let buf = Meta::new()
            .item(0, 0, &[0x00])
            // ItemStack: count 7, item 9, no added components, no removed
            .item(8, 7, &[0x07, 0x09, 0x00, 0x00])
            .item(9, 1, &varint(7))
            .end();
        let w = walk(&mut Reader::new(&buf), env);
        assert!(!w.truncated, "the walk must reach the end of the list");
        assert_eq!(w.unknown_component, None);
        assert_eq!(w.fields.keys().copied().collect::<Vec<_>>(), vec![0, 8, 9]);
        assert_eq!(w.fields[&8]["id"], Value::from("minecraft:cobblestone"));
        assert_eq!(w.fields[&8]["count"], Value::from(7));
        assert_eq!(w.fields[&9], Value::from(7), "the field AFTER the stack is still read correctly");
    }

    /// An EMPTY stack is a real value (an item frame with nothing in it), not a gap -- carried as
    /// null so a consumer can tell "empty" from "never seen".
    #[test]
    fn an_empty_item_stack_is_carried_as_null() {
        let buf = Meta::new().item(8, 7, &[0x00]).item(9, 1, &varint(3)).end();
        let w = walk(&mut Reader::new(&buf), Env::default());
        assert!(!w.truncated);
        assert_eq!(w.fields[&8], Value::Null);
        assert_eq!(w.fields[&9], Value::from(3));
    }

    /// A component with no codec still ends the walk -- it must, because nothing after the stack can
    /// be located -- but now it says WHICH component, and keeps the stack read so far.
    #[test]
    fn an_unknown_component_ends_the_walk_by_name() {
        let comps: std::collections::HashMap<u32, String> = [(300u32, "create:goggles_overlay".to_string())].into_iter().collect();
        let env = Env { items: None, components: Some(&comps), enchantments: None , ..Env::default() };
        let mut stack = vec![0x01u8, 0x09, 0x01, 0x00]; // count 1, item 9, 1 added, 0 removed
        stack.extend_from_slice(&varint(300)); // the mod component
        stack.extend_from_slice(&[0xde, 0xad]); // its opaque payload
        let buf = Meta::new().item(0, 0, &[0x00]).item(8, 7, &stack).item(9, 1, &varint(5)).end();
        let w = walk(&mut Reader::new(&buf), env);
        assert!(w.truncated);
        assert_eq!(w.unknown_component.as_deref(), Some("create:goggles_overlay"));
        assert!(w.fields.contains_key(&0), "fields before the stack survive");
        assert!(!w.fields.contains_key(&9), "nothing after the stack may be claimed");
    }

    #[test]
    fn a_modded_serializer_id_stops_the_walk_rather_than_desyncing() {
        let buf = Meta::new().item(0, 0, &[0x00]).item(30, 200, &[0x01]).end();
        let w = walk(&mut Reader::new(&buf), Env::default());
        assert!(w.truncated);
        assert_eq!(w.fields.len(), 1);
    }

    /// A custom name that is plain text arrives as a bare TAG_String, not a compound. Reading it as
    /// a compound used to fail and end the walk, so the entity lost its name AND every field after
    /// it -- and only for the simplest names, which is the worst way for a bug to be shaped.
    #[test]
    fn a_plain_string_custom_name_is_read_and_does_not_end_the_walk() {
        let mut name = vec![0x08u8, 0x00, 0x03]; // TAG_String, length 3
        name.extend_from_slice(b"Bob");
        let mut body = vec![0x01u8]; // Optional present
        body.extend_from_slice(&name);
        let buf = Meta::new().item(2, 6, &body).item(9, 1, &varint(4)).end();
        let w = walk(&mut Reader::new(&buf), Env::default());
        assert!(!w.truncated);
        assert_eq!(w.custom_name, Some(Some("Bob".to_string())));
        assert_eq!(w.fields[&9], Value::from(4), "the field after the name still decodes");
    }

    #[test]
    fn strings_are_truncated_by_characters() {
        let long = "é".repeat(200);
        let mut w = Writer::new();
        w.string(&long);
        let body = w.into_inner();
        let walked = walk(&mut Reader::new(&Meta::new().item(3, 4, &body).end()), Env::default());
        let s = walked.fields[&3].as_str().unwrap();
        assert_eq!(s.chars().count(), MAX_STRING);
        assert!(s.is_char_boundary(s.len()));
    }
}

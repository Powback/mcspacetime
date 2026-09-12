//! Wire `ItemStack`: a count, an item id, and a **data-component patch**.
//!
//! This is the one decoder in the proxy that **cannot be generic and cannot fail softly**. Since
//! 1.20.5 an item stack carries a patch of data components, and each component's bytes are written
//! by that component's own `StreamCodec` with **no length prefix**. There is nothing to skip. A
//! component this file does not know ends the decode *and* poisons everything after the stack in the
//! packet, because the reader's position is no longer knowable.
//!
//! So the contract here is: decode what is understood, and when something is not understood, **say
//! which component by name and stop**. A decoder that guessed a length would silently corrupt the
//! rest of the packet, which is strictly worse than refusing.
//!
//! ## Layout (read out of the 1.21.1 server jar, not from a wiki)
//!
//! `ItemStack.OPTIONAL_STREAM_CODEC` (which is what entity metadata, containers and slots all use):
//!
//! ```text
//! varint count           <= 0 => the EMPTY stack, and nothing else follows
//! varint item id         into minecraft:item
//! varint added, varint removed      -- DataComponentPatch.STREAM_CODEC
//!   if added == 0 && removed == 0 => empty patch, done
//!   added times:   varint type id (into minecraft:data_component_type), then that type's codec
//!   removed times: varint type id                 (a removal has NO payload)
//! ```
//!
//! ## Why dispatch is by NAME, never by id
//!
//! `minecraft:data_component_type` has **327 entries on this server** against vanilla's 57 — mods
//! register their own, and the ids are assigned per pack. A hardcoded id table would be wrong on any
//! other modpack and silently wrong on this one after a mod update. The proxy already receives the
//! whole registry in the NeoForge frozen sync, so the id is resolved to a name first and the name
//! selects the codec — the same rule `metadata.rs` follows for entity-data indices, and for the same
//! reason.
//!
//! A corollary worth stating: **a mod component in a stack is unreadable and always will be**,
//! because its codec is compiled Java in that mod's jar. Naming it is the whole of what can be done.
//!
//! ## The codecs, and how each was established
//!
//! Read by remapping `libraries/net/minecraft/server/.../server-*-slim.jar` with NeoForge's own
//! `AutoRenamingTool` against Mojang's `server-*-mappings.txt` (`--reverse`), then `javap`-ing
//! `DataComponents`' registration lambdas to get each type's `StreamCodec`. Seven vanilla components
//! are **not network-synchronised at all** and can therefore never appear here: `custom_data`,
//! `intangible_projectile`, `map_decorations`, `debug_stick_state`, `recipes`, `lock`,
//! `container_loot`. Notably `custom_data` — the old NBT `tag` — is server-side only, so a mod that
//! keeps its state there is invisible to any client, this one included.

use crate::wire::Reader;
use serde_json::{Map, Value};
use std::collections::HashMap;

/// `DyeColor` ordinals, which is what `DyeColor.STREAM_CODEC` (an `idMapper`) puts on the wire.
const DYE_COLOURS: [&str; 16] = [
    "white", "orange", "magenta", "light_blue", "yellow", "lime", "pink", "gray",
    "light_gray", "cyan", "purple", "blue", "brown", "green", "red", "black",
];

/// The registries an item stack's ids point into. All optional: a stack can still be decoded to
/// numeric ids before the frozen sync has been seen, and reporting `#31` beats reporting nothing.
#[derive(Default, Clone, Copy)]
pub struct Env<'a> {
    pub items: Option<&'a HashMap<u32, String>>,
    pub components: Option<&'a HashMap<u32, String>>,
    /// `minecraft:enchantment` is a DATAPACK registry, so it arrives in `registry_data` during
    /// configuration rather than in the frozen sync — an ordered list, indexed by id.
    pub enchantments: Option<&'a [String]>,
    /// `minecraft:potion` and `minecraft:mob_effect` ARE in the NeoForge frozen sync (57 and 74
    /// entries here), unlike enchantments.
    pub potions: Option<&'a HashMap<u32, String>>,
    pub mob_effects: Option<&'a HashMap<u32, String>>,
    /// `minecraft:banner_pattern`, another DATAPACK registry -- ordered list from `registry_data`.
    pub banner_patterns: Option<&'a [String]>,
}

impl<'a> Env<'a> {
    fn item(&self, id: u32) -> String {
        self.items.and_then(|m| m.get(&id)).cloned().unwrap_or_else(|| format!("#{id}"))
    }
    fn component(&self, id: u32) -> String {
        self.components.and_then(|m| m.get(&id)).cloned().unwrap_or_else(|| format!("#{id}"))
    }
    fn enchantment(&self, id: u32) -> String {
        self.enchantments.and_then(|v| v.get(id as usize)).cloned().unwrap_or_else(|| format!("#{id}"))
    }
    fn potion(&self, id: u32) -> String {
        self.potions.and_then(|m| m.get(&id)).cloned().unwrap_or_else(|| format!("#{id}"))
    }
    fn mob_effect(&self, id: u32) -> String {
        self.mob_effects.and_then(|m| m.get(&id)).cloned().unwrap_or_else(|| format!("#{id}"))
    }
    fn banner_pattern(&self, id: u32) -> String {
        self.banner_patterns.and_then(|v| v.get(id as usize)).cloned().unwrap_or_else(|| format!("#{id}"))
    }
}

/// A chat component, as `ComponentSerialization.STREAM_CODEC` writes it: one network NBT tag of ANY
/// root type, flattened to its plain text. See `Reader::nbt_tag` -- a text-only component is a bare
/// TAG_String, and reading it as a compound is the bug this helper exists to avoid repeating.
fn read_chat(r: &mut Reader) -> Option<String> {
    match r.nbt_tag().ok()? {
        Some(tag) => Some(crate::wire::component_tag_to_text(&tag)),
        None => Some(String::new()),
    }
}

/// `Filterable<T>` (`Filterable.streamCodec`): the raw value, then an optional filtered version.
/// Books use it for the title and every page. Only the raw value is kept -- the filtered variant is
/// the profanity-filtered text a client would show instead, which a mirror has no use for.
fn read_filterable<T>(r: &mut Reader, mut inner: impl FnMut(&mut Reader) -> Option<T>) -> Option<T> {
    let raw = inner(r)?;
    if r.bool().ok()? {
        inner(r)?;
    }
    Some(raw)
}

/// `MobEffectInstance.Details.STREAM_CODEC`: five scalars then an OPTIONAL nested Details. The
/// nesting is a real recursion (a potion whose effect hides another effect), so it is read as one.
fn read_effect_details(r: &mut Reader, depth: u32) -> Option<Value> {
    if depth > 8 {
        return None; // a cycle cannot happen on the wire, but a corrupt length must not recurse forever
    }
    let mut o = Map::new();
    o.insert("amplifier".into(), Value::from(r.varint().ok()?));
    o.insert("duration".into(), Value::from(r.varint().ok()?));
    o.insert("ambient".into(), Value::from(r.bool().ok()?));
    o.insert("visible".into(), Value::from(r.bool().ok()?));
    o.insert("show_icon".into(), Value::from(r.bool().ok()?));
    if r.bool().ok()? {
        o.insert("hidden_effect".into(), read_effect_details(r, depth + 1)?);
    }
    Some(Value::Object(o))
}

pub struct Stack {
    pub count: i32,
    pub item: String,
    /// component name -> value, only for components whose codec is implemented below
    pub components: Map<String, Value>,
    /// components the stack explicitly *removes* from the item's defaults. Names only — a removal
    /// carries no payload, which is why these are always readable even when an added one is not.
    pub removed: Vec<String>,
}

pub struct Decoded {
    /// `None` for the empty stack (count <= 0), which is a real and common value — an empty
    /// container slot, an item frame with nothing in it.
    pub stack: Option<Stack>,
    /// `Some(name)` if an unimplemented or mod component stopped the decode.
    ///
    /// **When this is set the `Reader` is unusable**: the component carried no length, so the
    /// position after it is unknown and every subsequent field in the packet is garbage. The caller
    /// must abandon the whole packet, not just the stack. The fields already in `stack` are still
    /// correct — everything before the unknown component was read correctly.
    pub stopped_at: Option<String>,
}

impl Stack {
    pub fn to_json(&self) -> Value {
        let mut o = Map::new();
        o.insert("id".into(), Value::from(self.item.clone()));
        o.insert("count".into(), Value::from(self.count));
        if !self.components.is_empty() {
            o.insert("components".into(), Value::Object(self.components.clone()));
        }
        if !self.removed.is_empty() {
            o.insert("removed".into(), Value::from(self.removed.clone()));
        }
        Value::Object(o)
    }
}

/// Read one component's value. `None` means "no codec for this name" — see `Decoded::stopped_at`.
///
/// Every arm below was read off that component's `StreamCodec` in the remapped server jar. Adding an
/// arm on a guess is the one thing that must not happen here: a wrong length does not fail, it
/// desynchronises the packet and produces plausible nonsense somewhere else entirely.
fn read_component(r: &mut Reader, name: &str, env: Env) -> Option<Value> {
    // Strip the namespace only for vanilla: a mod's `foo:damage` is NOT `minecraft:damage`.
    let short = name.strip_prefix("minecraft:")?;
    let v = match short {
        // ByteBufCodecs.VAR_INT
        "max_stack_size" | "max_damage" | "damage" | "repair_cost" | "ominous_bottle_amplifier" | "map_id" | "custom_model_data" => {
            Value::from(r.varint().ok()?)
        }
        // Rarity.STREAM_CODEC is ByteBufCodecs.idMapper -> the enum ordinal as a varint
        "rarity" => Value::from(r.varint().ok()?),
        // ByteBufCodecs.BOOL / Unbreakable.STREAM_CODEC (a record of one bool)
        "unbreakable" | "enchantment_glint_override" => Value::from(r.bool().ok()?),
        // StreamCodec.unit: ZERO bytes on the wire. Present means "flag set".
        "hide_additional_tooltip" | "hide_tooltip" | "creative_slot_lock" | "fire_resistant" => Value::Bool(true),
        // ComponentSerialization.STREAM_CODEC -- a chat component as network NBT, ANY root type.
        // Flattened to plain text, which is what a label wants.
        "custom_name" | "item_name" => Value::from(read_chat(r)?),
        // CustomData.STREAM_CODEC -- an NBT compound.
        "entity_data" | "bucket_entity_data" | "block_entity_data" => {
            let (nbt, _) = r.nbt().ok()?;
            match nbt {
                simdnbt::owned::Nbt::Some(base) => crate::wire::nbt_compound_to_json(&base),
                simdnbt::owned::Nbt::None => Value::Object(Map::new()),
            }
        }
        // ResourceLocation.STREAM_CODEC -- a string.
        "note_block_sound" => Value::from(r.string().ok()?),
        // DyedItemColor: ByteBufCodecs.INT (fixed 4 bytes, NOT a varint) + BOOL
        "dyed_color" => {
            let rgb = r.i32().ok()?;
            let show = r.bool().ok()?;
            Value::from(vec![Value::from(rgb), Value::from(show)])
        }
        // MapItemColor: ByteBufCodecs.INT
        "map_color" => Value::from(r.i32().ok()?),
        // ItemLore: ByteBufCodecs.list(ComponentSerialization.STREAM_CODEC)
        "lore" => {
            let n = r.varint().ok()?;
            if !(0..=1024).contains(&n) {
                return None;
            }
            let mut out = Vec::with_capacity(n as usize);
            for _ in 0..n {
                out.push(Value::from(read_chat(r)?));
            }
            Value::from(out)
        }
        // ItemEnchantments: ByteBufCodecs.map(Enchantment.STREAM_CODEC, VAR_INT) then BOOL.
        // `Enchantment.STREAM_CODEC` is `holderRegistry`, i.e. a PLAIN varint id -- not the
        // `id+1 / 0 then inline` form that `ByteBufCodecs.holder` uses. Getting those two confused
        // is a one-byte shift that would decode into plausible garbage.
        "enchantments" | "stored_enchantments" => {
            let n = r.varint().ok()?;
            if !(0..=1024).contains(&n) {
                return None;
            }
            let mut m = Map::new();
            for _ in 0..n {
                let ench = r.varint().ok()? as u32;
                let level = r.varint().ok()?;
                m.insert(env.enchantment(ench), Value::from(level));
            }
            let _show_in_tooltip = r.bool().ok()?;
            Value::Object(m)
        }
        // ItemContainerContents: ByteBufCodecs.list(ItemStack.OPTIONAL_STREAM_CODEC) -- recursive,
        // and the reason a shulker box in a dropped stack works at all.
        "container" | "charged_projectiles" | "bundle_contents" => {
            let n = r.varint().ok()?;
            if !(0..=1024).contains(&n) {
                return None;
            }
            let mut out = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let d = decode(r, env);
                if d.stopped_at.is_some() {
                    // A nested stack that could not be read poisons the buffer exactly as an
                    // unreadable component does; propagate the stop rather than pretend.
                    return None;
                }
                out.push(match d.stack {
                    Some(s) => s.to_json(),
                    None => Value::Null,
                });
            }
            Value::from(out)
        }
        // WrittenBookContent: Filterable<string(32)> title, string author, varint generation,
        // list<Filterable<Component>> pages, bool resolved.
        "written_book_content" => {
            let title = read_filterable(r, |r| r.string().ok())?;
            let author = r.string().ok()?;
            let generation = r.varint().ok()?;
            let n = r.varint().ok()?;
            if !(0..=1024).contains(&n) {
                return None;
            }
            let mut pages = Vec::with_capacity(n as usize);
            for _ in 0..n {
                pages.push(Value::from(read_filterable(r, read_chat)?));
            }
            let resolved = r.bool().ok()?;
            let mut o = Map::new();
            o.insert("title".into(), Value::from(title));
            o.insert("author".into(), Value::from(author));
            o.insert("generation".into(), Value::from(generation));
            o.insert("pages".into(), Value::from(pages));
            o.insert("resolved".into(), Value::from(resolved));
            Value::Object(o)
        }
        // WritableBookContent (a book and quill): just the pages, as plain strings not components.
        "writable_book_content" => {
            let n = r.varint().ok()?;
            if !(0..=1024).contains(&n) {
                return None;
            }
            let mut pages = Vec::with_capacity(n as usize);
            for _ in 0..n {
                pages.push(Value::from(read_filterable(r, |r| r.string().ok())?));
            }
            Value::from(pages)
        }
        // PotionContents: optional potion holder (a PLAIN varint id -- holderRegistry again),
        // optional INT custom colour, list<MobEffectInstance> custom effects.
        "potion_contents" => {
            let mut o = Map::new();
            if r.bool().ok()? {
                let id = r.varint().ok()? as u32;
                o.insert("potion".into(), Value::from(env.potion(id)));
            }
            if r.bool().ok()? {
                o.insert("custom_color".into(), Value::from(r.i32().ok()?));
            }
            let n = r.varint().ok()?;
            if !(0..=256).contains(&n) {
                return None;
            }
            if n > 0 {
                let mut effects = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let id = r.varint().ok()? as u32;
                    let mut e = Map::new();
                    e.insert("effect".into(), Value::from(env.mob_effect(id)));
                    e.insert("details".into(), read_effect_details(r, 0)?);
                    effects.push(Value::Object(e));
                }
                o.insert("custom_effects".into(), Value::from(effects));
            }
            Value::Object(o)
        }
        // BannerPatternLayers: list of (Holder<BannerPattern>, DyeColor).
        //
        // THE HOLDER HERE IS THE OTHER FORM. `BannerPattern.STREAM_CODEC` is
        // `ByteBufCodecs.holder(key, DIRECT_STREAM_CODEC)`, not `holderRegistry` -- so the varint is
        // `id + 1` for a registry reference and **0 means an inline definition follows**
        // (a ResourceLocation asset id and a translation key). Enchantments use the plain-varint
        // form; confusing the two is a one-byte shift with no error, which is exactly why both are
        // spelled out here.
        //
        // Found by measurement, not planning: an ominous banner on a raid captain was the ONE
        // component in live traffic with no codec.
        "banner_patterns" => {
            let n = r.varint().ok()?;
            if !(0..=256).contains(&n) {
                return None;
            }
            let mut out = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let holder = r.varint().ok()?;
                let pattern = if holder == 0 {
                    // inline: asset id + translation key
                    let asset = r.string().ok()?;
                    let _translation_key = r.string().ok()?;
                    asset
                } else {
                    env.banner_pattern((holder - 1) as u32)
                };
                let colour = r.varint().ok()?;
                let mut o = Map::new();
                o.insert("pattern".into(), Value::from(pattern));
                o.insert("color".into(), Value::from(DYE_COLOURS.get(colour as usize).copied().unwrap_or("unknown")));
                out.push(Value::Object(o));
            }
            Value::from(out)
        }
        _ => return None,
    };
    Some(v)
}

pub fn decode(r: &mut Reader, env: Env) -> Decoded {
    let count = match r.varint() {
        Ok(c) => c,
        Err(_) => return Decoded { stack: None, stopped_at: Some("<truncated>".into()) },
    };
    if count <= 0 {
        return Decoded { stack: None, stopped_at: None };
    }
    let item_id = match r.varint() {
        Ok(i) => i as u32,
        Err(_) => return Decoded { stack: None, stopped_at: Some("<truncated>".into()) },
    };
    let mut stack = Stack { count, item: env.item(item_id), components: Map::new(), removed: Vec::new() };

    let (added, removed) = match (r.varint(), r.varint()) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return Decoded { stack: Some(stack), stopped_at: Some("<truncated>".into()) },
    };
    if !(0..=4096).contains(&added) || !(0..=4096).contains(&removed) {
        return Decoded { stack: Some(stack), stopped_at: Some("<implausible patch size>".into()) };
    }
    for _ in 0..added {
        let id = match r.varint() {
            Ok(i) => i as u32,
            Err(_) => return Decoded { stack: Some(stack), stopped_at: Some("<truncated>".into()) },
        };
        let name = env.component(id);
        match read_component(r, &name, env) {
            Some(v) => {
                stack.components.insert(name, v);
            }
            None => return Decoded { stack: Some(stack), stopped_at: Some(name) },
        }
    }
    // Removals are just ids, so they are always readable -- and they only come after every added
    // component, which is why an unknown added one loses these too.
    for _ in 0..removed {
        let id = match r.varint() {
            Ok(i) => i as u32,
            Err(_) => return Decoded { stack: Some(stack), stopped_at: Some("<truncated>".into()) },
        };
        stack.removed.push(env.component(id));
    }
    Decoded { stack: Some(stack), stopped_at: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Writer;

    fn env_maps() -> (HashMap<u32, String>, HashMap<u32, String>, Vec<String>) {
        let items: HashMap<u32, String> = [(5u32, "minecraft:diamond_pickaxe".to_string()), (9, "minecraft:cobblestone".into())].into_iter().collect();
        let comps: HashMap<u32, String> = [
            (3u32, "minecraft:damage".to_string()),
            (5, "minecraft:custom_name".into()),
            (9, "minecraft:enchantments".into()),
            (16, "minecraft:repair_cost".into()),
            (300, "create:some_mod_component".into()),
        ]
        .into_iter()
        .collect();
        let ench = vec!["minecraft:protection".to_string(), "minecraft:efficiency".into()];
        (items, comps, ench)
    }

    /// A count of 0 is the empty stack and consumes exactly one byte -- an empty container slot.
    #[test]
    fn an_empty_stack_is_one_byte_and_not_an_error() {
        let mut w = Writer::new();
        w.varint(0);
        let buf = w.into_inner();
        let mut r = Reader::new(&buf);
        let d = decode(&mut r, Env::default());
        assert!(d.stack.is_none());
        assert!(d.stopped_at.is_none());
        assert_eq!(r.remaining(), 0, "exactly one byte consumed");
    }

    #[test]
    fn decodes_a_stack_with_no_components() {
        let (items, comps, ench) = env_maps();
        let env = Env { items: Some(&items), components: Some(&comps), enchantments: Some(&ench) , ..Env::default() };
        let mut w = Writer::new();
        w.varint(42); // count
        w.varint(9); // cobblestone
        w.varint(0); // added
        w.varint(0); // removed
        w.varint(0x7f); // a sentinel AFTER the stack, to prove the reader stops in the right place
        let buf = w.into_inner();
        let mut r = Reader::new(&buf);
        let d = decode(&mut r, env);
        assert!(d.stopped_at.is_none());
        let s = d.stack.expect("stack");
        assert_eq!(s.count, 42);
        assert_eq!(s.item, "minecraft:cobblestone");
        assert!(s.components.is_empty());
        assert_eq!(r.varint().unwrap(), 0x7f, "the reader is positioned exactly after the stack");
    }

    #[test]
    fn decodes_damage_name_and_enchantments() {
        let (items, comps, ench) = env_maps();
        let env = Env { items: Some(&items), components: Some(&comps), enchantments: Some(&ench) , ..Env::default() };
        let mut w = Writer::new();
        w.varint(1);
        w.varint(5); // diamond pickaxe
        w.varint(3); // 3 added
        w.varint(1); // 1 removed
        w.varint(3); // damage
        w.varint(137);
        w.varint(5); // custom_name -- a chat component as network NBT
        // simdnbt writes a nameless-root compound; build {"text":"Digger"} by hand
        w.raw(&[0x0a, 0x08, 0x00, 0x04, b't', b'e', b'x', b't', 0x00, 0x06, b'D', b'i', b'g', b'g', b'e', b'r', 0x00]);
        w.varint(9); // enchantments
        w.varint(1); // one entry
        w.varint(1); // efficiency
        w.varint(5); // level 5
        w.bool(true); // showInTooltip
        w.varint(16); // removed: repair_cost
        w.varint(0x7f); // sentinel
        let buf = w.into_inner();
        let mut r = Reader::new(&buf);
        let d = decode(&mut r, env);
        assert_eq!(d.stopped_at, None);
        let s = d.stack.expect("stack");
        assert_eq!(s.item, "minecraft:diamond_pickaxe");
        assert_eq!(s.components["minecraft:damage"], Value::from(137));
        assert_eq!(s.components["minecraft:custom_name"], Value::from("Digger"));
        assert_eq!(s.components["minecraft:enchantments"]["minecraft:efficiency"], Value::from(5));
        assert_eq!(s.removed, vec!["minecraft:repair_cost".to_string()]);
        assert_eq!(r.varint().unwrap(), 0x7f, "the reader ends exactly after the stack");
    }

    /// The whole point of the file: an unknown component must be NAMED and must stop the decode.
    /// The fields read before it survive; nothing after it is claimed.
    #[test]
    fn an_unknown_component_is_named_and_stops_the_decode() {
        let (items, comps, ench) = env_maps();
        let env = Env { items: Some(&items), components: Some(&comps), enchantments: Some(&ench) , ..Env::default() };
        let mut w = Writer::new();
        w.varint(1);
        w.varint(5);
        w.varint(2); // two added
        w.varint(0);
        w.varint(3); // damage -- readable
        w.varint(7);
        w.varint(300); // a mod's own component -- no codec exists for it anywhere but that mod
        w.raw(&[1, 2, 3, 4]);
        let buf = w.into_inner();
        let mut r = Reader::new(&buf);
        let d = decode(&mut r, env);
        assert_eq!(d.stopped_at.as_deref(), Some("create:some_mod_component"));
        let s = d.stack.expect("the fields before the unknown component are still valid");
        assert_eq!(s.components["minecraft:damage"], Value::from(7));
    }

    /// `banner_patterns` uses `ByteBufCodecs.holder`, the form where the varint is `id + 1` and **0
    /// means an inline definition follows** -- unlike `enchantments`, whose `holderRegistry` varint is
    /// a plain id. Both forms appear in the same file, so both are pinned by a test: confusing them
    /// is a one-byte shift that produces plausible garbage rather than an error.
    ///
    /// This component was found by the live census, on an ominous banner carried by a raid captain.
    #[test]
    fn banner_patterns_uses_the_other_holder_form() {
        let comps: HashMap<u32, String> = [(49u32, "minecraft:banner_patterns".to_string())].into_iter().collect();
        let patterns = vec!["minecraft:base".to_string(), "minecraft:creeper".to_string()];
        let env = Env { components: Some(&comps), banner_patterns: Some(&patterns), ..Env::default() };
        let mut w = Writer::new();
        w.varint(1);
        w.varint(5);
        w.varint(1);
        w.varint(0);
        w.varint(49); // banner_patterns
        w.varint(2); // two layers
        w.varint(2); // holder id+1 => patterns[1] = creeper
        w.varint(14); // DyeColor 14 = red
        w.varint(0); // holder 0 => INLINE definition
        w.string("minecraft:block/banner/skull");
        w.string("block.minecraft.banner.skull");
        w.varint(15); // black
        w.varint(0x7f); // sentinel
        let buf = w.into_inner();
        let mut r = Reader::new(&buf);
        let d = decode(&mut r, env);
        assert_eq!(d.stopped_at, None);
        let s = d.stack.unwrap();
        let layers = &s.components["minecraft:banner_patterns"];
        assert_eq!(layers[0]["pattern"], Value::from("minecraft:creeper"), "id+1, not id");
        assert_eq!(layers[0]["color"], Value::from("red"));
        assert_eq!(layers[1]["pattern"], Value::from("minecraft:block/banner/skull"), "inline asset id");
        assert_eq!(layers[1]["color"], Value::from("black"));
        assert_eq!(r.varint().unwrap(), 0x7f, "the inline layer consumed exactly two strings");
    }

    /// A component whose name only *looks* vanilla must not borrow a vanilla codec. `foo:damage` is
    /// a different component from `minecraft:damage` and reading a varint for it would desynchronise.
    #[test]
    fn a_modded_name_that_shadows_a_vanilla_one_is_still_unknown() {
        let comps: HashMap<u32, String> = [(1u32, "create:damage".to_string())].into_iter().collect();
        let env = Env { items: None, components: Some(&comps), enchantments: None , ..Env::default() };
        let mut w = Writer::new();
        w.varint(1);
        w.varint(5);
        w.varint(1);
        w.varint(0);
        w.varint(1);
        w.varint(99);
        let buf = w.into_inner();
        let mut r = Reader::new(&buf);
        let d = decode(&mut r, env);
        assert_eq!(d.stopped_at.as_deref(), Some("create:damage"));
    }

    /// `container` nests stacks, which is what makes a shulker box readable -- and the recursion has
    /// to leave the reader in the right place afterwards.
    #[test]
    fn a_container_component_nests_stacks() {
        let (items, mut comps, ench) = env_maps();
        comps.insert(52, "minecraft:container".into());
        let env = Env { items: Some(&items), components: Some(&comps), enchantments: Some(&ench) , ..Env::default() };
        let mut w = Writer::new();
        w.varint(1);
        w.varint(9);
        w.varint(1);
        w.varint(0);
        w.varint(52); // container
        w.varint(2); // two slots
        w.varint(0); // empty slot
        w.varint(64); // 64 cobblestone
        w.varint(9);
        w.varint(0);
        w.varint(0);
        w.varint(0x7f);
        let buf = w.into_inner();
        let mut r = Reader::new(&buf);
        let d = decode(&mut r, env);
        assert_eq!(d.stopped_at, None);
        let s = d.stack.expect("stack");
        let c = &s.components["minecraft:container"];
        assert!(c[0].is_null(), "the empty slot stays null rather than becoming a fake stack");
        assert_eq!(c[1]["id"], Value::from("minecraft:cobblestone"));
        assert_eq!(c[1]["count"], Value::from(64));
        assert_eq!(r.varint().unwrap(), 0x7f);
    }

    /// A unit component occupies ZERO bytes. If it were read as one byte the next component's id
    /// would be off by one -- a silent, plausible corruption, which is exactly the failure mode this
    /// file exists to avoid.
    #[test]
    fn a_unit_component_consumes_nothing() {
        let comps: HashMap<u32, String> = [(15u32, "minecraft:hide_tooltip".to_string()), (3, "minecraft:damage".into())].into_iter().collect();
        let env = Env { items: None, components: Some(&comps), enchantments: None , ..Env::default() };
        let mut w = Writer::new();
        w.varint(1);
        w.varint(5);
        w.varint(2);
        w.varint(0);
        w.varint(15); // hide_tooltip, no payload at all
        w.varint(3); // damage
        w.varint(11);
        let buf = w.into_inner();
        let mut r = Reader::new(&buf);
        let d = decode(&mut r, env);
        assert_eq!(d.stopped_at, None);
        let s = d.stack.unwrap();
        assert_eq!(s.components["minecraft:hide_tooltip"], Value::Bool(true));
        assert_eq!(s.components["minecraft:damage"], Value::from(11));
    }
}

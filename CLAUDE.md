# CLAUDE.md — mcspacetime gotchas

Context for future work on this repo. The bot joins a NeoForge 1.21.1 server over the native
protocol and mirrors the world into SpacetimeDB. Read `README.md` first for the data model and
the handshake write-up; this file is the sharp edges.

## Toolchain
- **Build the client from inside `client/`** (`cd client && cargo build`), not with
  `--manifest-path` from the repo root — `RUSTC_BOOTSTRAP=1` lives in `client/.cargo/config.toml`
  and cargo only reads a `.cargo/config.toml` relative to the invocation directory. azalea 0.10.3
  (the one release for MC 1.21.1 / protocol 767) needs a few nightly features; bootstrap on stable
  is how we avoid pinning a nightly that would then be too old for the SpacetimeDB 2.10 SDK.
- **`client/vendor/`** holds simdnbt 0.6.1 and azalea-chat 0.10.3 with tiny edits (see each
  `MCSPACETIME_PATCH.md`): simdnbt dropped a removed `slice::array_chunks` call; azalea-chat used
  `serde::__private::ser::FlatMapSerializer`, gone from modern serde. Do not `cargo update` these
  away — they are `[patch.crates-io]` in `client/Cargo.toml`.
- Regenerate `client/src/module_bindings/` with `spacetime generate` only after changing the
  module schema; `rustfmt` may warn it can't format them if a rustfmt component isn't installed —
  harmless.

## The handshake (the whole point — see README for the full sequence)
- All NeoForge negotiation is `minecraft:custom_payload`; we read PLAY packets by hand
  (`client/src/wire.rs`) because azalea's typed decoders reject modded registry/state ids.
- The channel list in `data/neoforge_channels.json` is **learned** from the server's
  `neoforge:modded_network_setup_failed` reasons (`neoforge.rs::learn_from_failure`). Batch
  classification (is this failure batch about CONFIGURATION or PLAY?) is a heuristic: NeoForge
  negotiates CONFIGURATION first and aborts on the first failing protocol, so once the config list
  is accepted, later unknown ids are PLAY. If a pack changes drastically, delete the file and let
  it re-learn (a dozen quick reconnects).
- **Two non-obvious blockers, both solved, both easy to reintroduce:**
  1. **Fabric-API config tasks** (forgified-fabric-api mods: spell_engine, bettercombat). The kick
     `Network configuration task not supported: <id>` names the *task* id; `canSend` checks the
     *payload* id, which is `<task>_sync`. We register `<task>`, `<task>_sync`, `<ns>:ack` ad-hoc
     via `minecraft:register` (NeoForge `hasChannel` honors the ad-hoc set), and reply an Ack on
     `<ns>:ack` whose `code` is the task id (payload id minus `_sync`). The Ack body is a single
     UTF string. If a new mod uses different naming, extend `neoforge::adhoc_candidates` and the
     ack code logic in `protocol.rs`.
  2. **Unconditional optional PLAY channels.** Some mods send an optional play channel on join
     regardless of negotiation (`kubejs:sync_server_data`, `kubejs:sync_stages`,
     `accessories:main`, `jade:server_ping_v1`, `railways:s2c`). Not being in the PLAY setup makes
     NeoForge throw server-side and kick with a **misleading** `multiplayer.disconnect.invalid_player_data`
     (looks vanilla; it is not). These are optional so they never appear in a negotiation probe —
     they must be listed explicitly. **Discovery is dev-time and needs the server log** (the client
     only sees `invalid_player_data`):
     ```
     # with the dev replica running and data/neoforge_channels.json in place:
     run the bot ~6s, then:
     docker compose -f devserver/docker-compose.yaml logs mcdev --tail 200 \
       | grep "may not be sent to the client"     # names the missing channel
     # add it to the "play" map (version "1", flow Clientbound, optional true) and repeat.
     ```
     The loop that did this lives in the shell history; it converged in 5 rounds for this pack.

## The block-state table
- `data/blockstates.json` (344 003 states) is dumped from a server with the **same jars** by
  `tools/kubejs_dump_registries.js` (`/reload` re-runs it; writes to
  `devserver/data/kubejs/exported/`, copy into `data/`). NeoForge state ids = flatten
  `(blocks in registry order) × getPossibleStates()`; the second factor is compiled Java, so it
  can't be derived from the wire — carrying the dump is the fix McWebViewer's ARCHITECTURE.md said
  was missing for mineflayer.
- At connect time the bot re-flattens the dump in the **live** block-registry order (from the
  frozen-registry sync) and logs whether it matched exactly. If a future pack reorders blocks the
  ids still come out right as long as every block is in the dump; a block missing from the dump
  logs a warning and is assumed 1 state (re-run the dump).

## KubeJS reflection is filtered
- KubeJS/Rhino blocks `java.lang.Class`, `getClass()`, and `java.lang.reflect`, so a script can't
  read NeoForge's private `PAYLOAD_REGISTRATIONS` to dump the full channel list directly. That's
  why the optional-play-channel discovery goes through the server *log* instead. The registry/
  blockstate dump works because it only touches public `BuiltInRegistries` + `Block.BLOCK_STATE_REGISTRY`.
- Rhino is not JS-modern: no `let`/`const` inside loops (redeclaration errors), and
  `JsonIO.write` needs the target directory to exist (`kubejs/exported/`).

## NBT
- Network NBT since 1.20.2 has a **nameless root that can be any tag type**. simdnbt's
  `read_unnamed` only accepts a compound root — fine for block entities / heightmaps / chat, but
  spell_engine's `minecraft:spell` dynamic-registry entries use a **byte-array-of-JSON root**
  (`Invalid root type 7`). `wire.rs::nbt_any` reads any root type; use it for registry data.

## Operational
- The bot rewrites `data/neoforge_channels.json` on negotiation success/learning. When running a
  discovery loop or CI that owns the file, set `MCSPACETIME_CHANNELS_READONLY=1` or concurrent bot
  saves will clobber your edits (this bit me — the file kept resetting).
- **Never** stop/restart the live Minecraft container, and never change its online-mode. The
  `devserver/` replica is offline-mode on port 25566 specifically so development doesn't need a
  Microsoft account or the live server. Its `data/` was rsync-copied from
  `../minecraft-create121/data` (same jars ⇒ same ids); it is disposable.
- Live server auth: `MC_AUTH=microsoft` runs azalea-auth's device-code flow once and caches the
  token in `data/ms-auth.json`; the client runs the sessionserver `join` when the server sets
  `should_authenticate`. Offline servers need nothing.
- SpacetimeDB image runs as user `spacetime`; a fresh named volume is root-owned and the server
  dies with `Permission denied` on first start — the compose service runs it as root (`user: "0:0"`)
  to avoid that. Data dir inside the container is `/home/spacetime/.local/share/spacetime/data`.

## Lighting: the packet always carried it; nothing ever read it (2026-09-11)

`ClientboundLevelChunkWithLightPacket` carries sky and block light immediately after the block
entities, and `protocol.rs`'s `0x27` handler **stopped at the block entities** and let the rest fall
off the end of the buffer. So the mirrored world had no light at all and every consumer had to render
it fully lit -- fine on a top-down map, wrong the moment anyone walks into a cave.

`read_light()` now parses the four BitSets and both arrays, and the data lands in a **`chunk_light`
table**, keyed identically to `chunk_section`. Verified live against the dev replica: 117 rows, and
at `sy=5` (y 80..95, above the surface) sky light reads `0xff` with `0xef` where a block casts shade,
while `sy=-4` at world-bottom is all zero. Both are exactly right.

Three decisions worth keeping:

- **Its own table, not columns on `chunk_section`.** `Vec<u8>` cannot have a compile-time default
  (`E0493`: the destructor cannot be evaluated at compile time), so adding columns could only land by
  wiping the database. A new table is a non-breaking migration -- and it is the better design anyway,
  because light and blocks change independently and a terrain-only subscriber should not pay 4 KB a
  section for data it will not read.
- **An absent row means UNKNOWN, not dark.** The server omits sections it has nothing to say about
  and marks *empty* the ones that are uniformly zero -- and empty is a real value, because a sealed
  cave really is dark, so it is stored as 2048 zero bytes (a present row). A consumer that reads the
  absent case as darkness blacks out the world every time a chunk arrives before its light does.
- **A block change must not erase light.** `put_section` serves both the full chunk (which has light)
  and a section re-sent because blocks changed (which does not). Writing empty arrays through would
  blank the light of exactly the sections people edit, and only those -- a miserable thing to debug.
  Empty writes nothing.

### The wasm toolchain, which is not actually missing

`spacetime generate` and `spacetime publish` build the module to wasm, and with Homebrew's cargo on
PATH they fail with "wasm32-unknown-unknown target is not installed". Do not chase that: the
**rustup** stable toolchain at `~/.rustup` already has the target. Just put rustup's cargo first:

    export PATH="$HOME/.cargo/bin:$PATH"

Installing the official `rust-std` component into Homebrew's toolchain does NOT work and is a dead
end -- Homebrew's rustc 1.98.0 is a *different build* from the official dist, so its own `core` is
rejected with "found crate `core` compiled by an incompatible version of rustc" despite the version
strings matching exactly.

### The "garbage coordinate" chunk is somebody's airship (2026-09-12)

`chunk` holding one row at `cx = cz = 1280064` was read as a mis-parse -- identical x and z looked
like two i32s taken from the wrong offset. It is not. Logging the frame shows the bytes after the
coordinates are a textbook heightmaps NBT (`0a 0c 00 0f "MOTION_BLOCKING" 00 00 00 25`, 37 longs),
and writing the chunk out to a region file and decoding it shows **4 sections of
`aeronautics:white_envelope`, oak fence and slab, a chest and a `simulated:altitude_sensor`**.

The Aeronautics mod keeps its craft in a side level 20 million blocks out and streams it over the
ordinary `level_chunk_with_light` packet, with **nothing on the wire saying the chunk belongs to a
different level than the player's**. So the decoder was right all along and the *mirror* is wrong:
that chunk lands in `chunk`/`chunk_section` as if it were overworld terrain, and it will also skew
any per-dimension count taken from those tables.

Fixing the mirror means giving it a notion of which level a chunk belongs to — a schema change, and
the level id is not on the wire, so it would have to be inferred (distance from the player is what
actually separates them). The **world download already refuses it** on exactly that basis:
`anvil::plausible` rejects a chunk more than 512 chunks from the bot, so an airship never lands in
`r.40002.40002.mca` next to the overworld.

### The non_air_count drift: two obvious causes ruled out

A decoder check measured decoded cells against the stored `non_air_count` and found 369/432 sections
exact with the rest out by up to ±536 in **both** directions. Two candidates are now eliminated, so
nobody needs to re-check them:

- `PalettedContainer::set` genuinely returns the OLD value (`world.rs:122-126`), so the incremental
  `+1 / -1` in `set_block` is arithmetically right.
- The air set is not naive: it holds `minecraft:air`, `cave_air` AND `void_air`
  (`blockstates.rs:92`), matching vanilla's `isAir()` — so it agrees with the packet's
  `nonEmptyBlockCount` about what counts.

Bidirectional drift also argues against a systematic definition mismatch, which would err one way.
Whatever this is needs the bot running with instrumentation on both paths; it is not a read-the-code
bug. Worth resolving when the client becomes a server mod, since the same bookkeeping would carry
over — a mod can just ask the section for its real count instead of tracking one.

### Light updates: 0x2a found by observation, and what it does NOT cover

`light_update` is **0x2a**, not the 0x28 a protocol table suggests. It was found empirically: set
`MCST_TRACE_UNHANDLED=1` to log the ids this client ignores, place a glowstone, and watch 2074 bytes
arrive on 0x2a at that instant. Its coordinates are **VarInts**, unlike the i32s the 0x27 chunk packet
uses -- read them as i32 and the rest of the packet silently desynchronises.

The handler and the `update_light` reducer both work: 0x2a packets parse, and they arrive in a burst
as the bot's chunks load.

**What is still missing, measured rather than assumed:** placing a light source does NOT produce a
light update for the chunk that contains it. Five separate glowstones, at 70,70,40 / 71,70,41 /
72,71,42 / 66,68,36 / 68,69,38 / 74,72,44, and not one "light update 4,2" ever appeared -- while
neighbouring chunks 3,2 / 3,3 / 4,1 / 4,3 / 5,2 did get updates, all timestamped within the same
millisecond ~20s after join, i.e. part of chunk LOAD rather than any edit.

So today: light is correct when a chunk arrives, and goes stale as soon as anything changes it, until
that chunk is re-sent. The mirror is not wrong, it is behind.

Two candidates worth testing next, in order: the server may only send light to clients *tracking* the
chunk in a way this bot does not satisfy, or the centre chunk's relight may ride on a packet the match
already handles and drops (a bundle, or the block-update packet itself carrying light). Do NOT assume
the vanilla protocol table settles it -- the id being 0x2a rather than 0x28 is proof enough that a
modded 1.21.1 server is worth observing rather than looking up.

## World download: Anvil region files (2026-09-12)

`WORLD_SAVE=1` writes every chunk the bot is sent to `WORLD_SAVE_DIR/<dimension>/region/r.<rx>.<rz>.mca`
(default `data/worlds`). `client/src/anvil.rs`. Off by default; its own thread behind a **bounded**
`sync_channel` with `try_send`, so a slow disk drops chunks (counted, logged) instead of pushing
back into the packet loop and throttling the chunk stream — the mirror must not pay for the backup.

Verified against the dev replica: 201 chunks, 4 region files, opened with McWebViewer's own
`core/region.ts` + `core/chunk.ts` (`npx tsx src/tools/scan-world.ts …`) — 0 failures, DataVersion
3955, 579 distinct block states, 192 block names, 27 block-entity types, 8 Terralith biomes, and
`render/world.ts` resolves the bot's spawn column to cobblestone at y=61 under air at y=62 with sky
light 15 above and 13 below the floor.

### The Anvil details that actually bite

- **The root NBT tag is NAMED** (empty name). Network NBT has been nameless since 1.20.2, so
  re-emitting the wire shape produces a file every reader rejects. `BaseNbt::write`, not
  `write_unnamed`.
- **The bit width is not stored, it is recomputed from the palette length** — by us when writing
  and by the reader when reading, and the two must agree. It is *not* simply `max(4, ceillog2(len))`:
  vanilla's `PalettedContainer.Strategy` promotes a section with more than 256 distinct states to
  the GLOBAL palette, whose width comes from the block-state registry size. `anvil::block_bits`
  follows vanilla exactly. **McWebViewer uses the naive rule** (`core/chunk.ts::bitsFor`) and so
  would misread such a section — from our files *and* from a genuine vanilla save. It has not come
  up on the replica, and the count is logged, but it is a real McWebViewer bug.
- **A single-state section must have no `data` array at all** (0 bits), not an array of zeroes.
- **Block entities move their identity inside the NBT.** The wire carries position and type
  *outside* it (packed byte, short y, varint type id); Anvil wants `id`, `x`, `y`, `z` as fields of
  the block entity's own compound.
- **Heightmaps were being read and thrown away** in the 0x27 handler. The packet's compound is
  exactly Anvil's root `Heightmaps`, so it is re-emitted verbatim — and it doubles as an
  independent check on the block packing: 3216 sampled columns across 201 chunks all had a solid
  block at the height the *server* said, which no amount of bit-packing luck would produce.
- **Update the region file in place.** A proxy revisits regions constantly; writing a fresh file
  would throw away every chunk of that region it is not currently looking at. That means reading
  the 8 KiB header back on open and rebuilding the sector allocation from it. Payload first, header
  last, header flushed on every chunk — the bot gets killed rather than shut down, so the file has
  to be openable at any instant.
- **Light for the sections above and below the world.** `read_light` returns entries for
  `min_section - 1` and `min_section + count`, which have no blocks and therefore no `SectionData`;
  the mirror drops them, the download keeps them as block-less sections (vanilla writes them too).
- `isLightOn` is only set when every block section actually carried light. Wrongly claiming it
  leaves a partially-downloaded chunk dark forever; leaving it 0 makes vanilla relight on load.

## Entity appearance: publish indices, not meanings (2026-09-12)

The mirror had position, type and custom name for every entity and nothing that said a sheep was
black or a villager a farmer, so `entity.appearance` now carries the `set_entity_data` scalars as
JSON, `{"<index>": <value>}`. `client/src/metadata.rs` walks the metadata generically;
`protocol.rs`'s `0x58` handler merges and rate-limits; the module stores the blob.

**The proxy does not decode "index 17 is sheep colour", and this pack proves why.** Two mods add
fields to `LivingEntity`, so every subclass index shifts by two: sheep wool colour is **19** here,
not the 17 every protocol table lists, and `Mob`'s flags byte is 17 rather than 15. Verified by
summoning mobs with known NBT and reading the column back — the full measured table is in README
under "`entity.appearance`: indices, not meanings". A consumer must derive the mapping for the pack
in front of it.

### Things that cost a round of debugging

- **An added column needs `#[default(...)]` AND has to be last in the struct.** SpacetimeDB 2.10
  refuses "Adding a column appearance to table entity requires a default value annotation" — the
  type having a natural default is not enough — and separately refuses "Reordering table entity
  requires a manual migration" if the field is inserted anywhere but the end. With both right the
  publish is an automatic migration: 344 003 block states, 202 chunks, 4848 sections, 353 block
  entities and 5194 light rows all survived. It still **disconnects every client**, though, so
  "non-breaking" means the data survives, not that subscribers do.
- **`spacetime publish` blocks on a confirmation prompt** when the schema changed. Non-interactively
  it just hangs; pass `--yes=migrate,break-clients` (which still refuses anything needing
  `--delete-data`, so a successful run is itself proof the migration was automatic).
- **Metadata packets are deltas, and the server only ever sends non-default values.** A *white*
  sheep sends no colour field at all, because `ServerEntity.sendPairingData` packs only what differs
  from the class defaults. So an absent index means "default", never "unknown", and the merged map
  has to be kept somewhere — it lives in the client (`Play::entity_meta`), which also lets a packet
  that re-states what we already hold cost nothing at all.
- **Rate-limit rather than blocklist the churn.** Air supply and anger timers tick *every* metadata
  packet: two drowning axolotls were a third of all metadata traffic in a 20-mob test world.
  Excluding them by index would mean encoding exactly the version-specific knowledge above, so
  instead each entity's blob is sent at most once per 500 ms (`META_MIN_INTERVAL`), with the first
  change after a quiet period going immediately — that one carries the appearance. Measured: 512 of
  540 packets changed a carried field, 193 became reducer calls, 319 were coalesced away.
- **Item-shaped entities used to get no blob** — the walk stopped at the `ItemStack` serializer.
  Fixed: see "Item stacks" below. They now carry `{"8":{"id":…,"count":…}}`.

### The dev replica stops sending chunks, and a restart fixes it

Twice during this work the replica served the bot **one** chunk (the Aeronautics airship at
1280064,1280064) and nothing else: no `chunk_batch_start`, no `chunk_batch_finished`, and therefore
no entities either, since entity tracking follows chunk tracking. Same binary that had just pulled
202 chunks; server at 3 ms/tick with no exceptions in its log; the player really was in
`ServerLevel[world]` and teleporting it 2000 blocks away changed nothing. `docker compose restart
mcdev` cleared it.

**Superseded 2026-09-12, and the diagnosis here was probably wrong.** The same symptoms — one chunk,
no `chunk_batch_start`, no entities, a healthy server — turned out on the LIVE server to be a **dead
bot**: a dead player is sent no chunks, and `data get entity <bot> Health` read `0.0f`. A container
restart respawns the player, which is why restarting appeared to fix it. So if a run reports
`play: … 1 chunks, 0 entities`, **check the health first**; the client now requests its own respawn
(see "Routing, and the corpse that looked like a pathfinding bug"). Do not reach for a restart on the
live server, which must never be restarted; the replica is disposable, but restarting it may only be
hiding the same corpse.

## ComputerCraft monitor screens: a full screen every tick, and a reversed palette (2026-09-12)

`computercraft:monitor_client` was arriving, being counted by the payload census, and dropped.
`client/src/computercraft.rs` now decodes it and it lands in a **`monitor` table**, one row per panel
keyed by the panel's origin (top-left) block. The layout was recovered by `javap`-ing
`MonitorClientMessage`, `TerminalState` and `NetworkedTerminal.write()` straight out of
`cc-tweaked-1.21.1-forge-1.119.0.jar` — **there is no protocol table for a mod's payload and it must
not be guessed.** There is no JDK on this host; `docker run --rm -v "$HOME/<dir>:/w" -w /w
maven:3.9-eclipse-temurin-21 javap -p -c <class>` does it (and Docker Desktop will not share
`/tmp` — copy the extracted classes under `$HOME` first).

- **The payload is a FULL screen every time, not a delta** — so unlike `entity.appearance` there is
  nothing to merge, and the useful work is *suppression*: 70% of payloads on the dev replica were
  byte-identical to the row already published (114 of 159 over three minutes) and never reach the
  database. On top of that each monitor's row is rate-limited to one write per 500 ms. Both numbers
  are in the 60-second summary (`monitors: … payloads … unchanged … coalesced … sent`).
- **The palette arrives in the OPPOSITE order to the per-cell colour digits.** The 16 RGB entries
  are `Colour.values()` order (black first, white last); a cell's digit is CC's *Lua* colour index,
  where `colours.white == 0`. The mod's own renderer therefore does `palette[15 - digit]`
  (`FixedWidthFontRenderer.getColour`). `decode` reverses the array once so the published
  `palette[digit]` is direct. A default screen is exactly the case that hides this: `fg` `0` on `bg`
  `f` is white-on-black, and indexing straight gives a plausible-looking black-on-white.
- **Only the origin block is ever addressed** (`MonitorWatcher.getMonitor` returns null unless
  `xIndex == 0 && yIndex == 0`), and `toWorldPos` walks `getRight()`/`getDown()`, so index (0,0) is
  the top-left *as seen facing the screen* — the corner McWebViewer asks for. A 3x4 panel is one row.
- **The payload has no block dimensions at all**, only the terminal size in characters, and the two
  are related by the monitor's text scale which is not on the wire either. `Width`/`Height` come
  from `MonitorBlockEntity.getUpdateTag`, which the mirror already receives with the chunk — so the
  client keeps a small `monitor_geom` map rather than trying to derive geometry from character
  counts.
- **Forgetting a chunk must forget its monitors** (`forget_chunk_monitors`). `unload_chunk` deletes
  the rows; if the client kept its `published` copy, the identical screen arriving after the chunk
  reloaded would compare equal, be suppressed, and the monitor would have **no row at all** until
  its content happened to change.

### The 1377-payloads-a-minute figure did not reproduce, and the reason is in the screens

A live run (own Microsoft account, ~2 minutes) saw **22 payloads over 10 screens**, not ~1377 —
`create_aeronautics_automated_logistics:*` at 121/min each were far louder. The screens say why:
every drone on the DroneMan dashboard reads `offline`, and TaskMan/StorageMan/MapServer only repaint
when something happens. The earlier census was taken with the settlement working. So the write
volume this feature has to survive is **the busy case, which was not measured** — the dedupe and the
500 ms limit are the insurance for it, and `sent` in the summary is the number to watch. Worst case
is bounded at 2 writes/s per monitor of ~9–11 KB each.

### `MCST_DUMP_PAYLOAD`

Set it to a channel name and every raw payload on that channel is appended to
`data/payload-dump/<channel>.bin` as `<u32 LE len><bytes>` (relative to the process's cwd, so
running from `client/` puts it in `client/data/`). It is how the above was read, it is gitignored,
and `client/testdata/monitor_client_71_67_33.bin` is one captured payload kept as a decoder
regression test — the synthetic encoder test only proves the decoder agrees with what the decoder
*thinks* the mod does.

## Item stacks: the one decoder that must refuse rather than guess (2026-09-12)

`client/src/itemstack.rs` decodes `ItemStack.OPTIONAL_STREAM_CODEC` — count, item id, then a
`DataComponentPatch`. It closes the `entity.appearance` hole (dropped items and item frames now carry
their stack at their metadata index) and it is the same wall that stands in front of inventories,
containers and books.

**The property that shapes everything: a component has no length prefix.** Each one is written by its
own `StreamCodec`, so a component with no codec here cannot be skipped — it ends the decode *and*
makes every byte after the stack unlocatable. `Decoded::stopped_at` carries the component's **name**
and the caller must abandon the rest of the packet. Frame boundaries limit the blast radius (each
PLAY frame is length-delimited, so one poisoned stack cannot desynchronise the next packet), but
within the frame nothing after the stack may be believed. **Never add a codec arm on a guess**: a
wrong length does not fail, it produces plausible nonsense somewhere else.

- **Dispatch by NAME, never by id.** `minecraft:data_component_type` has **327 entries on this
  server** against vanilla's 57 — mods register their own and the ids are per-pack. The registry is in
  the NeoForge frozen sync, so the id is resolved first and the name picks the codec. Same rule as the
  entity-metadata indices, same reason. `foo:damage` must NOT borrow `minecraft:damage`'s codec, which
  is why the match strips the `minecraft:` prefix rather than ignoring the namespace.
- **Enchantments are a DATAPACK registry** since 1.21, so they are absent from the frozen sync and
  come from `registry_data` during configuration (`play.enchantment_names`), exactly like biomes.
  Potions and mob effects *are* in the frozen sync.
- **`Enchantment.STREAM_CODEC` is `holderRegistry` = a PLAIN varint id**, not the `id+1 / 0-then-
  inline` form `ByteBufCodecs.holder` uses. One byte of difference, no error, garbage output.
- **A unit component (`hide_tooltip`, `fire_resistant`, …) is ZERO bytes.** Reading one byte for it
  shifts every following component id by one.
- **`ComponentSerialization.STREAM_CODEC` is an NBT tag of ANY root type.** A component that is just
  text serialises as a bare **TAG_String**, so `Reader::nbt` (compound-only) fails on exactly the
  simplest case. `Reader::nbt_tag` was added for it — and **the same latent bug was in the entity
  custom-name path**, where a plain-string name would have ended the whole metadata walk.
- **Seven vanilla components are not network-synchronised** and can never appear: `custom_data`,
  `intangible_projectile`, `map_decorations`, `debug_stick_state`, `recipes`, `lock`,
  `container_loot`. `custom_data` is the old NBT `tag` — server-side only, so a mod keeping state
  there is invisible to every client.

### Reading vanilla codecs: remap the server jar, do not look them up

The vanilla jar in `libraries/net/minecraft/server/1.21.1-*/server-*-slim.jar` is **obfuscated**.
NeoForge ships both the mapping and the tool to undo that:

    L=<live-or-dev>/data/libraries
    cp $L/net/minecraft/server/1.21.1-*/server-*-slim.jar      ~/mcjar/in/server-slim.jar
    cp $L/net/minecraft/server/1.21.1-*/server-*-mappings.txt  ~/mcjar/in/mappings.txt
    cp $L/net/neoforged/AutoRenamingTool/*/AutoRenamingTool-*-all.jar ~/mcjar/in/art.jar
    docker run --rm -v "$HOME/mcjar:/w" -w /w maven:3.9-eclipse-temurin-21 \
      java -jar in/art.jar --input in/server-slim.jar --output deobf.jar --map in/mappings.txt --reverse

Then `javap -p -c` the classes (same maven image; there is no JDK on this host and Docker will not
share `/tmp`, so keep everything under `$HOME`). `DataComponents`' registration lambdas name each
type's codec — `javap`-ing that one class gave the complete 57-row name→codec table in one pass.

### The census said: build nothing else yet

`MCST_DUMP_PAYLOAD` is no use here (metadata is not a mod channel), so the instrument is the decoder
itself: every component name it meets is counted and the ones with no codec are logged as a to-do
list (`item stack components with NO codec: …`). Over **three minutes of live traffic: 28 stacks, ZERO
components, zero unknowns** — every stack in real entity metadata was a bare drop (`egg`,
`glow_ink_sac`, `bone`, `arrow`) with an empty patch. `0 truncated by an unskippable serializer`, down
from nonzero.

So the ~30 codecs that exist were driven by **summoned** test items on the dev replica, not by demand,
and the honest reading is that entity metadata alone will not exercise them. The codecs will earn
their keep when containers and inventories land, which is where non-trivial stacks actually live
(`set_equipment`, 0x5a, is still unhandled and is the cheapest next source of real stacks).

Verified by summoning items with known NBT on the dev replica and comparing the mirror against the
server's *own* `data get entity` view — which also caught two false alarms: SNBT that rcon silently
dropped (`custom_name`/`lore` written with the wrong quoting) looked like decoder bugs and were the
test command. Always check `data get entity` before blaming the decode.

## Container sessions, and the bot acting on the world (2026-09-12)

`ContainerOpen`/`SetContent`/`SetSlot`/`Close` are handled, `container` is a table, and
`request_open_container` makes the bot right-click a block. **This was never blocked on a server mod:**
the proxy is a real connected client, so the server sends *it* these packets like any other client.
The embedded-channel trick is only needed for the many-players idea.

### Packet ids: stop hunting them, they are in the jar

`data/packet_ids.json` + `tools/dump_packet_ids.py`. The ids are the `addPacket` **registration order**
in `GameProtocols`, which one `javap` extracts. The generated table agreed with all 13 ids this project
had found by hand, **including `light_update` = 0x2a**, which a protocol table got wrong and which cost
a glowstone-placing session to discover. 124 clientbound, 58 serverbound.

**The trap that makes a wrong table look right:** GAME packets are interleaved with COMMON, COOKIE and
PING ones (`CommonPacketTypes`, `CookiePacketTypes`, `PingPacketTypes`). Matching only
`GamePacketTypes` gives a table that is correct for the first 14 entries and then drifts by one — and
the first few agreeing is exactly what makes you trust it. I hit this: `container_close` came out 0x12
instead of 0x13 because `cookie_request` (0x16) was missing. Match `\w*PacketTypes` in every protocol
sub-package.

### The container-id width is NOT consistent, in the same feature

- `open_screen` (0x33): container id is a **VarInt**
- `container_set_content` (0x13) / `container_set_slot` (0x15) / `container_close` (0x12): a single
  **byte**

Reading all four the same way works fine until a window id goes above 127. Read each as the jar says.

Other layout details worth not re-deriving:

- `container_set_slot` slot **-1 is the CARRIED item**, not slot index -1.
- The slot list is the **whole menu**, container plus the player's inventory: a single chest is 63
  slots, not 27.
- `set_equipment` (0x5b) repeats `(i8 flags, ItemStack)` with `flags & 0x7f` the `EquipmentSlot`
  ordinal and the top bit meaning "another follows". Ordinals are mainhand, offhand, feet, legs, chest,
  head, **body** — BODY is new in 1.21 (horse/wolf armour), so a 1.20 table mis-names it.
- `use_item_on` (serverbound 0x38) is `varint hand`, `i64 blockPos`, `varint face` (Direction ordinal:
  down, up, north, south, west, east), three `f32` cursor offsets, `bool isInside`, `varint sequence`.
  The sequence must increment; a client that reuses one gets its block changes rolled back.

### A container is session state, and close has to be authoritative

The row is DELETED on close, disconnect and dimension change — never marked closed. A stale open
container shows a chest's contents from ten minutes ago as if they were live, which is worse than
showing nothing. Two consequences that are easy to get wrong:

- **The server does not echo a clientbound close for a client-initiated one.** After sending
  serverbound `container_close` the bot has to drop the row itself or it lingers forever.
- **The bot's own inventory is window 0**, arrives unprompted on join, and has no `open_screen` — so
  it has no menu type or title, and that is correct rather than missing.
- **A SIGKILLed bot leaves its rows behind** and nothing in-process can prevent that. A server-side
  disconnect clears them (`main.rs`), and the next connect clears them regardless — verified: open a
  chest, `kill -9`, the row survives; restart, and it is gone before the new window 0 appears. A
  consumer that cares about liveness should check `bot_status`.

### Acting on the world: the failure that reports nothing

`ServerGamePacketListenerImpl.handleUseItemOn` calls `player.canInteractWithBlock(pos, 1.0)` —
`blockInteractionRange()` (4.5) plus that padding, measured from the **eyes** to the nearest point of
the block — and if it fails the server **says nothing at all**. So:

- reach is checked client-side against the same 5.5 and `rejected` with the distance, rather than sent
  and silently dropped;
- "no `open_screen` within 1500 ms" is the only available failure signal, and becomes `failed`;
- pending requests are expired on connect (`expire_pending_bot_commands`) and the command channel is
  drained **after** that, because the initial subscription delivers existing rows as inserts. Expire
  first, then drain: the other order can hand the bot a command that is already marked rejected.
- The bot CAN now move (`request_walk_to`, 2026-09-12), so reach is no longer fixed to the spawn
  point — but it walks in a straight line only, so what it can open is what it can walk to. See "The
  bot can walk" below.

One structural change: the play loop's 90 s dead-connection timeout used to BE the idle
`tokio::timeout`. It now wakes every 200 ms to poll for commands and tracks `last_packet` separately —
without that split, shortening the poll would have silently disabled the dead-connection detector.

### The container census: real stacks at last, and one real gap

Entity metadata had given 28 stacks with ZERO components. Containers and equipment changed that
immediately — live: **199 stacks, `damage`=84, `charged_projectiles`=42** (pillager crossbows), and
**`banner_patterns`=2 with NO codec**: the ominous banner on a raid captain. That is the one component
in live traffic the decoder could not read, and it is now implemented.

**It also caught the exact trap the itemstack header warns about.** `BannerPattern.STREAM_CODEC` is
`ByteBufCodecs.holder(key, DIRECT_STREAM_CODEC)` — the form where the varint is **`id + 1`** and **0
means an inline definition follows** (an asset ResourceLocation plus a translation key). Enchantments
use `holderRegistry`, a plain id. Both forms now have a test, because confusing them is a one-byte
shift with no error. Verified against vanilla's known ominous-banner layers (rhombus/cyan,
stripe_bottom/light_gray, stripe_center/gray, border/light_gray, …), which came out exactly right.

A real HiveMind spoils chest at 62,68,32 decoded 27/27 slots with `undecoded_component` empty —
including `create:veridium` and `computercraft:wired_modem_full`, i.e. **mod items are fine**; it is
mod *components* that cannot be read, and this pack's mod items do not carry any.

## ContainerClick: predict nothing, and the cursor is a pseudo-window (2026-09-12)

`request_click_slot(window_id, slot, button, mode)` sends `container_click` (serverbound 0x0e):
`u8 containerId`, `varint stateId`, `i16 slotNum`, `u8 buttonNum`, `varint clickType`,
`map<i16, ItemStack> changedSlots`, `ItemStack carriedItem`.

**Only PICKUP (mode 0) and QUICK_MOVE (mode 1) are implemented.** Swap, clone, throw, quick_craft and
pickup_all are not sent, and neither are vanilla's special slots −1 and −999 (−999 with pickup THROWS
the carried stack on the floor). Anything else is rejected client-side.

### The server's failures are all silent, which is why validation is client-side

`handleContainerClick`, read from the jar:

- `containerId != menu.containerId` → **returns, doing nothing**, with no reply.
- `!menu.isValidSlotIndex(slotNum)` → logs at debug and **returns**. Note `isValidSlotIndex` is
  `i == -1 || i == -999 || i < slots.size()` — so **negative indices other than those two pass the
  check** and then throw inside `doClick`. Validate `0 <= slot < len` yourself.
- `stateId != menu.getStateId()` → **not** a rejection: the click is applied and the server answers
  with `broadcastFullState()` (a whole `container_set_content`) instead of `broadcastChanges()`.

So an unvalidated click is indistinguishable from a working one that did nothing.

### Predict nothing — and then adopt the claim you made

`changedSlots`/`carriedItem` are **what the client claims it now believes**. The server writes them
into its model of us (`setRemoteSlotNoCopy` / `setRemoteCarried`) and then `broadcastChanges()` sends a
correction for every difference from reality. So sending an **empty map** asks it to correct
*everything* that changed, and `container.slots` can be written from server packets only — no second
copy of `doClick`, nothing to reconcile.

**The subtlety, which one click cannot reveal and two can:** claiming an empty cursor is itself a
statement, so when a click genuinely leaves the cursor empty **no correction arrives**. Pick a stack up
and put it back, and `carried` keeps the stack forever. The fix is to adopt the claim on send (set the
session's `carried` to empty), which makes the *absence* of a correction mean "empty" rather than
"unchanged". The slot list needs no equivalent because claiming an empty `changedSlots` already says
"unchanged", which is what we hold.

### THE CURSOR IS A PSEUDO-WINDOW −1

`ContainerSynchronizer.sendCarriedChange` sends `ClientboundContainerSetSlotPacket(-1, stateId, -1,
stack)` — **container id −1**, not the window the click was for. Found by measurement: the first live
pickup emptied a chest slot and the mirror showed the stack going nowhere, because the window −1 packet
was looked up as a container, not found, and silently dropped. The cursor belongs to the *player* and
only one menu is open at a time, so −1 is attributed to the open container (or window 0 when that is
all there is).

That is two traps in one feature where the wrong reading produces *plausible* output rather than an
error — same shape as the CC palette order and the `holder` vs `holderRegistry` varint.

### What was actually clicked

Dev replica: pickup, right-click-half, place-back, and quick_move in both directions, each reversed to
the starting state, with `data get block` confirming the chest matched the mirror at every step.

Live (own Microsoft account, ~4 minutes): **a chest placed for the purpose** at 65,70,36 — verified air
first with `execute if block` — holding one cobblestone. One pickup, one place-back, then the chest
emptied and `setblock … air`, leaving the block air again with no dropped items. **HiveMind's storage
was not touched.** Do it this way if it needs repeating; do not practise on the base.

### `ContainerClick` is where this stops

Nothing sends `swap`/`throw`/`quick_craft` and nothing crafts. The bot could not
walk when this was written; `request_walk_to` (below) lifted that, within the limits noted there.

## Multiple servers: scope by database, not by column (2026-09-12)

Nothing in the schema is keyed by server. `chunk_key(cx, cz)` spends all 64 bits on the
coordinates, and the Anvil saver updates `.mca` files **in place** under `<root>/<dimension>/region`
with no server in the path. So pointing the client at a second server overwrites the first one's
world, in the mirror and in the backup. **This already happened**: a run against the dev replica on
`:25566` wiped the live mirror (`monitor` 0 rows, `entity` 0, `bot_status.server_addr`
`127.0.0.1:25566`), which blocked a whole session of monitor-viewer verification because there were
no real panels left to verify against.

The fix is `client/src/scope.rs`: the database name and the world-save directory both default to a
slug of `MC_ADDR`.

- **Why not a `world` column on `Chunk`.** It would not have been enough. `BlockStateRow` and
  `RegistryEntry` are primary-keyed on *registry ids*, and modpack A's block-state 4231 is a
  different block from modpack B's. A mismatched registry does not leave a gap, it renders every
  block **wrong**, silently — worse than missing data. Only isolating the whole database scopes the
  registries too. This is the argument for per-database, and it is the reason to resist the
  cheaper-looking column.
- **`spacetime publish` rejects an underscore in a database name** — `mcspacetime_x` fails with
  "invalid characters in database name"; `mcspacetime-127-0-0-1` is accepted. The slug therefore uses
  a dash, which a path segment accepts too, so one separator serves both. A test pins this, because
  the failure is at publish time and nowhere near the code that chose the name.
- **The hash in the slug is hand-rolled FNV-1a, deliberately.** It names a directory on disk and
  `DefaultHasher` is explicitly not stable across Rust releases, so a toolchain upgrade would
  re-point the backup and start a second copy of the world beside the first. A test pins the literal
  — and it earned that immediately, catching the prime written `0x1000_0000_01b3`, one hex digit too
  long. Every other test in the file passed with the wrong prime, because a hash that is wrong but
  consistent still satisfies "distinct" and "bounded".
- **Ambiguity resolves toward *separate*.** `example.com` and its IP are one server to everyone but
  this function, so addressing a host two ways mirrors it twice. That wastes disk. Merging two
  servers into one name corrupts. An explicit `:25565` is stripped, though, since that really is the
  same server spelled two ways.

**Migration.** Existing data sits in the database named `mcspacetime`, which no address now maps to.
Either publish to the new name and re-walk the world, or pin the old one explicitly:

    STDB_MODULE=mcspacetime          # keep using the pre-2026-09-12 mirror
    WORLD_SAVE_DIR=data/worlds       # and the pre-2026-09-12 download root

`data/worlds/minecraft_overworld/` is from before the split and belongs to whichever server was last
run; new downloads land in `data/worlds/<slug>/minecraft_overworld/`.

## The bot can walk (2026-09-12)

`request_walk_to(x, y, z)` + `client/src/walk.rs`. The bot could open a chest but not cross the room,
which capped the whole mirror — and the world download — at whatever is within view distance of spawn.

**It is a straight-line walker, not a pathfinder.** It steps toward the target, slides along walls,
steps up and down one block, and walks off ledges it can survive. A wall between here and there fails
the command with the distance still to go, rather than solving it. That is the honest first cut: the
thing that unblocks "mirror more than spawn" is walking, and a stall that says so beats an A* that
quietly swims through lava.

### Verified live, and the verification is the interesting part

Walked off the spawn ledge at y=68, fell six blocks, landed on the settlement floor at y=62 and
reached the target — 29 steps, `done`. Checked properly rather than by reading our own mirror back:

- **The server's own `data get entity Powback Pos` agreed exactly** — `[64.5d, 62.0d, 40.155d]`.
- **No `moved too quickly`, `moved wrongly` or flying warnings** in the server log, so the movement is
  accepted as legitimate rather than tolerated.
- The block under its feet (`64,61,40`) is **air** — and that is correct, not a float: the box spans
  z 39.86–40.46, and `64,61,39` is solid. It is standing on a block edge, which is what the
  multi-column `footing` is for.

### Three things that were wrong first, all of which produce plausible-looking output

- **`footing` sampled only the centre column** while `box_fits` checked every column the box overlaps.
  A bot straddling a stair therefore read the LOWER floor and rejected the climb, so it could not use a
  single step anywhere. The fix is to take the HIGHEST support among the overlapped columns, which is
  what standing on an edge means.
- **`MAX_DROP` was 4.0**, meaning one step could descend four blocks — but a step is ONE position
  packet, so that asks the server to accept a four-block drop in a single tick. It is 1.0 now, and
  anything further is a fall paid out over several ticks the way a client does.
- **Refusing every drop left the bot stranded.** It joined standing on a two-block ledge (`64,67,36`
  and `65,67,36` solid, the floor at y=61 six blocks below) so every direction was a cliff and
  `walk_to` was correct and useless. Walking off a ledge is now allowed when the landing is known and
  within `SAFE_FALL` (8 blocks — `distance - 3` damage points, so 5 of 20, and the bot cannot heal).

### What it still cannot do, measured rather than assumed

- **It cannot descend a shaft it is standing on the lid of.** Asked to walk to the floor directly
  beneath its ledge it arrives horizontally and stops, because a fall needs a sideways step and every
  sideways step is away from the target. It says exactly that now; it first reported "blocked in every
  direction ... 0.00 blocks short", which reads like a walker bug rather than the geometry it is.
- **Long walks inside the settlement need pathfinding.** The base is a hollow structure: along x=64
  there is no floor at all at y=61 beyond z=41, so the reachable area from where it lands is small.
  Nothing to fix in the walker — the next real step is routing.
- **Unknown block names are treated as SOLID.** The two failures are not symmetric: a wall read as air
  desyncs the bot from the server, a flower read as a wall just stops it. So modded decoration can
  block a route, and that is the deliberate direction to fail in. `grass_block` vs `short_grass` is the
  trap a substring match falls into, and it has a test.
- **Gravity only applies while walking.** Making the bot fall whenever unsupported would change what
  the plain mirroring bot does — this one spawns on a ledge, so it would drop off on connect and land
  somewhere nobody chose.

## Routing, and the corpse that looked like a pathfinding bug (2026-09-12)

`client/src/path.rs` — A* over block nodes, using **the walker's own geometry** (`box_fits` and the
multi-column `footing` from `walk.rs`), so a route the search promises is one the walker will follow.
`walk_to` now plans a route, simplifies it to corners, walks the legs, and **re-plans once from where it
actually is** if it stalls, before reporting failure.

This closed the two things the straight-line walker could not do, both measured rather than assumed:

- **Descending a shaft it is standing on the lid of.** A fall needs a sideways step to fall FROM, and
  every sideways step is *away* from the goal, so a walker that only reduces distance can never take
  one. Verified live after the respawn below: from the spawn ledge at y=68 down to y=61, 27 steps,
  `done`, and health stayed at **20.0** — the fall pricing sent it down stairs rather than off the edge.
- **Routing round an obstacle.** The settlement is hollow; the gap at x=66 splits the floor in two and a
  straight line just stalls against it.

### The bot walked itself to death, and a dead player is sent NO CHUNKS

The failure presented as routing: every `walk_to` came back `no route ... or the world around the bot is
not mirrored yet`, and `chunk` held **1 row**. The process looked healthy and kept logging mod payloads
at 121/min. `data get entity Powback Health` read **`0.0f`**.

Two six-block falls while routing, no way to heal or eat, and the client had no concept of death. So:

- **`player_combat_kill` (0x3c) now asks for a respawn** — serverbound `client_command` (0x09) with
  action 0. The `respawn` (0x47) *handler* already existed; nothing had ever asked for one.
- **Zero health is the real signal, not the kill packet.** A bot that reconnects to a body it left
  behind is told `set_health 0.0` and then sent nothing at all — no kill packet, because the death
  happened in a previous session. The first fix logged "expecting player_combat_kill" and waited for a
  packet that was never coming. `set_health` (0x5d) requests the respawn itself, guarded by
  `RESPAWN_RETRY`.
- **The fall limit now tracks health** (`survivable_drop`): vanilla damage is `distance - 3` points, so a
  full-health bot may take 8 blocks and a hurt one much less, keeping `FALL_RESERVE` spare. A limit that
  ignores health is a bot that dies on its fourth drop.

Verified: the proxy rejoined a corpse, requested the respawn, came back to `20.0f` on the spawn ledge,
and the mirror refilled to 454 chunks / 10 896 sections / 127 entities / 10 monitors / 1869 block
entities.

**This is very likely what the "the dev replica stops sending chunks, and a restart fixes it" note
above was actually describing** — one chunk, no `chunk_batch_start`, no entities, a healthy-looking
server — because a restart respawns the player. I cannot prove it of that occurrence, but the symptoms
match exactly, and the remedy it recommends is unavailable here: the live server must never be
restarted. **Check `data get entity <bot> Health` before suspecting the server.**

### Two corrections to the router worth keeping

- **A node below the goal is not arrival, and this reported SUCCESS.** With a horizontal-only heuristic
  a node directly under the goal scores **zero** — the best score there is — so the search fell down a
  gap and the walk said `done` at 70,**55**,38 for a goal of 70,**61**,38. The heuristic now includes
  vertical terms and the arrival test treats height as exact, not slack. Silent success is the worst way
  to be wrong, so it has two tests.
- **The heuristic must be the MAX of its components, not the sum.** One step can move a block
  horizontally *and* climb one, so summing claims two units of progress for one move, overestimates, and
  A* stops returning shortest paths. A separate `closeness` (squared 3D distance) does the
  "closest reachable point" reporting, because an admissible heuristic cannot see horizontal progress
  when the vertical gap is larger — with only the heuristic, an unreachable floor above yielded no
  closest point at all and `find` returned `None`.

### Open, with evidence

**`moved too quickly!` twice, at the respawn only** — delta `-5.85, 13.0, -2.0`, exactly the death
position to the spawn point, at the same second as `respawn into minecraft:overworld` (three
`bot placed` lines, so three teleports in ~20 ms). Every successful walk logged **zero** complaints and
the server's `Pos` matched the mirror exactly afterwards, so this is a respawn-sequence artifact rather
than a movement problem: a position reply going out before the server has processed the matching
`accept_teleportation`. Benign — the server corrects — but it is the thing to look at if a respawn ever
does leave the bot desynced.

## Passability from collision shapes, not block names (2026-09-12)

`tools/kubejs_dump_collision.js` → `data/collision.json` → `BlockStates::collision_empty`. **12 189 of
344 003 states have an empty collision shape, in 894 ranges, 0 that could not be asked** — a 29 KB file
covering every block in the pack, mods included.

The walker used to decide what it could move through from the block's NAME, and a name is a guess: in a
modded pack most unrecognised names are real blocks, so unknown resolved to SOLID. Safe, but it means
any mod's decoration silently walls off a route the bot could really walk — and "any modded pack" is the
whole point of this project. Collision shapes are compiled Java, exactly like `getPossibleStates`, so
carrying a dump is the same fix `blockstates.json` already is, taken from the disposable replica because
the same jars give the same shapes.

### It is the UNION with the name heuristic, deliberately

`passable = collision_empty || name_says_passable`. Neither alone is right, and the live table shows why:

- `minecraft:grass_block` is correctly **non-empty** — the trap the name list had to special-case (read
  as a plant, the bot walks into the ground on every natural surface in the world). Collision gets it
  right for free.
- `minecraft:ladder` is **non-empty** too, because a ladder has a real if thin collision shape — and it
  is still somewhere a player stands. Collision alone would wall off every ladder in the world.

Both sources only ever *add* passability, so the union is no less safe than the stricter of them.
Hazards are checked **before** collision: lava has no collision shape at all and would otherwise read as
a perfectly good place to walk.

### The thing that would have failed silently

**`flatten` renumbers every state id** when the live registry order differs from the dump's — and it does
on this pack (`live block registry differs from the dump (9740 live vs 9740 dumped blocks)`). A collision
table keyed by the dump's ids would then describe the *wrong blocks*, marking arbitrary geometry
walk-through, with nothing in any log connecting the desync to the dump.

So collision is stored **parallel to `per_block`** (block → its states, in the block's own order), which
is the pairing the dump and the live server actually agree on, and `flatten` rebuilds the flat table
through the same ordering it builds `table` with. `COLLISION_SURVIVES_THE_LIVE_REGISTRY_REORDERING` pins
it by reordering a fixture's registry and checking the flags followed the blocks. A dump whose state
count disagrees is **refused**, not applied, for the same reason.

### Gotchas worth not rediscovering

- **KubeJS server scripts share ONE Rhino scope.** A top-level `const $Block` collides with the identical
  declaration in `mcspacetime_dump.js` and Rhino aborts the load with "redeclaration of const $Block",
  which takes the *other* script down too — so the blockstates dump silently stops being produced as
  well (`Loaded 1/3 KubeJS server scripts ... with 2 errors`). Wrap each script in an IIFE.
- **`EmptyBlockGetter` is `net.minecraft.world.level.EmptyBlockGetter`**, not `net.minecraft.world.*`.
  The server jar is obfuscated, so the only authority on hand is `server-*-mappings.txt` next to it:
  `grep -oE "net\.minecraft\.world[a-zA-Z.]*EmptyBlockGetter" mappings.txt`. Guessing the package costs
  a reload cycle per attempt.
- **`JsonIO.write` emits every number as a FLOAT** (`[[0.0, 0.0], ...]`), so the Rust side must
  deserialize `(f64, f64)`; an integer type fails to parse the file the tool actually writes.
- **`/kubejs reload server_scripts` via `rcon-cli` did not take** (the command came back with a parse
  marker). Plain `reload` re-runs the scripts, which is what the existing note says.

## Container clicks: swap and pickup_all added, throw deliberately not (2026-09-12)

`request_click_slot` now sends **0 pickup, 1 quick_move, 2 swap, 6 pickup_all**. Verified live: mode 2
came back `done` ("the server sent no correction: it applied no change" — both slots were empty, which is
a legitimate no-op), mode 4 came back `rejected`, and `@e[type=minecraft:item,distance=..12]` found
nothing on the floor afterwards.

- **THROW (4) will not be added.** It drops the stack on the ground. Slot **-999** with pickup does the
  same and is refused for the same reason. This is a standing constraint on the project, not a to-do.
- **QUICK_CRAFT (5) is a three-phase drag** (begin / add slot / end) with its own server-side state
  machine, not a click. Sending one packet of it leaves the drag half-open, so it needs its own request
  shape rather than a `mode` value.
- **CLONE (3)** is creative-mode only; the server drops it for a survival player with no reply.
- **`button` does not mean the same thing in every mode.** For swap it is the **hotbar slot** (0-8, or 40
  for the offhand), so the old blanket `button must be 0 or 1` check could not express a legal swap at
  all. Each mode now carries its own valid range, and slot 40 is refused for anything but window 0.

## Climbing, and the drag (2026-09-12)

### Ladders: the reachability limit was the search, not the world

The bot could not climb, and `STEP_UP` is one block, so whole floors of a built structure were
unreachable — reported as "no route", which was true of the search and false of the world. `walk::climbable`
(ladder / vine / scaffolding / cave_vines, substring-matched so modded variants count) plus vertical
neighbours in `path.rs` fixed it.

Three things this needed that are easy to miss:

- **Collision cannot settle it.** A ladder has a real if thin collision shape, so `collision_empty` says
  "solid" while a player both stands in it and climbs it. Climbables are a separate predicate for exactly
  this reason, and it is why `passable` is the union of collision and names rather than collision alone.
- **Hanging on a ladder is not falling.** Without that case in `fall_step` the bot "falls" one tick after
  every climb step and never ascends.
- **Climb at `CLIMB_SPEED` (0.118), not `WALK_SPEED`.** Climbing at walking speed is a speed violation.

`standable` now accepts a climbable cell, so the search and the walker still share one definition of
"can the bot be here" — the property that keeps a promised route walkable.

### `request_drag`: the whole drag, or none of it

`QUICK_CRAFT` spreads the carried stack over several slots. The reducer takes `Vec<i32>` and **sends all
three phases in one go** — begin, one packet per slot, end — because it is a server-side state machine and
exposing the phases separately invites a caller to leave `quickcraftStatus` set. (Vanilla self-heals:
`doClick` resets a dangling drag on the next non-quick_craft click. But then that ordinary click silently
does nothing except clean up, which is worse to debug than the drag failing.)

**Encoding read from the deobfuscated jar, not from memory** — the repo's own rule, and the deobf recipe
above is what it is for:

    getQuickcraftHeader(button) = button & 3          // 0 begin, 1 add slot, 2 end
    getQuickcraftType(button)   = (button >> 2) & 3   // 0 split evenly, 1 one each, 2 creative
    button = stage | (type << 2)

`isValidQuickcraftType` accepts 0 and 1 for any player but 2 only with `hasInfiniteMaterials`, which this
bot does not have.

- **Slot −999 on the begin and end packets is correct and does NOT drop anything.** The −999 that throws a
  stack on the floor is −999 with **PICKUP**, which stays refused. Two different meanings for one number.
- **Refuse a drag with an empty cursor.** The server accepts every packet and changes nothing, which is
  indistinguishable from success.
- **The `slots` column is a comma-separated String, not a `Vec<i32>`**, because a `Vec` column cannot have
  a compile-time default (`E0493`) and without one the column could only be added by wiping the database.
  The reducer takes a real `Vec<i32>` and joins it, so no caller sees the encoding.

Verified live, entirely inside the bot's own inventory — no blocks placed, nothing dropped: `give` 8
cobblestone, pick up menu slot 36 (window 0 is `0` result, `1-4` grid, `5-8` armor, `9-35` main, `36-44`
hotbar, `45` offhand), drag one-each across 9/10/11, and the server's own `data get entity Powback
Inventory` read exactly `count: 1` in slots 9, 10 and 11. Then the remainder was put back in slot 12 and
`clear`ed; inventory empty, no item entities within 16 blocks, health 20.

## THROW, added on request (2026-09-12)

Click mode **4 (THROW)** is implemented. It was previously withheld under the standing instruction that
the bot must never drop items on the ground; **the user lifted that explicitly and asked for it ungated**,
so it is a plain mode alongside the others and callers own the consequences.

Semantics read from the deobfuscated jar, not assumed:

    clickType == THROW && getCarried().isEmpty() && slotId >= 0
    count = (button == 0) ? 1 : slot.getItem().getCount()
    slot.safeTake(count, MAX_VALUE, player)  ->  player.drop(stack, true)

- **button 0 drops ONE item, button 1 drops the whole stack.**
- **The cursor must be empty.** With something carried the server returns without a reply, which is
  indistinguishable from a throw that did nothing — so the bot refuses that case rather than sending it.
- Slot **−999 with PICKUP** is a *different* way to drop the cursor and is still refused; nothing asked
  for it, and it is easy to send by accident. Ask if you want that one too.

Still not available as a `mode`: **5 (quick_craft)**, which is a three-phase drag with its own
`request_drag` reducer, and **3 (clone)**, which is creative-only and dropped silently for this bot.

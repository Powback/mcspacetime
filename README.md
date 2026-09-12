# mcspacetime

A headless Minecraft **Java 1.21.1** client that joins our NeoForge server over the **native
Minecraft network protocol** (like a real player — no RCON, no mod) and streams everything it
sees — terrain (chunks/sections), block updates, block entities (incl. ComputerCraft
turtles/computers/monitors), entities, players and world time — into **SpacetimeDB** as live,
queryable rows. Intended to feed the web viewer at `../McWebViewer`.

It is **no longer purely a mirror**: `request_open_container` and `request_click_slot` make the bot
right-click blocks and move items, so anyone who can call reducers on the database can move its hands. See "Containers, and the moment
spacetime mode stopped being read-only".

The hard part is the **NeoForge 21.1 handshake**: a vanilla protocol client is rejected during
the configuration phase. This client implements the full NeoForge negotiation and the
Fabric-API config-task dance, and **it works** — see "Verified" below.

```
mcspacetime/
├── docker-compose.yaml     # spacetimedb (mcspacetime.pow) + the bot service
├── module/                 # SpacetimeDB module (Rust, spacetimedb 2.10) — tables + reducers
│   └── spacetimedb/src/lib.rs
├── client/                 # the bot: MC protocol client + SpacetimeDB publisher (Rust)
│   ├── src/{main,protocol,neoforge,world,wire,blockstates,publisher}.rs
│   ├── src/{itemstack,computercraft,metadata,anvil}.rs  # decoders (see each file's header)
│   ├── src/module_bindings/ # generated from the module (spacetime generate)
│   └── vendor/             # simdnbt + azalea-chat, minimally patched to build on 2026 Rust
├── data/                   # blockstates.json, neoforge_channels.json, packet_ids.json  (bot input)
├── tools/                  # KubeJS dump scripts + build_channels.py
├── devserver/              # a DEV REPLICA of the live server (offline-mode) for development
└── CLAUDE.md               # gotchas
```

## What works

- **NeoForge 21.1 / MC 1.21.1 (protocol 767) handshake** — login (offline or Microsoft),
  configuration-phase NeoForge negotiation, frozen-registry sync, known-data-maps / extensible-
  enums / feature-flags acks, the Fabric-API configuration-task ack dance (spell_engine,
  bettercombat), and the unconditionally-sent optional play channels (kubejs, accessories, jade,
  railways). The bot **joins the game and stays in**.
- **Live streaming into SpacetimeDB**: block-state id table, registry name tables, chunks &
  16³ sections (vanilla paletted-container layout), block entities (with CC:T ComputerId / label
  / fuel / upgrades lifted out when present), entities, players, world time, and an append-only
  `block_change` feed. A block changed via RCON `setblock` shows up as a row within ~1–2 s.
- **Modded-safe**: the client decodes chunks and registries by hand, so modded block-state ids
  (well beyond vanilla's count), modded registries, and even a mod's byte-array-of-JSON registry
  root (spell_engine) don't break it — unlike azalea/mineflayer, which drop such chunks.

## Prerequisites

- Docker (compose v2) on the shared Traefik network (`~/Projects/PowStation`).
- Rust — the bot builds on **stable** with `RUSTC_BOOTSTRAP=1` (set in `client/.cargo/config.toml`);
  no nightly toolchain needed. The Docker build handles this itself.
- The `spacetime` CLI 2.10 for publishing the module and running queries:
  `curl -sSf https://install.spacetimedb.com | sh`.

## Run it

### 1. SpacetimeDB + publish the module

```bash
cd ~/Projects/mcspacetime
docker compose up -d spacetimedb          # http://mcspacetime.pow (LAN) or :3200 on the host
spacetime server add --url http://localhost:3200 mcspacetime --no-fingerprint
spacetime publish -s mcspacetime --module-path module/spacetimedb -y mcspacetime
spacetime generate --lang rust --out-dir client/src/module_bindings --module-path module/spacetimedb  # only after schema changes
```

### 2. The bot

Against the **dev replica** (offline mode, port 25566 — see `devserver/`):

```bash
cd devserver && docker compose up -d && cd ..
cd client
MC_ADDR=127.0.0.1:25566 MC_USERNAME=spacetime STDB_URI=http://127.0.0.1:3200 \
  DATA_DIR=../data cargo run --release
```

Against the **live server** (`192.168.50.100:25565`, `online-mode=true`): the bot must
authenticate with a real Microsoft account (do **not** change the live server's online-mode).

```bash
MC_ADDR=192.168.50.100:25565 MC_USERNAME=<mc-name> MC_AUTH=microsoft MC_MS_EMAIL=<email> \
  STDB_URI=http://mcspacetime.pow DATA_DIR=../data cargo run --release
# first run prints a device-code URL to open once; the token is cached in data/ms-auth.json
```

Or via compose (builds the bot image; set the env in a shell or `.env`):

```bash
MC_ADDR=192.168.50.100:25565 MC_AUTH=microsoft MC_MS_EMAIL=<email> docker compose up -d bot
docker compose logs -f bot
```

### Environment

| var | default | meaning |
|---|---|---|
| `MC_ADDR` | `127.0.0.1:25565` | Minecraft server host:port |
| `MC_USERNAME` | `spacetime` | player name (offline UUID derived from it) |
| `MC_AUTH` | `offline` | `offline` or `microsoft` |
| `MC_MS_EMAIL` | — | Microsoft account email (device-code login) |
| `MC_VIEW_DISTANCE` | `12` | client view distance requested |
| `STDB_URI` | `http://127.0.0.1:3200` | SpacetimeDB base URL |
| `STDB_MODULE` | `mcspacetime` | module/database name |
| `BLOCKSTATES_JSON` | `<DATA_DIR>/blockstates.json` | block-state id table (from the replica dump) |
| `NEOFORGE_CHANNELS_JSON` | `<DATA_DIR>/neoforge_channels.json` | learned channel list |
| `MCSPACETIME_CHANNELS_READONLY` | — | `1` = never rewrite the channel file (CI/discovery) |
| `MCST_DUMP_PAYLOAD` | — | a mod channel name; append every raw payload on it to `data/payload-dump/<channel>.bin` as `<u32 LE len><bytes>` |
| `WORLD_SAVE` | — | `1` = also download the world as Anvil `.mca` region files |
| `WORLD_SAVE_DIR` | `<DATA_DIR>/worlds` | `<dimension>/region/r.<rx>.<rz>.mca` per dimension |
| `DATA_DIR` | `data` | directory for the above + `ms-auth.json` |

### World download

With `WORLD_SAVE=1` every chunk the bot is sent is also written to disk as a standard Anvil region
file — a real Minecraft backup, not a bespoke dump, so McWebViewer (and any other save reader)
opens it directly:

```bash
WORLD_SAVE=1 MC_ADDR=127.0.0.1:25566 MC_AUTH=offline cargo run --release
# then, from the McWebViewer checkout:
npx tsx src/tools/scan-world.ts ../mcspacetime/data/worlds/minecraft_overworld
```

It runs on its own thread behind a **bounded** channel: if the disk falls behind, chunks are
dropped and counted rather than pushing back into the packet loop, because a stalled packet loop
throttles the chunk stream and the SpacetimeDB mirror with it. Off by default. See
`client/src/anvil.rs`.

## The NeoForge 21.1 handshake — findings

A NeoForge server rejects a vanilla protocol client in the **configuration** phase. There is no
separate "vanilla" connection type — the server defaults every connection to `OTHER` and only
promotes it to `NEOFORGE` when the client answers the modded-network query. The full sequence
this client implements (all NeoForge traffic is ordinary `minecraft:custom_payload`):

1. **Login** — vanilla hello/encryption/compression. On an online-mode server the server sets
   `should_authenticate`, and the client runs the sessionserver `join` with a Microsoft token
   (`client/src/protocol.rs`). Offline servers skip this.
2. Right after `LoginAcknowledged` the server sends `minecraft:unregister`, `minecraft:register`,
   a **`neoforge:register`** query (`ModdedNetworkQueryPayload`, empty map) and a `minecraft:ping(0)`.
3. The client replies **`neoforge:register`** with its channel lists per protocol
   (CONFIGURATION=4, PLAY=1), each channel `= (ResourceLocation id, utf version, optional flow,
   bool optional)`, then pongs. That flips the connection to `NEOFORGE`.
4. **`NetworkComponentNegotiator`** matches the lists. Every **non-optional** server channel must
   be present with the exact **version string** and **flow**, or the server sends
   `neoforge:modded_network_setup_failed` (a map of channel-id → reason component) and disconnects
   with `multiplayer.disconnect.incompatible`. **The failure reasons are how we discover the
   server's channel list**: connect with what we know, read the reasons
   (`missing.server.client` → add it; `version.mismatch` → adopt the server's version;
   `flow.*` → adopt the server's flow), reconnect. Converges in a handful of rounds; the result
   is cached in `data/neoforge_channels.json`. (`client/src/neoforge.rs::learn_from_failure`.)
5. On success the server sends `neoforge:network` (the negotiated setup), then runs the config
   tasks, each of which **blocks until the client replies**:
   - **Frozen registry sync** (`neoforge:frozen_registry_sync_start`, N × `neoforge:frozen_registry`
     = `RegistrySnapshot` of id→name, `neoforge:frozen_registry_sync_completed`). We reply the
     completed payload and **keep the id↔name maps** (block, block_entity_type, entity_type, …).
   - Vanilla `select_known_packs` (reply empty → the server sends all registry data),
     `registry_data` × N, `update_tags`.
   - `neoforge:known_registry_data_maps` → reply empty; `neoforge:extensible_enum_data` → ack;
     `neoforge:feature_flags` → ack.
   - **Fabric-API configuration tasks** (forgified-fabric-api mods, e.g. `spell_engine`,
     `bettercombat`). The kick names the *task* id (`spell_engine:config`) but `canSend` checks
     the *payload* id (`spell_engine:config_sync`). We register the candidate payload ids ad-hoc
     via `minecraft:register` (so NeoForge's `hasChannel` passes), and when the task sends its
     `*_sync` payload we reply an **Ack** on `<ns>:ack` carrying the task id (payload id minus the
     `_sync` suffix) so the server can `completeTask`.
6. `finish_configuration` → PLAY.
7. **PLAY join catch**: several mods send an *optional* play channel **unconditionally** on join
   (`kubejs:sync_server_data`, `kubejs:sync_stages`, `accessories:main`, `jade:server_ping_v1`,
   `railways:s2c`). If such a channel is not in the negotiated PLAY setup, NeoForge throws
   server-side (`Payload X may not be sent to the client!`) and kicks with a *vanilla-looking*
   `multiplayer.disconnect.invalid_player_data`. Because these are optional, they never show up in
   a negotiation probe; they are listed explicitly in `data/neoforge_channels.json` (discovered
   from the dev-replica server log — `tools/`).

Things that are **not** checked server-side: the client `minecraft:brand` (NeoForge never
inspects it), and there is no vanilla config-phase brand requirement.

### Two things a protocol client cannot derive from the wire

- **Block-state ids → block + properties.** NeoForge assigns global state ids by flattening
  `(blocks in registry order) × (each block's possible states)`; the second factor is compiled
  Java. We dump it once on a server running the same jars (`tools/kubejs_dump_registries.js` →
  `data/blockstates.json`, 344 003 states here) and, at connect time, re-flatten it in the live
  block-registry order from the frozen-registry sync (so a different registry order is handled;
  only a block absent from the dump is unrecoverable). This is exactly the blocker
  `../McWebViewer/ARCHITECTURE.md §"the blocker is NeoForge"` calls fatal for mineflayer — solved
  here by carrying the dump.
- **Monitor text / a program's screen** — not on the wire at all (as McWebViewer notes); this
  client does not attempt it. McWebViewer's `screen.json` feed remains the source for that.

## Data model (SpacetimeDB module)

Positions are packed like vanilla so keys are plain `i64` and the viewer can compute them:
`chunk` key = `(cx<<32)|cz`; `chunk_section` key packs `(cx,cz,sy)`; `block_entity`/`block_change`
use `BlockPos.asLong()`.

| table | key | contents |
|---|---|---|
| `block_state` | `id: u32` | global state id → `name` + `properties` (`"facing=north,…"`) |
| `registry_entry` | `"<reg>:<id>"` | id→name for `block` / `block_entity_type` / `entity_type` / `item` / `biome` / `data_component_type` / `enchantment` |
| `chunk` | `key` | dimension, cx, cz, min_section, section_count |
| `chunk_section` | `key` | `block_bits`/`block_palette`/`block_data` (+ biomes) in the Anvil paletted layout, `non_air_count` |
| `block_entity` | `key` | pos, `type_name`, `block_state_id`, raw network `nbt` + `nbt_json`, and CC:T `computer_id`/`label`/`fuel`/`on`/`left_upgrade`/`right_upgrade` when present |
| `monitor` | `key` (origin `BlockPos.asLong()`) | one ComputerCraft monitor panel's screen: `facing`, panel size in blocks, terminal size in characters, and `lines`/`fg`/`bg` + `palette` |
| `container` | `window_id` | an OPEN container — **session state**, deleted on close. Window 0 is the bot's own inventory |
| `bot_command` | `id` (auto) | requests for the bot to act, and their outcome |
| `entity` | `id: i32` | uuid, type, pos/rot/vel, spawn `data`, custom name, `appearance` (entity-metadata scalars as JSON), `equipment` (worn/held stacks as JSON) |
| `player` | `uuid` | name, entity_id, gamemode, latency, pos/rot, online |
| `world_time` | `dimension` | game_time, day_time |
| `block_change` | `seq` (auto) | append-only single-block deltas (old/new state id), pruned to ~20k |
| `bot_status` | `0` | connection state/detail, dimension, pos, chunks_loaded, packets_seen |

Reducers (the bot is the only writer): `upsert_chunk`, `update_sections`, `unload_chunk`,
`clear_world`, `upsert_block_entities`/`remove_block_entities`, `upsert_entities`/`move_entities`/
`remove_entities`/`set_entity_name`, `upsert_players`/`remove_players`, `set_block_states`,
`set_registry_entries`, `set_time`, `set_bot_status`, `update_monitors`/`remove_monitors`,
`set_entity_equipment`, `upsert_container`/`set_container_slot`/`close_container`/`close_all_containers`,
and the three that make the bot act: `request_open_container`/`request_close_container`/`request_click_slot`. Sections are re-published whole on any
change (the module never unpacks bits); entity moves are coalesced (last position wins) in the
publisher so a busy world stays well under SpacetimeDB's 32 MiB message limit.

## How McWebViewer should consume it

McWebViewer today reads Anvil region files. This module gives it the same information **live**:

- Terrain: subscribe to `chunk` + `chunk_section`. Each section's
  `block_bits`/`block_palette`/`block_data` are the **identical layout** to a region file's
  `block_states` (bits-per-entry, palette of global ids, `64/bits` entries per `long`, no
  straddling) — the mesher can be reused unchanged. Resolve palette ids through `block_state`
  (id → name + properties), exactly like reading a chunk-section palette. Biomes likewise.
- Block entities: `block_entity` gives position, `type_name`, and the network NBT (`nbt` raw +
  `nbt_json`); turtles carry `computer_id`/`label`/`fuel`/upgrades directly — the same fields the
  viewer already detects structurally.
- Entities / players / time / live edits: `entity`, `player`, `world_time`, and the append-only
  `block_change` feed for reacting to single blocks without diffing sections.
- **Monitor screens**: `monitor`, one row per panel, keyed by the panel's top-left block. See
  “Monitor screens” below — it is `LiveMonitor` in `McWebViewer/src/app/live.ts` almost field for
  field, with per-character colour instead of one colour per screen.
- **Entity appearance**: `entity.appearance` is the entity's SynchedEntityData scalars as JSON,
  `{"<index>": <value>}` — the sheep colour byte, the villager profession triple, the wolf variant
  id. It is deliberately **uninterpreted**; see below.

Subscribe with the SpacetimeDB TypeScript SDK against `http://mcspacetime.pow` (WebSocket). No
`save-all flush`, no region re-reads, no RCON bridge for the block view.

### Containers, and the moment spacetime mode stopped being read-only

**The proxy is a real connected client**, so when its own player right-clicks a chest the server sends
it `open_screen` and `container_set_content` like any other client. No server mod and no embedded
channel is needed for one player's containers — only handling the packets already addressed to us.

`container` holds what is open, keyed by the server's window id. It is **session state, not world
state**: it belongs to a player, and `container_close`, a disconnect and a dimension change all DELETE
the row. If there is no row, nothing is open. A stale open container is worse than none — it shows a
chest's contents from ten minutes ago as if they were live.

| column | |
|---|---|
| `window_id` | the server's id. **0 is the bot's own inventory**, which the server sends unprompted |
| `menu_type` | `minecraft:generic_9x3`, … from the `minecraft:menu` registry; empty for window 0 |
| `title` | the screen title as text. A vanilla chest is the raw translation key `container.chest`; a named one gives its name |
| `opened_from_x/y/z` | the block the bot clicked, when the proxy opened it itself |
| `slots` | one entry per slot in the menu's own order: a decoded stack as JSON, or `""` for empty |
| `carried` | the stack on the cursor, `""` if none |
| `state_id` | the server's container state id — carried for a future `ContainerClick`; nothing reads it yet |
| `undecoded_component` | names the component that stopped a slot's decode, if any. See below |

The slot list covers the **whole menu**, container plus the player's own inventory — a single chest is
63 slots (27 + 36), not 27.

#### Clicking a slot

`request_click_slot(window_id, slot, button, mode)`, same pending/outcome shape as the others.
**This is the only packet this project sends that mutates the world.**

`mode` 0 is *pickup* (`button` 0 = left, whole stack; 1 = right, half) and `mode` 1 is *quick_move*
(shift-click, moves the stack between the container and the player's inventory). **The other five
`ClickType`s are not sent** — swap, clone, throw, quick_craft, pickup_all — and neither are vanilla's
special slot indices −1 and −999 (−999 with pickup *throws the carried stack on the floor*). Anything
else is `rejected`, not forwarded.

Everything is validated before sending, because **every one of these failures is silent on the server
side**: a click for a window it does not have open, or a slot index it considers invalid, is logged at
debug and dropped with no reply. An unvalidated click is indistinguishable from a working one that did
nothing.

**The proxy predicts nothing.** A click may carry the client's *predicted* resulting slots; this one
sends an empty `changedSlots` map, so the server compares reality against "unchanged" and corrects
every slot that actually moved. `container.slots` is written from server packets only — there is no
second copy of vanilla's `doClick` to disagree with, and nothing to reconcile.

The one place that needs care is the cursor, and it took two clicks to find: the packet also claims a
`carriedItem`, we claim *empty*, and the server only corrects where reality differs from the claim — so
when a click genuinely leaves the cursor empty, **no correction arrives at all**. The session therefore
adopts the claim on send, which makes that silence mean "empty" rather than "unchanged". Without it,
pick a stack up and put it back and `carried` keeps the stack forever.

A stale `state_id` is not rejected by the server: it applies the click and answers with a full
`container_set_content` instead of targeted corrections, so the mirror resyncs either way.

Verified on the dev replica through pickup, right-click-half, place-back and quick_move in both
directions, each reversed to the starting state; and once on the live server against a chest placed for
the purpose at 65,70,36 (previously air) holding one cobblestone — picked up, put back, the chest
emptied and the block restored to air, with nothing of HiveMind's storage touched.

#### Asking the bot to open one

`request_open_container(x, y, z, face)` inserts a row in `bot_command`; the bot subscribes to the
pending ones, runs each exactly once, and writes back a terminal `status` — `done`, `failed`, or
`rejected` (it refused before sending anything). `face` may be empty, meaning "pick one". There is
also `request_close_container()`.

**This is the first serverbound gameplay packet this project sends, and it changes what the module
is: anyone who can call reducers on this database can move the bot's hands.** Nothing else about the
mirror is writable, but that much is. The checks that exist are worth knowing:

- **Reach is validated here, with vanilla's own rule**, because the server does not report a refusal:
  `canInteractWithBlock(pos, 1.0)` is `blockInteractionRange()` (4.5) plus 1.0 of padding, measured
  from the bot's *eyes* to the nearest point of the block. A request outside 5.5 is `rejected` with
  the distance rather than sent and silently dropped.
- **One at a time.** A second open while one is in flight, or while a container is already open, is
  `rejected`; the window-id bookkeeping would otherwise be ambiguous.
- **A silent failure still ends.** The server drops an interaction it does not like without telling
  the client anything, so "no `open_screen` within 1500 ms" is the only signal available and becomes
  `failed` with that explanation.
- **Requests made while the bot was down are abandoned**, not run on reconnect — they were aimed at a
  block the bot may no longer be near.
- The bot can **walk** (`request_walk_to`, 2026-09-12) but only in a straight line, so it can open
  what it can walk to — not merely what spawned within 5.5 blocks of it.
- A `kill -9` leaves the rows behind — nothing in-process can clear them. A server-side disconnect
  does, and so does the next connect. Check `bot_status` if liveness matters.

#### `entity.equipment`

`set_equipment` is a separate packet from entity metadata and carries real item stacks: every armoured
mob and everything anyone is holding. `entity.equipment` is
`{"mainhand"|"offhand"|"feet"|"legs"|"chest"|"head"|"body": {stack} | null}`, merged in the client
because the packet is a delta. A slot present with `null` means the server said it is empty; a slot
absent means nothing has been said about it. **`body` is 1.21's addition** (horse and wolf armour), so
a 1.20 slot table would mis-name it.

### Packet ids come from the jar, not from a table

`data/packet_ids.json` is the complete PLAY id table — 124 clientbound, 58 serverbound — generated by
`tools/dump_packet_ids.py` from `GameProtocols`' registration order in a deobfuscated server jar. It
agreed with all 13 ids this project had previously established by hand, **including the `light_update`
= 0x2a that a protocol table got wrong**. Use it instead of hunting empirically; regenerate it for a
new game version.

### Item stacks: `{ id, count }` and the components that can be read

Since 1.20.5 a wire `ItemStack` is a count, an item id, and a **data-component patch**, and each
component's bytes are written by that component's own codec with **no length prefix**. There is
nothing to skip: a component the proxy does not know ends the decode *and* makes everything after the
stack in that packet unreadable. `client/src/itemstack.rs` therefore decodes what it knows and
otherwise **names the component and stops** — a decoder that guessed a length would silently corrupt
the rest of the packet, which is worse than refusing.

The first consumer is `entity.appearance`: a dropped item or item frame now carries its stack at its
metadata index, e.g. `{"8":{"id":"minecraft:cobblestone","count":42}}`, and an enchanted named tool in
a shulker box comes out whole:

```json
{"8":{"id":"minecraft:shulker_box","count":1,"components":{"minecraft:container":[
  {"id":"minecraft:cobblestone","count":32}, null,
  {"id":"minecraft:diamond_sword","count":1,"components":{
    "minecraft:custom_name":"Sting","minecraft:enchantments":{"minecraft:sharpness":3}}}]}}}
```

An **empty** stack is `null`, not an absent field — an item frame with nothing in it is a real state.
A `removed` array lists components the stack strips from the item's defaults (names only; a removal
carries no payload, which is why those are readable even when an added component is not).

**Readable today** — every one established by disassembling that component's `StreamCodec`, never
guessed: `damage`, `max_damage`, `max_stack_size`, `repair_cost`, `ominous_bottle_amplifier`,
`map_id`, `custom_model_data`, `rarity`, `unbreakable`, `enchantment_glint_override`,
`hide_tooltip`, `hide_additional_tooltip`, `creative_slot_lock`, `fire_resistant`, `custom_name`,
`item_name`, `lore`, `dyed_color`, `map_color`, `note_block_sound`, `entity_data`,
`bucket_entity_data`, `block_entity_data`, `enchantments`, `stored_enchantments`, `container`,
`charged_projectiles`, `bundle_contents`, `written_book_content`, `writable_book_content`,
`potion_contents`.

**Never readable, and not a gap:** a mod's own component. Its codec is compiled Java in that mod's
jar. `minecraft:data_component_type` has **327 entries on this server** against vanilla's 57, so most
registered types are mod types — which is also why dispatch is by **name**, resolved through the
registry, and never by numeric id.

**Never on the wire at all:** seven vanilla components are not network-synchronised, so they can never
appear here — `custom_data`, `intangible_projectile`, `map_decorations`, `debug_stick_state`,
`recipes`, `lock`, `container_loot`. `custom_data` is the old NBT `tag`: a mod keeping state there is
invisible to every client, this one included.

Two colour/order traps worth naming, because both decode into plausible nonsense rather than failing:
`Enchantment`'s stream codec is `holderRegistry`, a **plain** varint id — not the `id+1 / 0-then-
inline` form `ByteBufCodecs.holder` uses — and a *unit* component (`hide_tooltip` and friends)
occupies **zero** bytes, so reading one byte for it shifts every component after it by one.

### Monitor screens

`monitor` carries what a ComputerCraft monitor panel is displaying, decoded from the mod's own
`computercraft:monitor_client` payload in `client/src/computercraft.rs`. One row per panel.

| column | |
|---|---|
| `key` | `BlockPos.asLong()` of the **origin** block = the panel's top-left as seen facing the screen |
| `x`,`y`,`z`, `chunk_key` | that block |
| `facing` | the origin block's `facing` property (`"north"`/`"south"`/`"east"`/`"west"`) |
| `block_width`, `block_height` | panel size in **blocks**; `0` until the monitor's block entity has been seen |
| `term_width`, `term_height` | terminal size in **characters** |
| `colour` | advanced (colour) monitor |
| `cursor_x`, `cursor_y`, `cursor_blink`, `cursor_fg`, `cursor_bg` | the terminal cursor |
| `has_screen` | `false` when the mod sent an empty terminal — the monitor exists, nothing drives it |
| `lines` | `term_height` strings of `term_width` characters |
| `fg`, `bg` | same shape; each character is a hex digit indexing `palette` |
| `palette` | 16 `#rrggbb` |

That is `LiveMonitor` (`McWebViewer/src/app/live.ts`) field for field, except that `bg`/`fg` there
are one colour for a whole screen and CC colours **per character** — so the consumer flattens (e.g.
take the most common `bg` digit) or, better, renders the grid.

**The things that are not obvious, all of them measured:**

- **Every payload is a full screen, never a delta.** No merging is needed, but plenty of
  suppression is: on the dev replica **70% of payloads were byte-identical to the screen already
  published** (114 of 159 in three minutes) and are dropped in the client without touching the
  database. A monitor's row is additionally rate-limited to one write per 500 ms.
- **The palette is sent in the OPPOSITE order to the colour digits**, and nothing on the wire says
  so: the 16 entries arrive in `Colour.values()` order (black first) while a cell's digit is CC's
  Lua colour index (`colours.white` is `0`). The mod's own renderer looks a cell up as
  `palette[15 - digit]`. **The `monitor` table's `palette` is already reversed**, so
  `palette[parseInt(fg[row][col], 16)]` is simply right. A default screen is the trap that makes
  this easy to get wrong and hard to notice: `fg` `0` on `bg` `f`, which is white-on-black and reads
  as black-on-white if you index straight.
- **Only the origin block is addressed**, so a 3x4 panel is one row, not twelve. `block_width`/
  `block_height` are **not in the payload** — it carries characters only, and the two are related by
  the monitor's text scale, which is not on the wire at all. They are lifted from the monitor block
  entity's update tag (`XIndex`/`YIndex`/`Width`/`Height`) instead.
- **Text is bytes.** CC's terminal stores `char & 0xff` and its font gives all 256 values a glyph
  (0x20–0x7e ASCII, the rest CC's own drawing/teletext characters). Each byte becomes the identical
  Unicode code point, so `codePointAt(i) & 0xff` recovers the CC glyph index losslessly; NUL, which
  fills a freshly resized terminal, becomes a space.
- Rows are big: a 3x5-block advanced monitor is 57x67 characters = **11.5 KB** of text + colour
  grids. Ten monitors on the live base totalled 53 KB.

`MCST_DUMP_PAYLOAD=computercraft:monitor_client` captures the raw payloads;
`client/testdata/monitor_client_71_67_33.bin` is one of them, kept as a decoder regression test.

### `entity.appearance`: indices, not meanings

The proxy walks entity metadata generically (`client/src/metadata.rs`) and publishes what it finds
under its own index. It does **not** decode "index 17 is sheep colour", and neither should anything
that copies a vanilla index table: **on this modpack the indices are shifted**. Two mods add fields
to `LivingEntity`, so every subclass index moves by two — sheep wool colour is index **19** here,
not the 17 every protocol reference lists, and `Mob`'s flags byte is 17 rather than 15.

So a consumer must derive the mapping for the pack it is looking at rather than look it up.
Measured on the dev replica by summoning mobs with known NBT and reading the column back:

| entity | index | type | value seen | from |
|---|---|---|---|---|
| sheep | 19 | Byte | `15`, `4`, `30` | `Color:15b`, `Color:4b`, `Sheared:1b,Color:14b` (0x10 = sheared, low nibble = dye) |
| villager | 20 | VillagerData | `[2,5,3]` | `{type:plains, profession:farmer, level:3}` |
| wolf | 22 / 24 | VarInt | `11` / `9` | `CollarColor:11b` / `variant:woods` |
| horse | 20 | VarInt | `2` | `Variant:2` |
| cat, parrot | 21 | VarInt | `3` | `variant:siamese`, `Variant:3` |
| creeper | 19 | Boolean | `true` | `powered:1b` |
| fox, frog, axolotl | 19 | VarInt | `1`, `2`, `3` | `Type:snow`, `variant:cold`, `Variant:3` |
| mooshroom | 19 | String | `"brown"` | `Type:brown` |
| panda | 22, 23 | Byte | `4`, `4` | `MainGene/HiddenGene: brown` |
| tropical fish | 19 | VarInt | `65536` | `Variant:65536` |
| any mob | 17 | Byte | `1` | `Mob` flags; `0x01` = NoAI |

Two consequences worth knowing before writing the rules:

- **An absent index means "default", not "unknown".** The server only sends the values that differ
  from the class defaults when an entity comes into view, so a *white* sheep has no index 19 at all.
  Fall back to the vanilla default, never to "no appearance".
- **Item-shaped entities now carry their stack** (`client/src/itemstack.rs`; see "Item stacks"
  above). The walk used to stop dead at the `ItemStack` serializer, so dropped items and item frames
  — whose appearance *is* the stack — came out empty. It still stops if a stack carries a data
  component with no codec, but it now says which component by name.

## Verified (dev replica, 2026-09-08)

Commands run and their results:

```
# server (dev replica of the live pack, NeoForge 21.1.248, offline-mode, seed 7):
docker compose -f devserver/docker-compose.yaml up -d          # mcspacetime-devserver on :25566

# module published, bot run:
spacetime publish -s mcspacetime --module-path module/spacetimedb -y mcspacetime
MC_ADDR=127.0.0.1:25566 ... cargo run --release

# server log shows the join (NOT a kick):
docker compose -f devserver/docker-compose.yaml logs mcdev | grep spacetime
  [..] PlayerList: spacetime[/172.17.0.1] logged in with entity id 3362 at (64.5, 62.0, 36.5)
  bot log: "joined the game (server view distance 6)", "bot placed at 64.5 62.0 36.5"

# rows present in SpacetimeDB:
spacetime sql mcspacetime "SELECT COUNT(*) FROM <t>"
  block_state 344003 | registry_entry 22172 | chunk 10 | chunk_section 240
  block_entity 136 | entity 7 | player 1 | world_time 1 | block_change 92 | bot_status 1

# live block change via RCON shows up as a row within ~1–2 s, with correct state-id resolution:
docker compose -f devserver/docker-compose.yaml exec mcdev rcon-cli "setblock 66 63 38 minecraft:gold_block"
docker compose -f devserver/docker-compose.yaml exec mcdev rcon-cli "setblock 66 64 38 minecraft:diamond_block"
spacetime sql mcspacetime "SELECT x,y,z,new_state_id FROM block_change WHERE x=66"
  66 63 38 -> 2091   66 64 38 -> 4276
spacetime sql mcspacetime "SELECT id,name FROM block_state WHERE id=2091 OR id=4276"
  2091 minecraft:gold_block | 4276 minecraft:diamond_block   # ids resolve correctly
```

**Against the live server** the only difference is authentication (online-mode): supply
`MC_AUTH=microsoft`. The protocol/handshake path is identical and shares the same
`data/neoforge_channels.json` (same jar set → same channels). The live server was **not** touched
(no config change, no restart, no mods added).

## No server-side allowance was needed

The handshake is passed entirely client-side. If a future pack update adds a mod whose Fabric-API
config task uses a non-`_sync`/`:ack` naming, or a new unconditional play channel appears, the bot
self-heals for the negotiated cases and the play-channel list is refreshed by replaying the
discovery step in `CLAUDE.md`. No mod or server property is required.

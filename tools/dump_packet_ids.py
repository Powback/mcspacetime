#!/usr/bin/env python3
"""Dump the authoritative PLAY packet-id table out of a deobfuscated Minecraft server jar.

WHY THIS EXISTS: this project found `light_update` at 0x2a by placing glowstone and watching the
wire, after a protocol table said 0x28. Packet ids are not worth guessing and not worth looking up --
they are the *registration order* in `GameProtocols`, which is in the jar. Extracting them took one
`javap` and agreed with all 13 ids this project had established by hand, including that 0x2a.

Prerequisites (see CLAUDE.md "Reading vanilla codecs"): a DEOBFUSCATED server jar, produced with
NeoForge's own AutoRenamingTool against Mojang's mappings.

    docker run --rm -v "$HOME/mcjar:/w" -w /w maven:3.9-eclipse-temurin-21 \
      java -jar in/art.jar --input in/server-slim.jar --output deobf.jar \
      --map in/mappings.txt --reverse
    mkdir -p cls && (cd cls && unzip -oq ../deobf.jar 'net/minecraft/network/protocol/game/GameProtocols*')
    docker run --rm -v "$HOME/mcjar/cls:/w" -w /w maven:3.9-eclipse-temurin-21 \
      javap -p -c net.minecraft.network.protocol.game.GameProtocols > gp.txt
    python3 tools/dump_packet_ids.py gp.txt > data/packet_ids.json

THE TRAP: the ids are the order of `addPacket` calls, and GAME packets are interleaved with COMMON,
COOKIE and PING packets (`CommonPacketTypes`, `CookiePacketTypes`, `PingPacketTypes`). Matching only
`GamePacketTypes` yields a table that is right for the first 14 entries and then drifts -- which is
worse than no table at all, because the first few agreeing makes it look correct. Hence the
deliberately broad `\\w*PacketTypes` match below.
"""
import json
import re
import sys


def parse(disassembly: str) -> dict[str, list[str]]:
    out: dict[int, list[str]] = {}
    cur = None
    for line in disassembly.split("\n"):
        m = re.match(r"\s+private static void lambda\$static\$(\d+)\(", line)
        if m:
            cur = int(m.group(1))
            out[cur] = []
            continue
        if cur is None:
            continue
        # any *PacketTypes constant, in any protocol sub-package -- see THE TRAP above
        f = re.search(r"// Field net/minecraft/network/protocol/\w+/\w*PacketTypes\.(\w+):", line)
        if f:
            out[cur].append(f.group(1))
    result: dict[str, list[str]] = {}
    for names in out.values():
        if not names:
            continue
        kind = "clientbound" if names[0].startswith("CLIENTBOUND") else "serverbound"
        prefix = len(kind) + 1
        result[kind] = [n[prefix:].lower() for n in names]
    return result


if __name__ == "__main__":
    src = open(sys.argv[1]).read() if len(sys.argv) > 1 else sys.stdin.read()
    table = parse(src)
    print(json.dumps({
        "_source": "GameProtocols registration order, from a deobfuscated 1.21.1 server jar",
        "_note": "index is the packet id; see tools/dump_packet_ids.py",
        **{k: {f"0x{i:02x}": n for i, n in enumerate(v)} for k, v in table.items()},
    }, indent=1))

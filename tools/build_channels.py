#!/usr/bin/env python3
"""Seed client/data/neoforge_channels.json from a full server channel dump.

The bot LEARNS the required channels by probing (reconnect + read the negotiation-failure
reasons), and discovers Fabric-API config-task channels and unconditionally-sent optional play
channels at runtime. This tool is only needed if you want to pre-seed the file so a fresh bot
joins on the first attempt instead of after a few probe reconnects.

Usage:
    python3 tools/build_channels.py devserver/data/kubejs/exported/neoforge_channels_full.json \
            data/neoforge_channels.json
"""
import json, sys

full = json.load(open(sys.argv[1]))
out = {"configuration": {}, "play": {}, "verified": False, "server_version": "", "configuration_complete": False, "adhoc": []}
for proto in ("configuration", "play"):
    for cid, c in full.get(proto, {}).items():
        out[proto][cid] = {"version": c["version"],
                           "flow": {"CLIENTBOUND": "Clientbound", "SERVERBOUND": "Serverbound"}.get(c.get("flow"), None),
                           "optional": c["optional"]}
json.dump(out, open(sys.argv[2], "w"), indent=1)
print(f"wrote {sys.argv[2]}: {len(out['configuration'])} config + {len(out['play'])} play channels")

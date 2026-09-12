// KubeJS SERVER script: dumps the tables a protocol client cannot derive from the wire.
//
//   kubejs/exported/blockstates.json   global block-state id -> "ns:block[prop=val,...]"
//   kubejs/exported/registries.json    id -> name for block / block_entity_type / entity_type
//
// Only ever installed on the mcspacetime DEV REPLICA (devserver/data/kubejs/server_scripts/).
// NeoForge assigns block-state ids as (blocks in registry-id order) x (getPossibleStates()),
// so the same jar set gives the same table; the live server's registry snapshot
// (neoforge:frozen_registry) is cross-checked against registries.json at connect time.
//
// Reload without restart:  /kubejs reload server_scripts
const $Block = Java.loadClass('net.minecraft.world.level.block.Block')
const $Reg = Java.loadClass('net.minecraft.core.registries.BuiltInRegistries')

function dump() {
  const states = []
  const reg = $Block.BLOCK_STATE_REGISTRY
  const n = reg.size()
  for (var i = 0; i < n; i++) {
    // "Block{minecraft:oak_stairs}[facing=north,half=bottom,...]" -> "minecraft:oak_stairs[...]"
    // (Rhino: no const/let inside loops — "redeclaration of var")
    var str = String(reg.byId(i).toString())
    str = str.replace(/^Block\{([^}]*)\}/, '$1')
    states.push(str)
  }
  function regDump(r) {
    // Registry#getKey / #getId take a T and KubeJS type-wraps that argument into an ID, which
    // fails for e.g. BlockEntityType. The holder id-map has no such parameters.
    var out = []
    var idMap = r.asHolderIdMap()
    var size = idMap.size()
    for (var j = 0; j < size; j++) {
      var h = idMap.byId(j)
      out.push([j, String(h.unwrapKey().get().location().toString())])
    }
    return out
  }
  const regs = {
    block: regDump($Reg.BLOCK),
    block_entity_type: regDump($Reg.BLOCK_ENTITY_TYPE),
    entity_type: regDump($Reg.ENTITY_TYPE),
  }
  JsonIO.write('kubejs/exported/blockstates.json', { count: n, states: states })
  JsonIO.write('kubejs/exported/registries.json', regs)
  console.info(`mcspacetime dump: ${n} block states, ${regs.block.length} blocks, ${regs.block_entity_type.length} block entity types, ${regs.entity_type.length} entity types`)
}

// Runs at script (re)load time: BuiltInRegistries are frozen long before server scripts load,
// and this way a plain `/reload` re-dumps (ServerEvents.loaded only fires on boot).
dump()

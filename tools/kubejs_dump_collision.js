// KubeJS SERVER script: dumps per-block-state COLLISION emptiness, which the wire never carries.
//
//   kubejs/exported/collision.json   { count, empty: [[startId, endId], ...] }
//
// WHY. The walker decides what it may move through from the block's NAME (`walk.rs::passable`), and a
// name is a guess. In a modded pack most unrecognised names are real blocks, so unknown resolves to
// SOLID -- safe, but it means any mod's decoration can block a route the bot could actually walk. The
// collision shape is the authoritative answer and it is compiled Java, exactly like `getPossibleStates`,
// so carrying a dump is the same fix `blockstates.json` already is.
//
// Only ever installed on the mcspacetime DEV REPLICA. Collision shapes come from the jars, so the same
// jar set gives the same table -- which is why a dump taken from the disposable replica is valid for the
// live server.
//
// Reload without restart:  /reload
//
// EVERYTHING IS INSIDE AN IIFE ON PURPOSE. KubeJS server scripts share ONE Rhino scope, so a top-level
// `const $Block` here collides with the identical declaration in mcspacetime_dump.js and Rhino aborts the
// load with "TypeError: redeclaration of const $Block" -- which takes the OTHER script down with it, so
// the existing blockstates dump silently stops being produced too. Measured: "Loaded 1/3 KubeJS server
// scripts ... with 2 errors".
;(function () {
  const $Block = Java.loadClass('net.minecraft.world.level.block.Block')
  const $EmptyBlockGetter = Java.loadClass('net.minecraft.world.level.EmptyBlockGetter')  // NOT net.minecraft.world.* -- resolved from the official mappings.txt, which is the only authority here since the jar is obfuscated
  const $BlockPos = Java.loadClass('net.minecraft.core.BlockPos')

  const reg = $Block.BLOCK_STATE_REGISTRY
  const n = reg.size()
  const level = $EmptyBlockGetter.INSTANCE
  const pos = $BlockPos.ZERO

  // Ranges rather than one entry per state: the states of a block are contiguous, so a passable block
  // collapses to a single pair and the file stays small instead of 344k booleans.
  const empty = []
  var runStart = -1
  var failures = 0
  for (var i = 0; i < n; i++) {
    var isEmpty = false
    try {
      // getCollisionShape(BlockGetter, BlockPos). EmptyBlockGetter is what vanilla itself passes when
      // asking a state about its shape with no world around it.
      isEmpty = reg.byId(i).getCollisionShape(level, pos).isEmpty()
    } catch (e) {
      // A block that insists on a real world (queries a neighbour, or its block entity) is left SOLID.
      // The name heuristic still applies, and guessing "passable" here is the one direction that
      // desyncs the bot from the server.
      failures++
      isEmpty = false
    }
    if (isEmpty) {
      if (runStart < 0) runStart = i
    } else if (runStart >= 0) {
      empty.push([runStart, i - 1])
      runStart = -1
    }
  }
  if (runStart >= 0) empty.push([runStart, n - 1])

  var total = 0
  for (var k = 0; k < empty.length; k++) total += empty[k][1] - empty[k][0] + 1
  JsonIO.write('kubejs/exported/collision.json', { count: n, empty: empty })
  console.info('mcspacetime collision dump: ' + n + ' states, ' + total + ' with an empty collision shape in ' + empty.length + ' ranges, ' + failures + ' could not be asked')
})()

// DEV REPLICA ONLY: dump NeoForge's full server-side channel registrations via reflection.
// KubeJS's class filter blocks java.lang.Class/java.lang.reflect, so we reach a Class instance
// through an existing object's getClass(), then use its reflection methods (allowed on the
// instance). Writes kubejs/exported/neoforge_channels_full.json.
try {
  // Get a real java.lang.Class instance via a plain Java object's getClass() (loading the
  // java.lang.Class TYPE by name is filtered, but instance methods on a Class object are fine),
  // then Class.forName the target class and reflect its private static field.
  var probe = new (Java.loadClass('java.util.LinkedHashMap'))()
  var classClass = probe.getClass().getClass() // java.lang.Class
  var forName = classClass.getMethod('forName', probe.getClass().getClass())
  var nrClass = forName.invoke(null, 'net.neoforged.neoforge.network.registration.NetworkRegistry')
  var f = nrClass.getDeclaredField('PAYLOAD_REGISTRATIONS')
  f.setAccessible(true)
  var regs = f.get(null)
  var out = {}
  var it = regs.entrySet().iterator()
  while (it.hasNext()) {
    var e = it.next()
    var proto = String(e.getKey().toString()).toLowerCase()
    var m = {}
    var it2 = e.getValue().entrySet().iterator()
    while (it2.hasNext()) {
      var e2 = it2.next()
      var id = String(e2.getKey().toString())
      var reg = e2.getValue()
      m[id] = { version: String(reg.version()), flow: reg.flow().isPresent() ? String(reg.flow().get().toString()) : null, optional: reg.optional() }
    }
    out[proto] = m
  }
  JsonIO.write('kubejs/exported/neoforge_channels_full.json', out)
  console.info('mcspacetime channels dump: ' + Object.keys(out.configuration||{}).length + ' configuration + ' + Object.keys(out.play||{}).length + ' play channels')
} catch (err) { console.error('mcspacetime channels dump FAILED: ' + err) }

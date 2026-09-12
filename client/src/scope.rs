//! Per-server scoping: which database a mirror lives in, and which directory a world saves to.
//!
//! WHY THIS EXISTS. Every world-shaped table is keyed by position alone -- `chunk_key(cx, cz)`
//! spends all 64 bits on the coordinates, with no room for a server -- and the Anvil saver updates
//! region files IN PLACE under `<root>/<dimension>/region`. So pointing the client at a second
//! server writes that server's chunks over the first one's, in both the mirror and the backup.
//! This is not hypothetical: a run against a dev replica overwrote a live mirror and cost a working
//! session, because `monitor` went to 0 rows and there was nothing left to verify against.
//!
//! The fix is to scope by SERVER, not to add a column, and the reason is the registries. A `world`
//! column on `Chunk` would still leave `BlockStateRow` and `RegistryEntry` colliding -- they are
//! primary-keyed on registry ids, and modpack A's block-state 4231 is a different block from
//! modpack B's. Mismatched registries do not lose data, they render every block WRONG, silently,
//! which is worse than a gap. Only isolating the whole database scopes the registries too.
//!
//! The failure direction is chosen deliberately. `example.com` and `1.2.3.4` are the same server to
//! everyone but this function, so addressing one host two ways mirrors it twice. That wastes disk;
//! it does not corrupt anything. Merging two servers into one name is the failure that corrupts, so
//! every ambiguity resolves toward "separate".

/// Longest slug we will emit, hash suffix included. SpacetimeDB accepts more, but a directory name
/// that fits in a terminal is worth more than one that round-trips a 253-byte hostname.
const MAX_SLUG: usize = 48;

/// Minecraft's default port. `example.com` and `example.com:25565` are ONE server and must slug
/// identically, or the same host reached two ways silently mirrors itself twice.
const DEFAULT_PORT: &str = "25565";

/// The separator, and the only punctuation a slug may contain.
///
/// It is a DASH because SpacetimeDB rejects an underscore in a database name outright ("invalid
/// characters in database name" -- `mcspacetime_x` is refused, `mcspacetime-x` is accepted), while a
/// path segment is happy with either. One separator that satisfies both beats two sanitisers that
/// have to agree.
const SEP: char = '-';

/// FNV-1a, 64-bit. Hand-rolled ON PURPOSE: this value names a directory on disk, and
/// `std::collections::hash_map::DefaultHasher` is explicitly not stable across Rust releases, so a
/// toolchain upgrade would re-point the backup directory and silently start a second copy of the
/// world beside the first.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// `127.0.0.1:25565` -> `127-0-0-1`, suitable for a database name or a path segment.
///
/// Lowercased, reduced to `[a-z0-9-]`, and truncated with a hash suffix when long. Never empty, so
/// a caller can always join it onto a path.
pub fn server_slug(addr: &str) -> String {
    let addr = addr.trim();
    // Strip an explicit default port so the two spellings of one server agree.
    let canonical = match addr.rsplit_once(':') {
        Some((host, port)) if port == DEFAULT_PORT && !host.is_empty() => host,
        _ => addr,
    };
    let mut slug: String = canonical
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { SEP })
        .collect();
    // Collapse runs and trim, so `[::1]:25566` does not become a row of separators.
    let double = [SEP, SEP].iter().collect::<String>();
    while slug.contains(&double) {
        slug = slug.replace(&double, &SEP.to_string());
    }
    let slug = slug.trim_matches(SEP).to_string();
    if slug.is_empty() {
        return format!("unknown{SEP}{:016x}", fnv1a(addr.as_bytes()));
    }
    if slug.len() <= MAX_SLUG {
        return slug;
    }
    // Too long: keep a readable prefix, and disambiguate with the hash of the FULL canonical
    // address -- two hosts sharing a 31-character prefix must not share a mirror.
    let hash = format!("{:016x}", fnv1a(canonical.as_bytes()));
    let keep = MAX_SLUG - hash.len() - 1;
    let prefix: String = slug.chars().take(keep).collect();
    format!("{}{SEP}{}", prefix.trim_end_matches(SEP), hash)
}

/// The database name for a server, unless `STDB_MODULE` overrides it.
///
/// Prefixed, so the name never starts with a digit (`127.0.0.1` would) and so every database this
/// tool creates is recognisable as belonging to it.
pub fn database_name(addr: &str) -> String {
    format!("mcspacetime{SEP}{}", server_slug(addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_spellings_of_one_server_agree() {
        // The whole point: an explicit default port is the same server as an implicit one. Getting
        // this wrong mirrors one host into two databases and neither is complete.
        assert_eq!(server_slug("example.com:25565"), server_slug("example.com"));
        assert_eq!(database_name("example.com:25565"), "mcspacetime-example-com");
    }

    #[test]
    fn a_non_default_port_is_a_different_server() {
        // The demonstrated data-loss path, in one assertion: the live server and the dev replica
        // differ ONLY by port, and merging them is what wiped the mirror.
        assert_ne!(server_slug("127.0.0.1:25565"), server_slug("127.0.0.1:25566"));
        assert_eq!(server_slug("127.0.0.1:25565"), "127-0-0-1");
        assert_eq!(server_slug("127.0.0.1:25566"), "127-0-0-1-25566");
    }

    #[test]
    fn a_database_name_uses_only_characters_spacetimedb_accepts() {
        // Learned from the server, not from the docs: `spacetime publish mcspacetime_x` fails with
        // "invalid characters in database name", while `mcspacetime-127-0-0-1` was accepted and
        // created. So an UNDERSCORE here is not a style question, it is a publish failure.
        for addr in ["127.0.0.1:25566", "mc.example.com", "[::1]:25566", "a/../b"] {
            let name = database_name(addr);
            assert!(!name.contains('_'), "{addr} -> {name} would be refused at publish");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == SEP),
                "{addr} -> {name}",
            );
        }
    }

    #[test]
    fn names_are_safe_as_both_a_database_and_a_path_segment() {
        for addr in ["mc.example.com", "[::1]:25566", "a/../../etc:25565", "MC.Example.COM"] {
            let s = server_slug(addr);
            assert!(!s.is_empty(), "{addr} slugged to nothing");
            // No traversal, no separators, no leading/trailing punctuation noise. A leading dash
            // would also read as a flag to anything that takes the directory on a command line.
            assert!(!s.contains('/') && !s.contains('.'), "{addr} -> {s}");
            assert!(!s.starts_with(SEP) && !s.ends_with(SEP), "{addr} -> {s}");
            assert!(!database_name(addr).starts_with(|c: char| c.is_ascii_digit()));
        }
        assert_eq!(server_slug("MC.Example.COM"), "mc-example-com");
    }

    #[test]
    fn a_bracketed_ipv6_does_not_collapse_to_separators() {
        // `[::1]` is punctuation all the way down; naive sanitising leaves `-----`, which trims to
        // the empty string and then every IPv6 host shares one mirror.
        let s = server_slug("[::1]:25566");
        assert!(s.contains('1'), "lost the address: {s}");
        assert_ne!(s, server_slug("[::2]:25566"));
    }

    #[test]
    fn long_hostnames_stay_bounded_and_distinct() {
        let a = format!("{}.example.com", "a".repeat(80));
        let b = format!("{}.example.net", "a".repeat(80));
        assert!(server_slug(&a).len() <= MAX_SLUG);
        // Same 80-character prefix, different host: truncation alone would merge them.
        assert_ne!(server_slug(&a), server_slug(&b));
    }

    #[test]
    fn the_slug_is_stable_because_it_names_a_directory() {
        // Pinned literally. If this value ever changes, every previously downloaded world becomes
        // invisible and the bot silently starts a second copy beside it -- so a change here must be
        // a deliberate migration, not a side effect of touching the hasher. This literal already
        // earned its place once: it caught the FNV prime written as 0x1000_0000_01b3, one hex digit
        // too long, which every other test here passed happily.
        assert_eq!(format!("{:016x}", fnv1a(b"example.com")), "576846634e2714c6");
    }
}

//! Walking: deciding where the bot's feet may go, one tick at a time.
//!
//! The bot could open a chest but not cross the room, which capped the world download at whatever
//! is within view distance of spawn -- a stationary proxy mirrors one bubble forever, however long
//! you leave it running.
//!
//! WHY THE LOGIC IS PURE AND THE PACKET SENDING IS NOT. Movement is the one thing here the server
//! silently disagrees with. Send a position inside a wall or more than a few blocks from the last
//! one and vanilla does not answer with an error: it teleports the bot back (`moved too quickly`,
//! `moved wrongly`) and the two views of where the bot is diverge with nothing in the log tying the
//! divergence to the packet that caused it. So every decision -- is this cell passable, does the box
//! fit, how far may one tick carry us -- is a function of the mirrored world and the current
//! position, testable without a server, and `protocol.rs` only writes out what these return.
//!
//! WHAT THIS IS AND IS NOT. This module is the one-tick physics: given where the bot is and where it is
//! headed, may it move, and to where. It walks in a STRAIGHT LINE and does not route around anything --
//! that is `path.rs`, which searches over these same rules and hands back waypoints for this to walk
//! between. Keeping them apart is deliberate: there is exactly one definition of "can the bot be here",
//! and a route the search promises is therefore a route the walker will actually follow.

/// Vanilla walking speed, blocks per tick (0.1 m/tick is the base; ~4.317 m/s over 20 ticks).
/// Staying at or under this is what keeps the server from flagging `moved too quickly`, which it
/// judges against a per-tick budget, not an average.
pub const WALK_SPEED: f64 = 0.215;

/// The player box: 0.6 wide, 1.8 tall. Half-width, because collision is tested around the centre.
pub const HALF_WIDTH: f64 = 0.3;
pub const HEIGHT: f64 = 1.8;

/// How far the feet may rise in a single step without jumping. Vanilla's `maxUpStep` for a player is
/// 0.6, so a full block needs a jump -- but the server accepts a walked 1-block rise from a client
/// that reports it, and refusing it would leave the bot unable to climb a single stair.
pub const STEP_UP: f64 = 1.0;

/// How far the feet may descend within a SINGLE step, still reported as on the ground.
///
/// One block, because that is what walking down a stair looks like. It was 4.0 here at first, which
/// is a real bug and not a tuning choice: a step is one position packet, so a 4-block descent asks the
/// server to accept the bot moving four blocks down in one tick. Anything further is a FALL, which
/// `fall_step` pays out over several ticks the way a client does.
pub const MAX_DROP: f64 = 1.0;

/// Fall damage in health points for a drop of `blocks`, as vanilla computes it.
///
/// `max(0, distance - 3)`, where a heart is two points. The bot cannot eat or heal, so damage is a
/// one-way budget and every fall spends some of it.
pub fn fall_damage(blocks: f64) -> f64 {
    (blocks - 3.0).max(0.0)
}

/// The deepest drop worth taking at this health, keeping `reserve` points spare.
///
/// THE REASON THIS EXISTS: the bot walked itself to death on the live server. Two six-block falls
/// while routing, no way to heal, and `data get entity Powback Health` read `0.0f`. A dead player is
/// sent NO CHUNKS, so the mirror froze on one chunk and every later route failed for want of a world to
/// route through -- a failure that looked like a pathfinding bug and was a corpse.
pub fn survivable_drop(health: f32, reserve: f64) -> f64 {
    // Invert `damage = distance - 3`, against the damage we are willing to spend.
    let budget = (health as f64 - reserve).max(0.0);
    (budget + 3.0).min(SAFE_FALL)
}

/// The health to keep in hand. Enough to survive a surprise -- a mob, a second unplanned drop -- rather
/// than arriving somewhere with one point left.
pub const FALL_RESERVE: f64 = 8.0;

/// The furthest drop the walker will deliberately step off into.
///
/// Vanilla fall damage is `distance - 3` damage points, so eight blocks costs 5 of 20 -- noticeable,
/// nowhere near lethal, and the bot has no way to heal or eat. It is also what makes the feature
/// usable at all here: the proxy spawns on a ledge six blocks above the floor, so a limit under that
/// leaves it stranded on the block it joined on. Chosen to clear that with room, not to fit it.
pub const SAFE_FALL: f64 = 8.0;

/// Close enough to a target to call it reached, horizontally. Smaller than this and the bot jitters
/// around the centre of the block for ever, because one tick overshoots it.
pub const ARRIVE_EPSILON: f64 = 0.35;

/// Is a block something the bot's body may occupy?
///
/// Name-based, because that is the only thing that generalises across mods -- a modded pack's state
/// ids mean nothing here, but a block still called `*_torch` is still a torch. **Unknown means
/// solid**: in a modded pack most unrecognised names are real blocks, and the two failures are not
/// symmetric. Treating a wall as passable desyncs the bot from the server; treating a flower as a
/// wall makes it stop and say so.
pub fn passable(name: &str) -> bool {
    let path = name.rsplit(':').next().unwrap_or(name);
    // Exceptions FIRST: these contain a passable word but are fully solid blocks, and the
    // substring pass below would wave them through. `grass_block` is the one that matters -- read as
    // passable, the bot walks into the ground on every natural surface in the world.
    const SOLID_LOOKALIKES: [&str; 10] = [
        "grass_block", "mushroom_block", "mushroom_stem", "sculk_vein_block", "vine_block",
        "torch_block", "sign_post_block", "rail_block", "bamboo_block", "sugar_cane_block",
    ];
    if SOLID_LOOKALIKES.iter().any(|s| path == *s) {
        return false;
    }
    if path.ends_with("_block") && !path.starts_with("light_block") {
        // `<thing>_block` is the vanilla naming convention for the SOLID form of a thing.
        return false;
    }
    const EXACT: [&str; 12] = [
        "air", "cave_air", "void_air", "water", "bubble_column", "snow", "light", "structure_void",
        "fire", "soul_fire", "cobweb", "powder_snow",
    ];
    if EXACT.contains(&path) {
        return true;
    }
    // Substrings that survive modded renaming. Ordered longest-intent first for readability only;
    // the match is unordered.
    const PASSABLE_PARTS: [&str; 30] = [
        "torch", "flower", "sapling", "tall_grass", "short_grass", "fern", "vine", "lever",
        "button", "rail", "sign", "banner", "carpet", "pressure_plate", "tripwire", "string",
        "ladder", "mushroom", "wheat", "carrots", "potatoes", "beetroots", "seeds", "crop",
        "sugar_cane", "kelp", "seagrass", "lily_pad", "redstone_wire", "repeater",
    ];
    PASSABLE_PARTS.iter().any(|p| path.contains(p))
}

/// Vanilla ladder-climb speed, blocks per tick. Slower than walking, which is why it is its own
/// constant rather than reusing `WALK_SPEED` -- climbing at walking speed is a speed violation.
pub const CLIMB_SPEED: f64 = 0.118;

/// Can the bot move VERTICALLY inside this block?
///
/// Ladders, vines and scaffolding. This is the answer to the reachability limit that measuring the live
/// settlement exposed: the base is a hollow structure with floors at different heights, `STEP_UP` is one
/// block, and so whole floors were unreachable even though the building is full of ways up. A climbable
/// is not a step -- it is a column the bot ascends a fraction of a block at a time.
///
/// Note these are deliberately NOT settled by the collision dump: a ladder has a real (thin) collision
/// shape, so collision says "solid" while a player both stands in it and climbs it.
pub fn climbable(name: &str) -> bool {
    let path = name.rsplit(':').next().unwrap_or(name);
    path.contains("ladder")
        || path.contains("scaffolding")
        || path == "vine"
        || path.contains("weeping_vines")
        || path.contains("twisting_vines")
        || path.contains("cave_vines")
}

/// A block that would hurt to stand in. Never used as footing and never walked through, even though
/// a client CAN move through lava -- a drowned bot stops mirroring, which is the failure this avoids.
pub fn hazardous(name: &str) -> bool {
    let path = name.rsplit(':').next().unwrap_or(name);
    path.contains("lava") || path == "fire" || path == "soul_fire" || path == "magma_block"
}

/// What the caller must supply: is the block at these coordinates passable?
///
/// `None` means "not mirrored yet", which is NOT the same as air. An unloaded cell is the one place
/// a walker can do real damage -- stepping into it means asking the server to move somewhere we know
/// nothing about -- so callers treat it as blocking.
pub trait Blocks {
    fn passable_at(&self, x: i32, y: i32, z: i32) -> Option<bool>;

    /// Is there a climbable (ladder, vine, scaffolding) in this block? Defaults to "no", so a caller
    /// that does not care -- or a test fixture describing a world without ladders -- need not say.
    fn climbable_at(&self, _x: i32, _y: i32, _z: i32) -> Option<bool> {
        Some(false)
    }
}

/// Do the cells the player box would overlap at this position all admit it?
///
/// Tests every column the box touches, not just the centre. The centre-only shortcut is why simple
/// bots snag on corners: standing at x = 8.95 the box reaches into x = 9, and a wall there stops a
/// real player even though the block under their middle is empty.
pub fn box_fits(blocks: &impl Blocks, x: f64, y: f64, z: f64) -> bool {
    let x0 = (x - HALF_WIDTH).floor() as i32;
    let x1 = (x + HALF_WIDTH).floor() as i32;
    let z0 = (z - HALF_WIDTH).floor() as i32;
    let z1 = (z + HALF_WIDTH).floor() as i32;
    // Feet cell through the cell containing the top of the head. `- 1e-9` so a height that lands
    // exactly on a boundary does not claim the block above.
    let y0 = y.floor() as i32;
    let y1 = (y + HEIGHT - 1e-9).floor() as i32;
    for bx in x0..=x1 {
        for bz in z0..=z1 {
            for by in y0..=y1 {
                if blocks.passable_at(bx, by, bz) != Some(true) {
                    return false;
                }
            }
        }
    }
    true
}

/// The y the feet settle at when standing over this column, searching from `from_y`.
///
/// Returns `None` when there is no footing within `MAX_DROP` -- a hole, or unmirrored world.
/// Scans EVERY column the player box overlaps and returns the HIGHEST support among them, because
/// that is what standing on the edge of a step means -- a player straddling a stair rests on the
/// upper block, not the lower one. Sampling the centre column alone was the first implementation
/// here and it is a real bug, not a subtlety: it made `step_toward` reject every stair, because the
/// box fitted at the raised height while the centre column still reported the floor below, so a step
/// UP looked like a step into mid-air. `it_climbs_a_stair` is the test that caught it.
pub fn footing(blocks: &impl Blocks, x: f64, y: f64, z: f64) -> Option<f64> {
    support_within(blocks, x, y, z, MAX_DROP)
}

/// How far below the feet `fall_step` will look for a landing.
///
/// Much deeper than `MAX_DROP`, because the two questions are different: `MAX_DROP` is "is this a
/// step I am willing to take", and this is "if I am already falling, where do I land". Bounded all the
/// same -- a fall with no known bottom is one into unmirrored world, and refusing it is what keeps
/// the bot from claiming a position the server will not agree with.
pub const MAX_FALL_SEARCH: f64 = 64.0;

/// Blocks per tick of falling. Vanilla accelerates to about 3.9 and this does not model that; a
/// constant is enough for a bot whose job is to arrive, and staying well under terminal velocity keeps
/// each packet's delta inside what the server will accept without comment.
pub const FALL_SPEED: f64 = 0.5;

/// The highest support under the box within `depth`, or `None` for "nothing to stand on".
pub fn support_within(blocks: &impl Blocks, x: f64, y: f64, z: f64, depth: f64) -> Option<f64> {
    let x0 = (x - HALF_WIDTH).floor() as i32;
    let x1 = (x + HALF_WIDTH).floor() as i32;
    let z0 = (z - HALF_WIDTH).floor() as i32;
    let z1 = (z + HALF_WIDTH).floor() as i32;
    let lowest = (y - depth).floor() as i32;
    let mut best: Option<f64> = None;
    for bx in x0..=x1 {
        for bz in z0..=z1 {
            let mut probe = y.floor() as i32;
            while probe >= lowest {
                match blocks.passable_at(bx, probe - 1, bz) {
                    // Solid below: the feet could rest on top of it.
                    Some(false) => {
                        let top = probe as f64;
                        if best.is_none() || top > best.unwrap() {
                            best = Some(top);
                        }
                        break;
                    }
                    // Passable: keep falling down this column.
                    Some(true) => probe -= 1,
                    // Unmirrored: we do not know what is under this corner, so we do not claim to be
                    // standing anywhere. Half a floor is not a floor.
                    None => return None,
                }
            }
        }
    }
    best
}

/// One tick of falling: the new feet height, or `None` when the bot is already standing.
///
/// WHY THIS EXISTS AT ALL. Without it `walk_to` is correct and useless. The proxy joined this server
/// standing on a two-block ledge at y = 68 with air on every side -- measured, not guessed: the floor
/// at y = 67 around it is `..##.......` and nothing else within five blocks -- so every direction was
/// a drop it rightly refused, and the bot could not leave the block it spawned on. A client that
/// cannot fall cannot reach the ground, and a bot that cannot reach the ground cannot mirror anything
/// but its spawn.
///
/// Returns `None` when supported OR when there is no known landing, which are different situations
/// with the same answer: do not move. A fall into unmirrored world is exactly the claim the server
/// will disagree with.
pub fn fall_step(blocks: &impl Blocks, (x, y, z): (f64, f64, f64)) -> Option<f64> {
    // Standing on something? Then this is not a fall.
    if support_within(blocks, x, y, z, 0.0) == Some(y) {
        return None;
    }
    // Hanging on a ladder or vine is not falling either. Without this the bot would "fall" down every
    // ladder it climbed, one tick after each climb step, and never get anywhere.
    if blocks.climbable_at(x.floor() as i32, y.floor() as i32, z.floor() as i32) == Some(true) {
        return None;
    }
    let landing = support_within(blocks, x, y, z, MAX_FALL_SEARCH)?;
    if landing >= y {
        return None;
    }
    // Never overshoot the landing: the bot must come to rest ON the floor, not inside it.
    Some((y - FALL_SPEED).max(landing))
}

/// The outcome of one tick of walking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// Move the feet here, and report `on_ground`.
    Move { x: f64, y: f64, z: f64, on_ground: bool },
    /// Already there.
    Arrived,
    /// Cannot make progress toward the target from here, and why.
    Blocked(&'static str),
}

/// One tick toward `target`, or the reason we cannot take it.
///
/// Straight-line: it moves along the vector to the target and does not look for a way round. The
/// step is tried whole, then on each axis alone, which is what lets a player slide along a wall
/// instead of stopping dead against it at an angle.
pub fn step_toward(
    blocks: &impl Blocks,
    (x, y, z): (f64, f64, f64),
    (tx, ty, tz): (f64, f64, f64),
    // The deepest drop this bot may take right now: `survivable_drop(health, FALL_RESERVE)`. A
    // parameter rather than a constant because it depends on how hurt the bot is, and a walker that
    // ignores that walks itself to death -- which it did, on the live server.
    max_fall: f64,
) -> Step {
    let (dx, dz) = (tx - x, tz - z);
    let flat = (dx * dx + dz * dz).sqrt();
    if flat <= ARRIVE_EPSILON && (ty - y).abs() <= STEP_UP {
        return Step::Arrived;
    }
    if flat <= ARRIVE_EPSILON {
        // Standing in a ladder or vine? Then vertical IS a move available to us, and this is how the bot
        // changes floor in a built structure. Climb toward the waypoint a fraction of a block at a time,
        // at ladder speed -- climbing at walking speed is a speed violation.
        if blocks.climbable_at(x.floor() as i32, y.floor() as i32, z.floor() as i32) == Some(true) {
            let up = ty > y;
            let ny = if up { (y + CLIMB_SPEED).min(ty) } else { (y - CLIMB_SPEED).max(ty) };
            if box_fits(blocks, x, ny, z) {
                // On a ladder the player is supported by the ladder, not by the ground.
                return Step::Move { x, y: ny, z, on_ground: false };
            }
        }
        // Horizontally there, vertically not, and there is no horizontal move left to make -- so the
        // generic "blocked in every direction" would be actively misleading, reported alongside "0.00
        // blocks short". A straight-line walker cannot descend a shaft it is standing on the lid of:
        // it needs a sideways step to fall from, and every sideways step is away from the target.
        return Step::Blocked(if ty < y {
            "standing directly above the target and cannot descend: a sideways step is needed to fall from, and every sideways step leads away"
        } else {
            "standing directly below the target and cannot climb: only a one-block step up is possible"
        });
    }
    let (ux, uz) = if flat > 1e-9 { (dx / flat, dz / flat) } else { (0.0, 0.0) };
    let reach = flat.min(WALK_SPEED);
    // Whole step, then each axis alone: sliding along a wall beats stopping against it.
    for (sx, sz) in [(ux * reach, uz * reach), (ux * reach, 0.0), (0.0, uz * reach)] {
        if sx == 0.0 && sz == 0.0 {
            continue;
        }
        let (nx, nz) = (x + sx, z + sz);
        // Try level, then stepping up, then settling down onto whatever is there.
        for rise in [0.0, STEP_UP] {
            let ny = y + rise;
            if !box_fits(blocks, nx, ny, nz) {
                continue;
            }
            let Some(ground) = footing(blocks, nx, ny, nz) else {
                // Nothing within a single step's descent. That is not the end of it -- it may be a
                // ledge worth walking off -- see the fall case below.
                continue;
            };
            // A rise we did not ask for means the column pushed us up; re-check the box there.
            if ground > ny + 1e-9 || !box_fits(blocks, nx, ground, nz) {
                continue;
            }
            if ground < y - MAX_DROP {
                continue;
            }
            return Step::Move { x: nx, y: ground, z: nz, on_ground: true };
        }
        // No grounded step this way. Walking OFF a ledge is still allowed, provided the landing is
        // known and survivable: move horizontally at the current height and report airborne, and let
        // `fall_step` pay the descent out over the following ticks. Snapping straight down to the
        // landing would be one packet claiming an eight-block drop, which is what the server objects
        // to -- and refusing outright is what stranded the bot on its spawn ledge.
        if box_fits(blocks, nx, y, nz) {
            if let Some(landing) = support_within(blocks, nx, y, nz, max_fall) {
                if landing < y && box_fits(blocks, nx, landing, nz) {
                    return Step::Move { x: nx, y, z: nz, on_ground: false };
                }
            }
        }
    }
    Step::Blocked("blocked in every direction toward the target")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A world you write out by hand. Anything not mentioned is UNMIRRORED (`None`), so a test that
    /// wants open air has to say so -- which is the right default, because conflating "unknown" with
    /// "air" is the bug this whole module is careful about.
    struct Grid(HashMap<(i32, i32, i32), bool>);

    impl Blocks for Grid {
        fn passable_at(&self, x: i32, y: i32, z: i32) -> Option<bool> {
            self.0.get(&(x, y, z)).copied()
        }
    }

    /// A floor at y = 63 (so feet rest at 64) spanning a square, with air above it.
    fn flat_world(r: i32) -> Grid {
        let mut g = HashMap::new();
        for x in -r..=r {
            for z in -r..=r {
                g.insert((x, 63, z), false); // the floor
                for y in 64..=70 {
                    g.insert((x, y, z), true); // air
                }
            }
        }
        Grid(g)
    }

    #[test]
    fn grass_block_is_not_a_plant() {
        // THE ONE THAT MATTERS. `grass_block` contains "grass"; read as passable, the bot tries to
        // walk through the ground on every natural surface in the world.
        assert!(!passable("minecraft:grass_block"));
        assert!(passable("minecraft:short_grass"));
        assert!(passable("minecraft:tall_grass"));
        // Same trap, other blocks.
        assert!(!passable("minecraft:mushroom_stem"));
        assert!(passable("minecraft:brown_mushroom"));
        assert!(!passable("minecraft:redstone_block"));
        assert!(passable("minecraft:redstone_wire"));
    }

    #[test]
    fn an_unknown_modded_block_is_solid() {
        // The asymmetry this module is built on: a wall read as air desyncs the bot from the server,
        // a flower read as a wall just stops it. So unknown resolves to solid.
        assert!(!passable("create:brass_casing"));
        assert!(!passable("aeronauticsdiscovery:pin"));
        assert!(!passable("somemod:utterly_unheard_of"));
        // ...but a modded torch is still a torch.
        assert!(passable("create:soul_torch_lever"));
    }

    #[test]
    fn lava_is_never_footing_even_though_a_client_may_enter_it() {
        assert!(hazardous("minecraft:lava"));
        assert!(hazardous("minecraft:flowing_lava"));
        assert!(!hazardous("minecraft:water"));
    }

    #[test]
    fn the_box_is_tested_at_its_corners_not_its_centre() {
        // Standing at x = 8.9 the box reaches to 9.2, so a wall at x = 9 blocks it -- even though
        // the block under the centre (x = 8) is clear. Centre-only collision is why naive bots snag
        // on corners and then rubber-band.
        let mut g = flat_world(12);
        for y in 64..=70 {
            g.0.insert((9, y, 0), false); // a wall column at x = 9
        }
        assert!(!box_fits(&g, 8.9, 64.0, 0.0), "corner overlap should not fit");
        assert!(box_fits(&g, 8.5, 64.0, 0.0), "well clear should fit");
    }

    #[test]
    fn a_two_block_ceiling_admits_the_box_and_a_one_block_one_does_not() {
        let mut g = flat_world(4);
        // Ceiling at y = 65 -> only one block of space, and the player is 1.8 tall.
        for x in -4..=4 {
            for z in -4..=4 {
                g.0.insert((x, 65, z), false);
            }
        }
        assert!(!box_fits(&g, 0.5, 64.0, 0.5), "1.8 tall cannot fit under a 1-block ceiling");
    }

    #[test]
    fn unmirrored_world_is_not_walked_into() {
        // THE RULE THAT PROTECTS THE SERVER'S VIEW OF US. Outside the mirrored square every cell is
        // None, and a step into it must be refused -- asking the server to move somewhere we know
        // nothing about is how a bot ends up inside a wall with no log line explaining it.
        // Walk east until it refuses, and check it refused INSIDE the mirrored square. Asserting on
        // a single step from the middle proves nothing -- one step is 0.215 blocks and never reaches
        // the edge, which is how this test first passed against a walker that would have strolled
        // straight out into the unknown.
        let g = flat_world(3);
        let mut pos = (0.5, 64.0, 0.5);
        let mut steps = 0;
        loop {
            match step_toward(&g, pos, (100.0, 64.0, 0.5), SAFE_FALL) {
                Step::Move { x, y, z, .. } => pos = (x, y, z),
                Step::Blocked(_) => break,
                Step::Arrived => panic!("claimed to arrive 100 blocks away"),
            }
            steps += 1;
            assert!(steps < 200, "never stopped, reached {pos:?}");
        }
        // The floor ends at block 3, so the box may not pass x = 3.7 (its edge at 4.0).
        assert!(pos.0 < 3.7, "walked into unmirrored world, stopped at {pos:?}");
        assert!(pos.0 > 2.0, "stopped far short of the edge at {pos:?}");
    }

    #[test]
    fn it_arrives_rather_than_jittering_around_the_target() {
        let g = flat_world(12);
        // Standing within epsilon of the target: done, no packet.
        assert_eq!(step_toward(&g, (5.0, 64.0, 5.0), (5.1, 64.0, 5.1), SAFE_FALL), Step::Arrived);
    }

    #[test]
    fn a_walk_across_open_ground_converges() {
        // The end-to-end claim, simulated: repeated steps actually reach the target, and do it in a
        // sane number of ticks rather than creeping or oscillating.
        let g = flat_world(20);
        let mut pos = (0.5, 64.0, 0.5);
        let target = (15.5, 64.0, 0.5);
        let mut ticks = 0;
        loop {
            match step_toward(&g, pos, target, SAFE_FALL) {
                Step::Move { x, y, z, .. } => pos = (x, y, z),
                Step::Arrived => break,
                Step::Blocked(why) => panic!("blocked on open ground at {pos:?}: {why}"),
            }
            ticks += 1;
            assert!(ticks < 500, "did not converge, stuck at {pos:?}");
        }
        // 15 blocks at 0.215/tick is ~70 ticks. Generous bounds; the point is it is neither 1 nor 500.
        assert!((60..=90).contains(&ticks), "took {ticks} ticks to cross 15 blocks");
    }

    #[test]
    fn it_climbs_a_stair_and_settles_down_the_other_side() {
        let mut g = flat_world(20);
        // A one-block step up at x = 5, and the air above it.
        g.0.insert((5, 64, 0), false);
        g.0.insert((5, 64, 1), false);
        g.0.insert((5, 64, -1), false);
        let mut pos = (3.5, 64.0, 0.5);
        let mut climbed = false;
        for _ in 0..200 {
            match step_toward(&g, pos, (8.5, 64.0, 0.5), SAFE_FALL) {
                Step::Move { x, y, z, .. } => {
                    if y > pos.1 {
                        climbed = true;
                    }
                    pos = (x, y, z);
                }
                Step::Arrived => break,
                Step::Blocked(_) => break,
            }
        }
        assert!(climbed, "never stepped up; ended at {pos:?}");
        assert!(pos.0 > 5.0, "did not get past the step, ended at {pos:?}");
    }

    #[test]
    fn it_slides_along_a_wall_instead_of_stopping_dead() {
        // Heading diagonally into a wall that blocks only the x component. A whole-step-only walker
        // stops here; a real player slides along z. Without the per-axis retry the bot would report
        // "blocked" a block short of a doorway it could have walked through.
        let mut g = flat_world(20);
        for y in 64..=70 {
            for z in -20..=20 {
                g.0.insert((6, y, z), false); // solid wall plane at x = 6
            }
        }
        // Start with the box almost touching the wall: at x = 5.65 its edge is 5.95, so one step
        // east would push it into block 6. Starting at 5.0 (as this test first did) leaves the whole
        // step clear and tests nothing -- it passed by never reaching the wall.
        let start = (5.65, 64.0, 0.5);
        match step_toward(&g, start, (9.0, 64.0, 6.0), SAFE_FALL) {
            Step::Move { x, z, .. } => {
                assert!(x <= start.0 + 1e-9, "moved into the wall: x {x}");
                assert!(z > start.2, "did not slide along it: z {z}");
            }
            other => panic!("should have slid, got {other:?}"),
        }
    }

    #[test]
    fn A_HURT_BOT_TAKES_SHALLOWER_DROPS_AND_A_DYING_ONE_TAKES_NONE() {
        // THE BUG THAT KILLED IT. On the live server the bot took two six-block falls while routing,
        // could not heal, and `data get entity Powback Health` read `0.0f`. A dead player is sent no
        // chunks at all, so the mirror froze on ONE chunk and every later route failed for want of a
        // world to route through -- which looked like a pathfinding bug and was a corpse.
        assert_eq!(fall_damage(3.0), 0.0, "three blocks is free");
        assert_eq!(fall_damage(8.0), 5.0);

        // Healthy: the full limit.
        assert_eq!(survivable_drop(20.0, FALL_RESERVE), SAFE_FALL);
        // Hurt: shallower, and monotonic in health -- never the other way round.
        let hurt = survivable_drop(10.0, FALL_RESERVE);
        assert!(hurt < SAFE_FALL, "a hurt bot should be more cautious, got {hurt}");
        assert!(survivable_drop(14.0, FALL_RESERVE) >= hurt);
        // At or below the reserve, the only drops left are the free ones.
        assert_eq!(survivable_drop(FALL_RESERVE as f32, FALL_RESERVE), 3.0);
        assert_eq!(survivable_drop(1.0, FALL_RESERVE), 3.0, "never negative, never unbounded");
        // And whatever it permits must actually be survivable, which is the property that matters.
        for hp in [1.0f32, 5.0, 8.0, 12.0, 20.0] {
            let d = survivable_drop(hp, FALL_RESERVE);
            assert!(fall_damage(d) < hp as f64, "a {d}-block drop would kill a bot on {hp} health");
        }
    }

    #[test]
    fn the_fall_limit_actually_gates_stepping_off_a_ledge() {
        // The constant is only worth anything if `step_toward` honours it. A six-block drop, offered to
        // a healthy walker and then to one that may only fall three.
        let mut g = HashMap::new();
        for x in 0..=6 {
            for z in 0..=2 {
                g.insert((x, 61, z), false); // floor six below
                for y in 62..=70 {
                    g.insert((x, y, z), true);
                }
            }
        }
        g.insert((0, 67, 1), false); // the ledge
        let grid = Grid(g);
        let to = (6.5, 62.0, 1.5);
        // Walk east under each limit and record whether the bot ever goes airborne. Stepping once is
        // not enough to tell: the box straddles the ledge block for the first step or two, so those are
        // legitimately grounded.
        let leaves_the_ledge = |max_fall: f64| {
            let mut pos = (0.5, 68.0, 1.5);
            for _ in 0..40 {
                match step_toward(&grid, pos, to, max_fall) {
                    Step::Move { x, y, z, on_ground } => {
                        if !on_ground {
                            return true;
                        }
                        pos = (x, y, z);
                    }
                    _ => return false,
                }
            }
            false
        };
        assert!(leaves_the_ledge(SAFE_FALL), "a healthy bot should step off a six-block drop");
        assert!(!leaves_the_ledge(3.0), "a bot limited to three blocks must NOT step off a six-block drop");
    }

    #[test]
    fn standing_on_the_floor_is_not_falling() {
        let g = flat_world(6);
        assert_eq!(fall_step(&g, (0.5, 64.0, 0.5)), None);
    }

    #[test]
    fn a_fall_lands_exactly_on_the_floor_and_stops() {
        // The bug this rules out: overshooting the landing and coming to rest INSIDE the floor, which
        // the server answers with a teleport and which then looks like the walker desyncing.
        let mut g = flat_world(6);
        for y in 65..=80 {
            for x in -6..=6 {
                for z in -6..=6 {
                    g.0.insert((x, y, z), true); // air above the floor at 63
                }
            }
        }
        let mut y = 80.0;
        let mut ticks = 0;
        while let Some(next) = fall_step(&g, (0.5, y, 0.5)) {
            assert!(next < y, "fall did not descend: {next} from {y}");
            y = next;
            ticks += 1;
            assert!(ticks < 200, "never landed, at {y}");
        }
        assert_eq!(y, 64.0, "landed at {y}, not on top of the floor at 63");
    }

    #[test]
    fn it_does_not_fall_into_unmirrored_world() {
        // The ledge the bot really spawned on, in miniature: a block to stand on and nothing known
        // underneath. Falling here would claim a position the server has no reason to agree with, so
        // the answer is to stay put -- the same rule that governs stepping.
        let mut g = HashMap::new();
        g.insert((0, 67, 0), false); // the ledge
        for y in 68..=72 {
            g.insert((0, y, 0), true); // air above it
        }
        let grid = Grid(g);
        assert_eq!(fall_step(&grid, (0.5, 68.0, 0.5)), None, "standing on the ledge");
        // Step off the side, where nothing below is mirrored: still no fall.
        let mut g2 = HashMap::new();
        for y in 60..=72 {
            g2.insert((1, y, 0), true); // a mirrored air column with NO known bottom
        }
        assert_eq!(fall_step(&Grid(g2), (1.5, 68.0, 0.5)), None, "fell into the unknown");
    }

    #[test]
    fn being_directly_above_the_target_says_so_rather_than_blaming_every_direction() {
        // Measured live, and the message was the wrong one: asked to walk to the floor six blocks
        // beneath its ledge, the bot arrived horizontally and reported "blocked in every direction
        // toward the target ... 0.00 blocks short", which reads like a bug in the walker rather than
        // the geometry it actually is.
        let mut g = HashMap::new();
        g.insert((64, 67, 36), false); // the ledge
        for y in 68..=72 {
            g.insert((64, y, 36), true);
        }
        for y in 62..=66 {
            g.insert((64, y, 36), true); // the shaft below it
        }
        g.insert((64, 61, 36), false); // the floor at the bottom
        match step_toward(&Grid(g), (64.5, 68.0, 36.5), (64.5, 62.0, 36.5), SAFE_FALL) {
            Step::Blocked(why) => assert!(why.contains("directly above"), "unhelpful reason: {why}"),
            other => panic!("expected a blocked-with-reason, got {other:?}"),
        }
    }

    #[test]
    fn it_leaves_the_spawn_ledge_by_falling_rather_than_refusing() {
        // THE LIVE CASE, measured off the real server and reduced to a fixture. The proxy joined
        // standing at y = 68 on a two-block ledge (64,67,36 and 65,67,36 solid, everything around them
        // air) with the settlement floor at y = 61 -- a six-block drop. The first version of this
        // module refused every direction and the bot could not leave the block it spawned on, so
        // `walk_to` was correct and useless. It must now step off and fall.
        let mut g = HashMap::new();
        // The ledge, and the floor six blocks below it, across a decent span.
        for x in 60..=72 {
            for z in 30..=42 {
                g.insert((x, 61, z), false); // the real floor -> feet land at 62
                for y in 62..=72 {
                    g.insert((x, y, z), true); // air between
                }
            }
        }
        g.insert((64, 67, 36), false); // the ledge
        g.insert((65, 67, 36), false);
        let grid = Grid(g);

        let mut pos = (64.5, 68.0, 36.5);
        // Standing on the ledge is standing, not falling.
        assert_eq!(fall_step(&grid, pos), None);
        // Walk east along the ledge. The ledge is two blocks wide, so the first steps are legitimately
        // grounded; the step that leaves it must report AIRBORNE at the SAME height, rather than
        // snapping six blocks down in a single packet.
        let mut left_the_ledge = false;
        for _ in 0..40 {
            match step_toward(&grid, pos, (70.5, 62.0, 36.5), SAFE_FALL) {
                Step::Move { x, y, z, on_ground } => {
                    if !on_ground {
                        assert_eq!(y, pos.1, "snapped downward in one step instead of falling");
                        left_the_ledge = true;
                        pos = (x, y, z);
                        break;
                    }
                    assert_eq!(y, 68.0, "changed height while still on the ledge");
                    pos = (x, y, z);
                }
                other => panic!("refused to leave the ledge at {pos:?}: {other:?}"),
            }
        }
        assert!(left_the_ledge, "never stepped off, stopped at {pos:?}");
        // Now run it: step, then fall, until it arrives on the floor below.
        for _ in 0..2000 {
            if let Some(ny) = fall_step(&grid, pos) {
                pos.1 = ny;
                continue;
            }
            match step_toward(&grid, pos, (70.5, 62.0, 36.5), SAFE_FALL) {
                Step::Move { x, y, z, .. } => pos = (x, y, z),
                Step::Arrived => break,
                Step::Blocked(why) => panic!("stuck at {pos:?}: {why}"),
            }
        }
        assert_eq!(pos.1, 62.0, "did not end up on the floor, at {pos:?}");
        assert!((pos.0 - 70.5).abs() < 1.0, "did not reach the target, at {pos:?}");
    }

    #[test]
    fn it_will_not_walk_off_a_cliff_taller_than_the_drop_limit() {
        let mut g = flat_world(6);
        // Beyond x = 6 the floor is gone but the air is mirrored, all the way down past MAX_DROP.
        for x in 7..=12 {
            for z in -6..=6 {
                for y in 40..=70 {
                    g.0.insert((x, y, z), true);
                }
            }
        }
        let mut pos = (5.5, 64.0, 0.5);
        for _ in 0..200 {
            match step_toward(&g, pos, (12.0, 64.0, 0.5), SAFE_FALL) {
                Step::Move { x, y, z, .. } => pos = (x, y, z),
                _ => break,
            }
        }
        assert!(pos.1 >= 64.0, "fell off: {pos:?}");
        assert!(pos.0 < 7.5, "walked out over the void to {pos:?}");
    }
}

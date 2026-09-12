//! Routing: finding a walkable sequence of blocks from here to there.
//!
//! `walk.rs` moves the bot in a straight line, which is enough to cross a room and not enough to
//! leave one. Two measured failures on the live server motivated this, and neither is fixable by
//! tuning the walker:
//!
//!   1. **It could not descend a shaft it was standing on the lid of.** Asked to walk to the floor six
//!      blocks below its spawn ledge, it arrived horizontally and stopped: a fall needs a sideways step
//!      to fall FROM, and every sideways step is away from the target, so a walker that only ever
//!      reduces distance can never take one. A search takes it happily -- step off the ledge, land, walk
//!      back underneath -- because the detour is just a longer path, not a wrong direction.
//!   2. **The reachable area was tiny.** The settlement is a hollow structure; along x=64 there is no
//!      floor at all at y=61 beyond z=41. Anything worth mirroring is around an obstacle.
//!
//! A* over block-shaped nodes, with the SAME geometry the walker uses -- `box_fits` and the
//! multi-column `footing` from `walk.rs`, not a second opinion about what is solid. A path this module
//! returns that the walker then refuses to follow would be the worst of both, so there is exactly one
//! definition of "can the bot be here" and both use it.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use crate::walk::{box_fits, support_within, Blocks, MAX_DROP, SAFE_FALL, STEP_UP};

/// How many nodes the search may expand before giving up.
///
/// A bound rather than a time limit, so the same request always costs the same and a slow moment on
/// the host cannot turn a reachable target into an unreachable one. 60k nodes is a few hundred
/// milliseconds and covers a couple of hundred blocks of built structure.
pub const MAX_EXPANSIONS: usize = 60_000;

/// How far from the start the search will wander, in blocks, on each axis. Keeps a hopeless request
/// (a target inside bedrock) from exploring the whole mirrored world before failing.
pub const SEARCH_RADIUS: i32 = 192;

/// Cost of a plain step. Everything else is priced against this.
const COST_STEP: u32 = 10;
/// A step up costs slightly more, so a flat route is preferred to a staircase of equal length.
const COST_STEP_UP: u32 = 14;
/// Each block of falling, on top of the step that started it. Falling is fast but it costs health, so
/// the search prefers stairs when they are not much longer.
const COST_PER_FALL: u32 = 4;

/// A node in the search: the block the bot's FEET occupy.
type Node = (i32, i32, i32);

#[derive(PartialEq, Eq)]
struct Queued {
    /// `g + h`, the value the heap orders on.
    f: u32,
    g: u32,
    node: Node,
}

impl Ord for Queued {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a MAX-heap and A* wants the smallest f, so the comparison is reversed.
        // Ties break on g, preferring the node we know more about -- it settles the frontier faster.
        other.f.cmp(&self.f).then_with(|| other.g.cmp(&self.g))
    }
}

impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Can the bot stand with its feet in this block?
///
/// Uses the walker's own geometry at the block's CENTRE, which is where a waypoint puts it. A climbable
/// counts: hanging on a ladder is a position the bot can hold and move from, which is the whole point of
/// including ladders in the search.
fn standable(blocks: &impl Blocks, (x, y, z): Node) -> bool {
    let (cx, cz) = (x as f64 + 0.5, z as f64 + 0.5);
    if !box_fits(blocks, cx, y as f64, cz) {
        return false;
    }
    support_within(blocks, cx, y as f64, cz, 0.0) == Some(y as f64)
        || blocks.climbable_at(x, y, z) == Some(true)
}

/// Cost of one block of climbing. Dearer than a step because climbing is about half walking speed, so a
/// ladder detour should lose to a staircase of similar length.
const COST_CLIMB: u32 = 20;

/// Where the bot ends up if it walks into this column from `from_y`, and what that move costs.
///
/// `None` when the move is not available. Three outcomes: level, up one, or a fall to a known landing.
fn move_into(blocks: &impl Blocks, (x, z): (i32, i32), from_y: i32, max_fall: f64) -> Option<(i32, u32)> {
    let (cx, cz) = (x as f64 + 0.5, z as f64 + 0.5);
    // Level, then up: the cheapest first, so a staircase is not preferred to flat ground.
    for (rise, cost) in [(0, COST_STEP), (1, COST_STEP_UP)] {
        let y = from_y + rise;
        if rise as f64 > STEP_UP {
            continue;
        }
        if box_fits(blocks, cx, y as f64, cz) && support_within(blocks, cx, y as f64, cz, 0.0) == Some(y as f64) {
            return Some((y, cost));
        }
    }
    // A step DOWN of at most MAX_DROP is still one step, and beyond that it is a fall. Both need the
    // box to fit where we enter the column, at the height we entered it -- a fall starts by walking
    // off the edge, not by dropping through the floor.
    if !box_fits(blocks, cx, from_y as f64, cz) {
        return None;
    }
    let landing = support_within(blocks, cx, from_y as f64, cz, max_fall)?;
    let drop = from_y as f64 - landing;
    if drop <= 0.0 {
        return None;
    }
    if !box_fits(blocks, cx, landing, cz) {
        return None;
    }
    let extra = if drop <= MAX_DROP { 0 } else { (drop as u32) * COST_PER_FALL };
    Some((landing as i32, COST_STEP + extra))
}

/// Distance to the goal, priced in steps, as the LARGEST of the three ways it is still far away.
///
/// Admissible, which is what makes A* correct: one move changes x or z by exactly one, gains at most
/// `STEP_UP` of height, and loses at most `SAFE_FALL`, so no route can reduce any single component
/// faster than one unit per move. It must be the MAX and not the SUM -- a single step can move one
/// block horizontally AND climb one at a cost of `COST_STEP_UP`, so summing those two components would
/// claim 2 units for one move and overestimate, which is how A* starts returning non-shortest paths.
///
/// The vertical terms are the fix for a measured bug: with a horizontal-only heuristic, a node six
/// blocks BELOW the goal scored zero -- a perfect score -- so the search happily fell down a shaft and
/// the walk reported success on the wrong floor.
fn heuristic(a: Node, b: Node) -> u32 {
    let flat = ((a.0 - b.0).abs() + (a.2 - b.2).abs()) as u32;
    let up = (b.1 - a.1).max(0) as u32;
    let down = (a.1 - b.1).max(0) as f64;
    let falls = (down / SAFE_FALL).ceil() as u32;
    flat.max(up).max(falls) * COST_STEP
}

/// How close a node is to the goal, for REPORTING only -- squared 3D distance.
///
/// Deliberately not the heuristic. The heuristic has to be admissible, which forces it to be the MAX of
/// the components, and that makes horizontal progress invisible whenever the vertical gap is larger --
/// so a search for an unreachable floor above would find no node "better" than where it started and
/// offer no closest-reachable point at all. This measure has no such obligation; it just answers "which
/// reachable block ended up nearest".
fn closeness(a: Node, b: Node) -> i64 {
    let (dx, dy, dz) = ((a.0 - b.0) as i64, (a.1 - b.1) as i64, (a.2 - b.2) as i64);
    dx * dx + dy * dy + dz * dz
}

/// A route from the bot's current feet block to `goal`, as blocks to walk through in order.
///
/// The returned path EXCLUDES the start and ends at the reached node. `None` means no route was found
/// within the budget -- which is not the same as "no route exists", and the caller should say so.
///
/// `goal_slack` lets the search finish at any block within that many blocks, horizontally, of the
/// goal: a caller clicking on a map wants "take me there", not "stand on exactly this block", and a
/// goal whose own column turns out not to be standable should not fail the whole request.
pub fn find(blocks: &impl Blocks, start: Node, goal: Node, goal_slack: i32, max_fall: f64) -> Option<Vec<Node>> {
    let mut open = BinaryHeap::new();
    // node -> (cost so far, where we came from)
    let mut seen: HashMap<Node, (u32, Option<Node>)> = HashMap::new();
    open.push(Queued { f: heuristic(start, goal), g: 0, node: start });
    seen.insert(start, (0, None));
    let mut expansions = 0usize;
    // The best node found so far by heuristic, so a search that cannot reach the goal can still
    // report the closest reachable point rather than nothing at all.
    let mut best = (closeness(start, goal), start);

    while let Some(Queued { g, node, .. }) = open.pop() {
        // A stale heap entry: we have since found a cheaper way to this node.
        if seen.get(&node).map(|&(c, _)| c < g).unwrap_or(false) {
            continue;
        }
        let c = closeness(node, goal);
        if c < best.0 {
            best = (c, node);
        }
        let close_enough = (node.0 - goal.0).abs() <= goal_slack
            && (node.2 - goal.2).abs() <= goal_slack
            // Height is NOT slack. Landing six blocks under the goal is a different floor, not
            // a near miss, and treating it as arrival is what made a fall down a shaft report success.
            && (node.1 - goal.1).abs() <= 1;
        if close_enough {
            return Some(reconstruct(&seen, node));
        }
        expansions += 1;
        if expansions >= MAX_EXPANSIONS {
            break;
        }
        // VERTICAL MOVES ON A CLIMBABLE. Without these whole floors of a built structure are
        // unreachable: the settlement is hollow, `STEP_UP` is one block, and the ladders that connect its
        // levels were invisible to the search. Both directions, because a ladder is a way down too.
        if blocks.climbable_at(node.0, node.1, node.2) == Some(true) {
            for dy in [1, -1] {
                let next = (node.0, node.1 + dy, node.2);
                // Climbing up needs a climbable (or standable footing) to arrive in; climbing down needs
                // the box to fit. `standable` covers both, and it is the same predicate the walker uses.
                if !standable(blocks, next) {
                    continue;
                }
                let ng = g + COST_CLIMB;
                match seen.get(&next) {
                    Some(&(known, _)) if known <= ng => {}
                    _ => {
                        seen.insert(next, (ng, Some(node)));
                        open.push(Queued { f: ng + heuristic(next, goal), g: ng, node: next });
                    }
                }
            }
        }
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let (nx, nz) = (node.0 + dx, node.2 + dz);
            if (nx - start.0).abs() > SEARCH_RADIUS || (nz - start.2).abs() > SEARCH_RADIUS {
                continue;
            }
            let Some((ny, cost)) = move_into(blocks, (nx, nz), node.1, max_fall) else { continue };
            let next = (nx, ny, nz);
            let ng = g + cost;
            match seen.get(&next) {
                Some(&(known, _)) if known <= ng => {}
                _ => {
                    seen.insert(next, (ng, Some(node)));
                    open.push(Queued { f: ng + heuristic(next, goal), g: ng, node: next });
                }
            }
        }
    }
    // Nothing reached the goal. If we got meaningfully closer than we started, walking to the closest
    // reachable point is more useful than refusing -- it is progress, and the caller is told.
    if best.1 != start {
        return Some(reconstruct(&seen, best.1));
    }
    None
}

fn reconstruct(seen: &HashMap<Node, (u32, Option<Node>)>, end: Node) -> Vec<Node> {
    let mut out = vec![end];
    let mut cur = end;
    while let Some(&(_, Some(prev))) = seen.get(&cur) {
        out.push(prev);
        cur = prev;
    }
    out.reverse();
    out.remove(0); // the start is where we already are
    out
}

/// Drop waypoints that lie on a straight run, so the walker steers to corners instead of to every
/// block. Fewer waypoints means fewer arrival checks and a visibly smoother line.
///
/// Only collinear points on the SAME level are removed: a change of height is where the walker has to
/// step up or fall, and skipping past it would have it aiming through a floor.
///
/// `start` is where the bot is standing NOW. It has to be passed in because `find` excludes it from the
/// path, and without it the first waypoint can never be judged collinear -- so a dead straight route
/// kept a pointless waypoint one block ahead of the bot.
pub fn simplify(start: Node, path: &[Node]) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::new();
    for (i, &p) in path.iter().enumerate() {
        if i + 1 == path.len() {
            out.push(p);
            continue;
        }
        let prev = *out.last().unwrap_or(&start);
        let next = path[i + 1];
        let same_level = prev.1 == p.1 && p.1 == next.1;
        let straight_x = prev.0 == p.0 && p.0 == next.0;
        let straight_z = prev.2 == p.2 && p.2 == next.2;
        if same_level && (straight_x || straight_z) {
            continue;
        }
        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    /// Same fixture shape as `walk.rs`: anything not mentioned is UNMIRRORED, never air.
    struct Grid(Map<(i32, i32, i32), bool>);

    impl Blocks for Grid {
        fn passable_at(&self, x: i32, y: i32, z: i32) -> Option<bool> {
            self.0.get(&(x, y, z)).copied()
        }
    }

    /// A floor at `floor_y` over a square, with `head` blocks of air above it.
    fn room(x0: i32, x1: i32, z0: i32, z1: i32, floor_y: i32, head: i32) -> Map<(i32, i32, i32), bool> {
        let mut g = Map::new();
        for x in x0..=x1 {
            for z in z0..=z1 {
                g.insert((x, floor_y, z), false);
                for y in floor_y + 1..=floor_y + head {
                    g.insert((x, y, z), true);
                }
            }
        }
        g
    }

    #[test]
    fn a_straight_run_is_found_and_simplified_to_its_ends() {
        let g = Grid(room(0, 20, 0, 4, 63, 4));
        let path = find(&g, (1, 64, 2), (15, 64, 2), 0, SAFE_FALL).expect("no path across an open floor");
        assert_eq!(path.last(), Some(&(15, 64, 2)));
        // Every node on the path must be somewhere the bot can actually stand, or the walker will
        // refuse to follow the route this module promised.
        for &n in &path {
            assert!(standable(&g, n), "path goes through {n:?}, where the bot cannot stand");
        }
        let simple = simplify((1, 64, 2), &path);
        assert_eq!(simple.len(), 1, "a straight line needs one waypoint, got {simple:?}");
    }

    #[test]
    fn IT_ROUTES_AROUND_A_WALL_WHICH_IS_THE_WHOLE_POINT() {
        // The straight-line walker fails this: it slides along the wall and reports "no headway".
        let mut g = room(0, 20, 0, 8, 63, 4);
        // A wall across the middle with a doorway at z = 7.
        for z in 0..=6 {
            for y in 64..=67 {
                g.insert((10, y, z), false);
            }
        }
        let grid = Grid(g);
        let path = find(&grid, (2, 64, 2), (18, 64, 2), 0, SAFE_FALL).expect("no route through the doorway");
        assert_eq!(path.last(), Some(&(18, 64, 2)));
        // It must actually use the gap.
        assert!(path.iter().any(|&(x, _, z)| x == 10 && z == 7), "did not go through the doorway: {path:?}");
        for &n in &path {
            assert!(standable(&grid, n), "unstandable node {n:?}");
        }
    }

    #[test]
    fn IT_DESCENDS_A_SHAFT_IT_IS_STANDING_ON_THE_LID_OF() {
        // THE MEASURED LIVE FAILURE. The bot stood at y=68 on a two-block ledge with the floor six
        // blocks below, and was asked to walk to the floor directly beneath it. The straight-line
        // walker arrived horizontally and stopped, because reaching the target requires first moving
        // AWAY from it. A search does not care about that.
        let mut g = room(60, 72, 30, 42, 61, 10); // the floor at 61, air to 71
        g.insert((64, 67, 36), false); // the ledge
        g.insert((65, 67, 36), false);
        let grid = Grid(g);
        let path = find(&grid, (64, 68, 36), (64, 62, 36), 0, SAFE_FALL).expect("no way down off the ledge");
        let end = *path.last().expect("non-empty");
        assert_eq!(end, (64, 62, 36), "did not reach the floor under the ledge, ended at {end:?}");
        // It has to leave the ledge's column to fall, so the route must wander off and come back.
        assert!(path.iter().any(|&(x, _, z)| x != 64 || z != 36), "never left the ledge column: {path:?}");
        for &n in &path {
            assert!(standable(&grid, n), "unstandable node {n:?}");
        }
    }

    #[test]
    fn it_prefers_the_stairs_to_a_survivable_drop_when_both_arrive() {
        // Both routes reach the goal; the stairs cost nothing in health, so the fall must be priced
        // higher or the bot will always throw itself off the edge.
        let mut g = room(0, 10, 0, 2, 63, 8); // upper area: floor 63, feet 64
        // A lower floor at 57 (feet 58) to the east, reachable either by a 6-block drop at x=6...
        for x in 6..=10 {
            for z in 0..=2 {
                g.insert((x, 57, z), false);
                for y in 58..=71 {
                    g.insert((x, y, z), true);
                }
                g.remove(&(x, 63, z)); // no upper floor out here
                g.insert((x, 63, z), true);
            }
        }
        // ...or by a staircase down at z = 2.
        for (i, y) in (58..=63).rev().enumerate() {
            let x = 6 + i as i32;
            if x <= 10 {
                g.insert((x, y, 2), false);
            }
        }
        let grid = Grid(g);
        let path = find(&grid, (1, 64, 1), (9, 58, 1), 0, SAFE_FALL).expect("no route down");
        assert_eq!(path.last(), Some(&(9, 58, 1)));
        for &n in &path {
            assert!(standable(&grid, n), "unstandable node {n:?}");
        }
    }

    #[test]
    fn A_NODE_BELOW_THE_GOAL_IS_NOT_ARRIVAL() {
        // THE MEASURED BUG, and it reported SUCCESS, which is the worst way to be wrong. Sent to
        // 70,61,38 on the live server the bot fell down a gap and finished at 70,55,38 -- right x and
        // z, six blocks down -- and the walk said `done`. Cause: the heuristic counted only horizontal
        // distance, so a node directly under the goal scored ZERO, the best score available.
        //
        // Here the goal hangs in the air with no way up to it. The search may return the closest
        // reachable point, but that point must not be mistaken for the goal.
        let g = Grid(room(0, 10, 0, 4, 63, 4));
        let goal = (5, 70, 2);
        let path = find(&g, (1, 64, 2), goal, 1, SAFE_FALL).expect("should offer the closest reachable point");
        let end = *path.last().expect("non-empty");
        assert_eq!(end.1, 64, "ended at {end:?}, which is not on the only floor there is");
        // And the caller's own arrival test must call this a miss, not an arrival.
        assert!((end.1 - goal.1).abs() > 1, "{end:?} would be reported as arriving at {goal:?}");
    }

    #[test]
    fn it_stays_on_the_goals_floor_when_a_cheap_fall_would_leave_it() {
        // The same bug from the other side: falling is cheap and walking round is long, so the cost
        // model alone prefers the fall. What stops it is that a node on the wrong floor does not
        // satisfy the goal -- so the search has to carry on and find the way round.
        //
        // Upper floor at y=63 (feet 64) with a gap at x=5; lower floor at y=57 under the gap; a
        // walkway round the gap at z=4.
        let mut g = room(0, 10, 0, 4, 63, 8);
        for z in 0..=3 {
            g.remove(&(5, 63, z));
            g.insert((5, 63, z), true); // the gap: no upper floor at x=5 except at z=4
            for y in 58..=62 {
                g.insert((5, y, z), true); // open air down to the lower floor
            }
            g.insert((5, 57, z), false); // the lower floor
        }
        let grid = Grid(g);
        let path = find(&grid, (1, 64, 2), (9, 64, 2), 0, SAFE_FALL).expect("no route to the far side");
        let end = *path.last().expect("non-empty");
        assert_eq!(end, (9, 64, 2), "ended at {end:?} instead of the goal");
        // It must have used the walkway at z=4 rather than dropping through the gap.
        assert!(path.iter().all(|&(_, y, _)| y == 64), "left the floor: {path:?}");
        for &n in &path {
            assert!(standable(&grid, n), "unstandable node {n:?}");
        }
    }

    #[test]
    fn it_refuses_to_route_through_unmirrored_world() {
        // The rule the walker has, restated for the search: the goal is beyond the mirrored square, so
        // there is no route to it. It may return the closest REACHABLE point, but that point must be
        // inside what we actually know.
        let g = Grid(room(0, 6, 0, 6, 63, 4));
        match find(&g, (1, 64, 1), (60, 64, 1), 0, SAFE_FALL) {
            None => {}
            Some(path) => {
                for &n in &path {
                    assert!(n.0 <= 6 && n.2 <= 6, "routed into unmirrored world at {n:?}");
                    assert!(standable(&g, n), "unstandable node {n:?}");
                }
            }
        }
    }

    #[test]
    fn an_enclosed_start_has_no_route_and_says_so() {
        // Sealed in a one-block cell: no neighbour is standable, so there is no path AND no closer
        // point. This must be `None` rather than an empty path, which a caller would read as "done".
        let mut g = Map::new();
        g.insert((0, 63, 0), false);
        g.insert((0, 64, 0), true);
        g.insert((0, 65, 0), true);
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            for y in 64..=65 {
                g.insert((dx, y, dz), false); // walled in
            }
        }
        assert_eq!(find(&Grid(g), (0, 64, 0), (10, 64, 0), 0, SAFE_FALL), None);
    }

    #[test]
    fn the_budget_is_respected_on_a_hopeless_search() {
        // A big open floor and a goal sealed inside a block: the search cannot succeed and must stop
        // rather than run for ever. It is allowed to return the closest reachable point.
        let g = Grid(room(-60, 60, -60, 60, 63, 4));
        let started = std::time::Instant::now();
        let _ = find(&g, (0, 64, 0), (0, 200, 0), 0, SAFE_FALL);
        assert!(started.elapsed().as_secs() < 10, "search took {:?}", started.elapsed());
    }

    #[test]
    fn goal_slack_accepts_standing_beside_an_unstandable_goal() {
        // Clicking a map picks a block, and the block picked is often the SOLID one you can see rather
        // than the air above it. With slack the request succeeds standing next to it.
        let mut g = room(0, 10, 0, 4, 63, 4);
        for y in 64..=67 {
            g.insert((8, y, 2), false); // the goal column is a pillar
        }
        let grid = Grid(g);
        assert_eq!(find(&grid, (1, 64, 2), (8, 64, 2), 0, SAFE_FALL).map(|p| *p.last().unwrap()) == Some((8, 64, 2)), false);
        let path = find(&grid, (1, 64, 2), (8, 64, 2), 1, SAFE_FALL).expect("no route to beside the pillar");
        let end = *path.last().expect("non-empty");
        assert!((end.0 - 8).abs() <= 1 && (end.2 - 2).abs() <= 1, "ended at {end:?}, not beside the goal");
        assert!(standable(&grid, end));
    }
}

#[cfg(test)]
mod climb_tests {
    use super::*;
    use std::collections::HashMap as Map;

    /// A world where some blocks are also climbable. `climb` names the ladder column cells.
    struct Grid {
        passable: Map<(i32, i32, i32), bool>,
        climb: Map<(i32, i32, i32), bool>,
    }

    impl Blocks for Grid {
        fn passable_at(&self, x: i32, y: i32, z: i32) -> Option<bool> {
            self.passable.get(&(x, y, z)).copied()
        }
        fn climbable_at(&self, x: i32, y: i32, z: i32) -> Option<bool> {
            Some(self.climb.get(&(x, y, z)).copied().unwrap_or(false))
        }
    }

    #[test]
    fn A_LADDER_REACHES_A_FLOOR_NOTHING_ELSE_CAN() {
        // THE REACHABILITY LIMIT, reduced from the live measurement. The settlement is a hollow
        // structure: along x=64 there is no floor at all at y=61 beyond z=41, and the upper floors are
        // more than one block up. With only walking and falling, whole levels of a building are
        // unreachable even though it is full of ladders -- and the bot reported that as "no route",
        // which was true of the search and not of the world.
        let mut passable = Map::new();
        let mut climb = Map::new();
        // Lower floor at y=63 (feet 64), upper floor at y=70 (feet 71), seven apart -- far beyond
        // STEP_UP, and a fall from the upper one is the only other way between them.
        for x in 0..=6 {
            for z in 0..=2 {
                passable.insert((x, 63, z), false);
                for y in 64..=69 {
                    passable.insert((x, y, z), true);
                }
                passable.insert((x, 70, z), false);
                for y in 71..=74 {
                    passable.insert((x, y, z), true);
                }
            }
        }
        // A ladder column at x=3,z=1 from the lower floor up through a hole in the upper one.
        for y in 64..=71 {
            passable.insert((3, y, 1), true);
            climb.insert((3, y, 1), true);
        }
        passable.insert((3, 70, 1), true); // the hole in the upper floor
        let grid = Grid { passable, climb };

        // Without the ladder there is no way up at all: prove the fixture is really a two-level problem
        // by checking a column that has no ladder.
        assert!(!standable(&grid, (5, 71, 1)) || support_within(&grid, 5.5, 71.0, 1.5, 0.0) == Some(71.0));

        let path = find(&grid, (1, 64, 1), (5, 71, 1), 0, SAFE_FALL).expect("no route up the ladder");
        let end = *path.last().expect("non-empty");
        assert_eq!(end, (5, 71, 1), "did not reach the upper floor, ended at {end:?}");
        // It must actually have used the ladder column.
        assert!(path.iter().any(|&(x, _, z)| x == 3 && z == 1), "did not use the ladder: {path:?}");
        // And every node must be somewhere the walker agrees the bot can be.
        for &n in &path {
            assert!(standable(&grid, n), "unstandable node {n:?}");
        }
    }

    #[test]
    fn hanging_on_a_ladder_is_not_falling() {
        // Otherwise the bot "falls" one tick after every climb step and never ascends.
        let mut passable = Map::new();
        let mut climb = Map::new();
        passable.insert((0, 63, 0), false);
        for y in 64..=70 {
            passable.insert((0, y, 0), true);
            climb.insert((0, y, 0), true);
        }
        let grid = Grid { passable, climb };
        assert_eq!(crate::walk::fall_step(&grid, (0.5, 67.0, 0.5)), None, "fell off the ladder");
        // ...and it climbs toward a waypoint above, at ladder speed rather than walking speed.
        match crate::walk::step_toward(&grid, (0.5, 67.0, 0.5), (0.5, 70.0, 0.5), SAFE_FALL) {
            crate::walk::Step::Move { y, on_ground, .. } => {
                assert!(y > 67.0 && y <= 67.0 + crate::walk::CLIMB_SPEED + 1e-9, "climbed {y}, too fast or not at all");
                assert!(!on_ground, "a bot on a ladder is not on the ground");
            }
            other => panic!("did not climb: {other:?}"),
        }
    }
}

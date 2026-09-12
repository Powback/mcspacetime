//! ONE MOD'S PRIVATE WIRE FORMAT. Nothing here is protocol.
//!
//! `computercraft:monitor_client` is CC:Tweaked's own `CustomPacketPayload`. It is not in any
//! protocol table, it has no version negotiation of its own, and the mod is free to change it in
//! any release — this file was written against **cc-tweaked-1.21.1-forge-1.119.0** by
//! disassembling `MonitorClientMessage`, `TerminalState` and `NetworkedTerminal.write()` out of
//! that jar, because a mod's payload layout cannot be looked up and must not be guessed.
//!
//! Keep it OUT of the generic protocol path: `protocol.rs` only routes the channel name here, the
//! same way it would route any other mod. If the decode starts failing after a mod update, the fix
//! is to re-read the jar, not to make the generic reader more forgiving.
//!
//! ## The layout, as the mod writes it
//!
//! `MonitorClientMessage` = `BlockPos.STREAM_CODEC` then `ByteBufCodecs.optional(TerminalState)`:
//!
//! ```text
//! i64                     BlockPos.asLong() of the ORIGIN block (see below)
//! bool                    terminal present?
//!   bool                  colour        (advanced monitor)
//!   varint                width         IN CHARACTERS, not blocks
//!   varint                height        IN CHARACTERS
//!   varint                cursorX
//!   varint                cursorY
//!   bool                  cursorBlink
//!   u8                    (cursorBg << 4) | cursorFg
//!   varint len, bytes     contents, len == width*height*2 + 48
//! ```
//!
//! and `contents` is `NetworkedTerminal.write()`:
//!
//! ```text
//! for each row:  width bytes of text, then width bytes of (bg << 4) | fg
//! then           16 * 3 bytes of palette, r/g/b each round(component * 255)
//! ```
//!
//! ## Three facts that shape the table
//!
//! - **Every payload is a FULL screen.** `TerminalState.create` serialises the whole terminal;
//!   there is no delta encoding and no dirty-rectangle. So unlike entity metadata there is nothing
//!   to merge — but there is plenty to *suppress*, because a program calling `term.write` in a loop
//!   re-sends an identical or near-identical screen every tick.
//! - **Only the ORIGIN block is ever addressed.** `MonitorWatcher.getMonitor` returns null unless
//!   `xIndex == 0 && yIndex == 0`, so a 3x4 panel produces messages for exactly one block.
//!   `MonitorBlockEntity.toWorldPos` walks `getRight()` and `getDown()` from it, so index (0,0) is
//!   the **top-left as seen facing the screen** — which is the corner the viewer asks for.
//! - **Colour is PER CHARACTER**, two 4-bit palette indices per cell, plus a per-monitor 16-entry
//!   RGB palette a program can redefine with `term.setPaletteColour`. A consumer holding one
//!   foreground and one background colour for a whole screen cannot express this, so the full
//!   per-cell grid is published and the flattening is left to whoever renders it.
//!
//! An absent terminal (`Optional.empty`) is a real message: it is what the server sends for a
//! monitor with no computer attached, and for one whose chunk is being watched before its
//! `ServerMonitor` exists. It means "this monitor has no screen", not "nothing changed".

use crate::wire::Reader;
use anyhow::{bail, Result};

pub const MONITOR_CHANNEL: &str = "computercraft:monitor_client";

/// CC's palette indices are written as these characters in Lua (`colours.lightGrey` is `'8'`), and
/// the terminal's own colour buffers store them that way, so the published grids use them too.
const HEX: [u8; 16] = *b"0123456789abcdef";

pub struct Terminal {
    /// characters across, NOT blocks
    pub width: u32,
    /// characters down, NOT blocks
    pub height: u32,
    /// advanced (colour) monitor; a basic monitor is greyscale but still uses the 16-slot palette
    pub colour: bool,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub cursor_blink: bool,
    pub cursor_fg: u8,
    pub cursor_bg: u8,
    /// `height` strings of `width` characters
    pub lines: Vec<String>,
    /// `height` strings of `width` hex digits: the foreground palette index of each cell
    pub fg: Vec<String>,
    /// same shape, background palette index
    pub bg: Vec<String>,
    /// 16 `#rrggbb` indexed DIRECTLY by the hex digits above (`palette[0]` is what a `0` cell
    /// renders as). The mod sends them in the opposite order; see `decode`.
    pub palette: Vec<String>,
}

pub struct MonitorMessage {
    /// origin block = top-left of the panel as seen facing the screen
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// `None` when the mod sent `Optional.empty`: the monitor exists but has no terminal.
    pub terminal: Option<Terminal>,
}

/// One byte of CC terminal text as a `char`.
///
/// CC:T's terminal is byte-oriented: `NetworkedTerminal.write` stores `charAt(i) & 0xff`, and CC's
/// font gives all 256 byte values a glyph — 0x20..0x7e are ASCII, the rest are CC's own drawing and
/// teletext characters that `paintutils` and `window` borders use. Mapping byte to the identical
/// Unicode code point (Latin-1) is therefore **lossless**: a consumer that wants the CC glyph index
/// back takes `codePointAt(i) & 0xff`. NUL is the one exception — a freshly resized terminal is
/// full of it and it means blank, so it becomes a space rather than an unprintable in every row.
fn cc_char(b: u8) -> char {
    if b == 0 { ' ' } else { b as char }
}

pub fn decode(payload: &[u8]) -> Result<MonitorMessage> {
    let mut r = Reader::new(payload);
    let (x, y, z) = r.block_pos()?;
    if !r.bool()? {
        return Ok(MonitorMessage { x, y, z, terminal: None });
    }
    let colour = r.bool()?;
    let width = r.varint()?;
    let height = r.varint()?;
    let cursor_x = r.varint()?;
    let cursor_y = r.varint()?;
    let cursor_blink = r.bool()?;
    let cursor = r.u8()?;
    let contents = r.byte_array()?;

    // A screen is at most 8x6 blocks and CC caps a monitor at 164x81 characters; anything wildly
    // outside that is a desynchronised read, and allocating from it would be the bug rather than
    // the symptom. Checked before the length arithmetic so a hostile/garbled varint cannot overflow.
    if !(1..=1024).contains(&width) || !(1..=1024).contains(&height) {
        bail!("implausible terminal size {width}x{height}");
    }
    let want = width as usize * height as usize * 2 + 48;
    if contents.len() != want {
        bail!("contents {} bytes, expected {want} for {width}x{height}", contents.len());
    }

    let w = width as usize;
    let mut lines = Vec::with_capacity(height as usize);
    let mut fg = Vec::with_capacity(height as usize);
    let mut bg = Vec::with_capacity(height as usize);
    let mut off = 0usize;
    for _ in 0..height {
        lines.push(contents[off..off + w].iter().map(|&b| cc_char(b)).collect());
        off += w;
        let mut f = String::with_capacity(w);
        let mut b = String::with_capacity(w);
        for &c in &contents[off..off + w] {
            b.push(HEX[(c >> 4) as usize] as char);
            f.push(HEX[(c & 0xf) as usize] as char);
        }
        off += w;
        fg.push(f);
        bg.push(b);
    }
    // THE PALETTE IS SENT IN THE OPPOSITE ORDER TO THE COLOUR DIGITS, and nothing on the wire says
    // so. The 16 entries are `Colour.values()` order (BLACK first, WHITE last), while the per-cell
    // digits are CC's Lua colour indices (`colours.white` is 0, `colours.black` is 15) -- so the
    // mod's own renderer looks a cell up as `palette[15 - digit]`
    // (`FixedWidthFontRenderer.getColour`). A default screen makes the mistake obvious and easy to
    // miss at the same time: it is fg `0` on bg `f`, which reads as black-on-white if you index
    // straight and is actually white-on-black.
    //
    // Reversed here, once, so that the published `palette[parseInt(digit, 16)]` is simply correct
    // and no consumer has to know this.
    let mut palette: Vec<String> = contents[off..]
        .chunks_exact(3)
        .map(|c| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]))
        .collect();
    palette.reverse();

    Ok(MonitorMessage {
        x,
        y,
        z,
        terminal: Some(Terminal {
            width: width as u32,
            height: height as u32,
            colour,
            cursor_x,
            cursor_y,
            cursor_blink,
            cursor_fg: cursor & 0xf,
            cursor_bg: cursor >> 4,
            lines,
            fg,
            bg,
            palette,
        }),
    })
}

/// Panel size in BLOCKS, lifted out of a monitor block entity's update tag.
///
/// The screen payload carries the terminal size in characters and says nothing at all about blocks,
/// and the two are related by the monitor's text scale, which is not on the wire either. But
/// `MonitorBlockEntity.getUpdateTag` puts `XIndex`, `YIndex`, `Width` and `Height` on every monitor
/// block entity, and the mirror already receives those with the chunk -- so the geometry costs
/// nothing but remembering it.
///
/// Returns `None` for anything that is not the ORIGIN block of a panel (`XIndex == 0 &&
/// YIndex == 0`), because that is the only block a screen payload ever names.
pub fn monitor_panel_size(type_name: &str, nbt_json: &str) -> Option<(u32, u32)> {
    if !type_name.starts_with("computercraft:monitor") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(nbt_json).ok()?;
    if v.get("XIndex")?.as_i64()? != 0 || v.get("YIndex")?.as_i64()? != 0 {
        return None;
    }
    let w = v.get("Width")?.as_i64()?;
    let h = v.get("Height")?.as_i64()?;
    if !(1..=64).contains(&w) || !(1..=64).contains(&h) {
        return None;
    }
    Some((w as u32, h as u32))
}

/// The `facing` property out of a block-state property string ("facing=north,orientation=north").
///
/// A CC monitor has TWO orientation properties: `facing`, the horizontal direction it was placed
/// against, and `orientation`, which is `north` for an upright screen and `up`/`down` for one
/// angled at the ceiling or floor. Only `facing` is published, because that is the field the
/// consumer has; the full state is in `chunk_section` for anyone who needs the tilt.
pub fn facing_of(props: &str) -> Option<String> {
    props.split(',').find_map(|kv| kv.strip_prefix("facing=")).map(|v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Writer;

    /// Build a payload the way the MOD does, not the way `decode` does: this mirrors
    /// `TerminalState.write` + `NetworkedTerminal.write` from the jar, so the test fails if the
    /// decoder drifts from the layout it was written against.
    fn encode(x: i32, y: i32, z: i32, w: usize, h: usize, cells: &[(u8, u8, u8)], palette: &[[u8; 3]; 16]) -> Vec<u8> {
        let mut out = Writer::new();
        let packed = (((x as i64) & 0x3FF_FFFF) << 38) | (((z as i64) & 0x3FF_FFFF) << 12) | ((y as i64) & 0xFFF);
        out.i64(packed);
        out.bool(true); // Optional present
        out.bool(true); // colour
        out.varint(w as i32);
        out.varint(h as i32);
        out.varint(3);
        out.varint(4);
        out.bool(true);
        out.u8((5 << 4) | 9); // cursor bg 5, fg 9
        let mut contents = Vec::new();
        for row in 0..h {
            for col in 0..w {
                contents.push(cells[row * w + col].0);
            }
            for col in 0..w {
                let (_, f, b) = cells[row * w + col];
                contents.push((b << 4) | f);
            }
        }
        for p in palette {
            contents.extend_from_slice(p);
        }
        out.byte_array(&contents);
        out.into_inner()
    }

    #[test]
    fn decodes_a_screen_the_mod_would_have_sent() {
        let (w, h) = (4usize, 2usize);
        // "ab\0 " / "cd  ", fg/bg varying per cell so a flattened decode cannot pass
        let cells: Vec<(u8, u8, u8)> = vec![
            (b'a', 0, 15), (b'b', 1, 14), (0, 2, 13), (b' ', 3, 12),
            (b'c', 4, 11), (b'd', 5, 10), (b' ', 6, 9), (b' ', 7, 8),
        ];
        // as the MOD orders them: Colour.values(), black first, white last
        let mut pal = [[0u8; 3]; 16];
        pal[0] = [0x11, 0x11, 0x11];
        pal[15] = [0xf0, 0xf0, 0xf0];
        pal[6] = [0x33, 0xbb, 0xff];
        let payload = encode(-1200, 71, 340, w, h, &cells, &pal);

        let m = decode(&payload).expect("decode");
        assert_eq!((m.x, m.y, m.z), (-1200, 71, 340));
        let t = m.terminal.expect("terminal present");
        assert_eq!((t.width, t.height), (4, 2));
        assert!(t.colour);
        // NUL renders as a space, everything else as itself
        assert_eq!(t.lines, vec!["ab  ".to_string(), "cd  ".to_string()]);
        assert_eq!(t.fg, vec!["0123".to_string(), "4567".to_string()]);
        assert_eq!(t.bg, vec!["fedc".to_string(), "ba98".to_string()]);
        // ...and reversed on the way out, so a cell whose digit is `0` is white, as CC's
        // `colours.white == 0` says it must be. Indexing the mod's array straight would give black.
        assert_eq!(t.palette[0], "#f0f0f0");
        assert_eq!(t.palette[15], "#111111");
        assert_eq!(t.palette[9], "#33bbff");
        assert_eq!((t.cursor_x, t.cursor_y), (3, 4));
        assert!(t.cursor_blink);
        assert_eq!((t.cursor_fg, t.cursor_bg), (9, 5));
    }

    #[test]
    fn an_empty_optional_is_a_monitor_with_no_screen() {
        let mut out = Writer::new();
        out.i64((((64i64) & 0x3FF_FFFF) << 38) | (((32i64) & 0x3FF_FFFF) << 12) | (66i64 & 0xFFF));
        out.bool(false);
        let m = decode(&out.into_inner()).expect("decode");
        assert_eq!((m.x, m.y, m.z), (64, 66, 32));
        assert!(m.terminal.is_none());
    }

    /// A REAL payload, captured off the wire (`MCST_DUMP_PAYLOAD=computercraft:monitor_client`)
    /// from the monitor at 71,67,33 on the dev replica. The synthetic test above only proves the
    /// decoder agrees with how this file *thinks* the mod encodes; this one proves it agrees with
    /// what the mod actually sent, which is the claim that matters.
    #[test]
    fn decodes_a_payload_captured_from_the_server() {
        let payload = include_bytes!("../testdata/monitor_client_71_67_33.bin");
        let m = decode(payload).expect("decode");
        assert_eq!((m.x, m.y, m.z), (71, 67, 33));
        let t = m.terminal.expect("terminal present");
        // 3x4 blocks at text scale 0.5
        assert_eq!((t.width, t.height), (57, 52));
        assert!(t.colour);
        assert_eq!(t.lines.len(), 52);
        assert!(t.lines.iter().all(|l| l.chars().count() == 57));
        assert_eq!(t.lines[0].trim_end(), "DockingMan!");
        assert_eq!(t.lines[1].trim_end(), "[1] tower - [4/4] -  (64, 68, 32)");
        // every grid is the same shape as the text
        assert_eq!(t.fg.len(), 52);
        assert_eq!(t.bg.len(), 52);
        assert!(t.fg.iter().chain(t.bg.iter()).all(|l| l.len() == 57));
        // A default CC screen is white on black. If the palette were published in the mod's own
        // order this would read as black text on a white screen -- see the reversal in `decode`.
        assert_eq!(&t.fg[0][..11], "00000000000");
        assert_eq!(&t.bg[0][..11], "fffffffffff");
        assert_eq!(t.palette[0], "#f0f0f0", "digit 0 is colours.white");
        assert_eq!(t.palette[15], "#111111", "digit f is colours.black");
    }

    /// A truncated or mis-sized `contents` must be an error, not a panic and not a half-screen:
    /// the length is the only check that catches the wire format changing under us.
    #[test]
    fn a_contents_length_that_does_not_match_the_size_is_rejected() {
        let (w, h) = (4usize, 2usize);
        let cells: Vec<(u8, u8, u8)> = vec![(b'x', 0, 0); w * h];
        let pal = [[0u8; 3]; 16];
        let mut payload = encode(0, 0, 0, w, h, &cells, &pal);
        // lie about the height: contents now describes 2 rows but the header claims 3
        // (header bytes: 8 pos + 1 optional + 1 colour + 1 width + 1 height)
        payload[11] = 3;
        assert!(decode(&payload).is_err());
    }
}

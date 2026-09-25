//! Writes a data stream of JSON messages as NUT, for fixtures and for poking
//! at by hand.
//!
//!     cargo run --example json_nut < messages.txt > messages.nut
//!
//! Each line of stdin is one message: its pts in microseconds, a tab, and
//! the JSON object. Blank lines are skipped.

use std::io::{self, BufRead, Write};

use ffrwd_nut::{Muxer, Packet, Stream, TimeBase};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let micros = TimeBase {
        num: 1,
        den: 1_000_000,
    };
    let mut out = Vec::new();
    {
        let mut muxer = Muxer::new(&mut out, &Stream::json(micros))?;
        for line in io::stdin().lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let (pts, message) = line
                .split_once('\t')
                .ok_or("each line is: pts in microseconds, a tab, the JSON object")?;
            let pts: i64 = pts.trim().parse()?;
            let packet = Packet {
                pts,
                dts: Some(pts),
                keyframe: true,
            };
            muxer.write_coded(&packet, message.as_bytes())?;
        }
        muxer.finish()?;
    }
    io::stdout().write_all(&out)?;
    Ok(())
}

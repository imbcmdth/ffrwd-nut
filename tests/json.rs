//! A data stream of JSON messages as the one stream of a wire: written here,
//! read back here, and carried through real ffmpeg.
//!
//! The ffmpeg half needs ffmpeg on PATH and skips, saying so, where it is not.

use std::io::Write;
use std::process::{Command, Stdio};

use ffrwd_nut::{Demuxer, Muxer, Packet, Stream, TimeBase, DATA_CLASS, JSON_FOURCC};

/// Microseconds, the time base a module writing messages would pick.
const MICROS: TimeBase = TimeBase {
    num: 1,
    den: 1_000_000,
};

/// Messages as sparse as a deal stream's: seconds apart, one a whole minute
/// after the one before, and one at an epoch-sized programme time.
fn messages() -> Vec<(i64, String)> {
    vec![
        (0, r#"{"kind":"break","id":1,"start_pts":25.0}"#.to_string()),
        (
            500_000,
            r#"{"kind":"award","break":1,"node":"root"}"#.to_string(),
        ),
        (
            61_500_000,
            r#"{"kind":"break","id":2,"start_pts":90.0}"#.to_string(),
        ),
        (
            1_790_221_773_000_000,
            r#"{"kind":"control","op":"break_now"}"#.to_string(),
        ),
    ]
}

fn wire(messages: &[(i64, String)]) -> Vec<u8> {
    let mut wire = Vec::new();
    {
        let mut muxer = Muxer::new(&mut wire, &Stream::json(MICROS)).expect("write the header");
        for (pts, message) in messages {
            // A message is carried as a coded packet, every one a keyframe:
            // any of them can be read without the ones before it.
            let packet = Packet {
                pts: *pts,
                dts: Some(*pts),
                keyframe: true,
            };
            muxer
                .write_coded(&packet, message.as_bytes())
                .expect("write a message");
        }
        muxer.finish().expect("finish the stream");
    }
    wire
}

fn read(wire: &[u8]) -> (Stream, Vec<(i64, String)>) {
    let mut demuxer = Demuxer::open(wire).expect("read the header");
    let stream = demuxer.stream().clone();
    let mut out = Vec::new();
    let mut buf = Vec::new();
    while let Some(packet) = demuxer.read_packet(&mut buf).expect("read a message") {
        assert!(packet.keyframe, "every message is a keyframe");
        out.push((
            packet.pts,
            String::from_utf8(buf.clone()).expect("a message is UTF-8"),
        ));
    }
    (stream, out)
}

#[test]
fn a_json_stream_round_trips_with_its_timestamps() {
    let sent = messages();
    let (stream, got) = read(&wire(&sent));
    assert!(stream.is_json(), "the stream reads back as JSON messages");
    assert_eq!(stream.class(), DATA_CLASS);
    assert_eq!(stream.fourcc, JSON_FOURCC);
    assert_eq!(stream.codec_name(), Some("json"));
    assert_eq!(stream.kind(), "data");
    assert_eq!(stream.time_base, MICROS);
    assert_eq!(got, sent, "every message, its bytes and its pts, as sent");
}

#[test]
fn a_data_stream_of_another_codec_is_still_refused_by_name() {
    let mut other = Stream::json(MICROS);
    other.fourcc = b"KLVA".to_vec();
    let mut wire = Vec::new();
    {
        let mut muxer = Muxer::new(&mut wire, &other).expect("write the header");
        muxer
            .write_frame(0, b"x")
            .expect("a codec this crate does not name is written raw");
        muxer.finish().expect("finish");
    }
    let err = Demuxer::open(wire.as_slice())
        .err()
        .expect("a data stream this wire does not carry is refused");
    let message = err.to_string();
    assert!(
        message.contains("KLVA") && message.contains("JSON"),
        "the refusal names the codec it got and the one it carries: {message}"
    );
}

fn ffmpeg_on_path() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn ffmpeg_copies_a_json_stream_and_keeps_its_clock() {
    if !ffmpeg_on_path() {
        eprintln!("SKIPPED: ffmpeg is not on PATH.");
        return;
    }
    // A process that only maps the stream, which is how a data edge crosses
    // an ffmpeg: -copyts keeps the programme clock, where ffmpeg would
    // otherwise rebase the first message to zero.
    let sent = messages();
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-copyts",
            "-f",
            "nut",
            "-i",
            "-",
            "-map",
            "0",
            "-c",
            "copy",
            "-f",
            "nut",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg");
    let mut stdin = child.stdin.take().expect("ffmpeg stdin");
    let input = wire(&sent);
    let feeder = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output().expect("wait for ffmpeg");
    feeder
        .join()
        .expect("the feeder thread")
        .expect("write the stream to ffmpeg");
    assert!(
        output.status.success(),
        "ffmpeg exited with {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let (stream, got) = read(&output.stdout);
    assert!(stream.is_json(), "ffmpeg wrote the tag back as it read it");
    assert_eq!(
        got.iter().map(|(_, m)| m.clone()).collect::<Vec<_>>(),
        sent.iter().map(|(_, m)| m.clone()).collect::<Vec<_>>(),
        "every message, byte for byte"
    );
    let seconds = |pts: i64, base: TimeBase| pts as f64 * base.num as f64 / base.den as f64;
    for ((pts, _), (sent_pts, _)) in got.iter().zip(&sent) {
        let (a, b) = (seconds(*pts, stream.time_base), seconds(*sent_pts, MICROS));
        assert!((a - b).abs() < 1e-6, "a message at {b}s came back at {a}s");
    }
}

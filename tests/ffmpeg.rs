//! What real ffmpeg makes of these wires, in both directions: a stream this
//! muxer wrote, demuxed by ffmpeg's own NUT reader, and streams ffmpeg muxed,
//! read by this one.
//!
//! ffmpeg must be on PATH for these to do anything. Where it is not they skip
//! and say so, since a machine without ffmpeg can still be one where the rest
//! of the crate is worth testing.

use std::io::Write;
use std::process::{Command, Stdio};

use ffrwd_nut::{Demuxer, Media, Muxer, Packet, Stream, TimeBase};

/// One encoded h264 stream ffmpeg wrote, B-frames and all.
const H264: &[u8] = include_bytes!("h264.nut");

/// Whether ffmpeg is on PATH, so the tests that shell out to it can skip
/// rather than fail where it is not installed.
fn ffmpeg_on_path() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Says why a test did nothing, since a skip is otherwise silent.
fn announce_skip(what: &str) {
    eprintln!("SKIPPED: {what}. Install ffmpeg and put it on PATH to run this test.");
}

/// Hands `wire` to ffmpeg's NUT demuxer and fails if it will not read it.
fn ffmpeg_reads(wire: &[u8]) {
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "nut",
            "-i",
            "-",
            "-c",
            "copy",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg");
    child
        .stdin
        .take()
        .expect("ffmpeg stdin")
        .write_all(wire)
        .expect("write the NUT stream to ffmpeg's stdin");
    let output = child.wait_with_output().expect("wait for ffmpeg");
    assert!(
        output.status.success(),
        "ffmpeg exited with {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.is_empty(), "ffmpeg complained:\n{stderr}");
}

/// Every packet of the encoded fixture, as the reader hands them out.
fn read_fixture() -> Vec<(Packet, Vec<u8>)> {
    let mut demuxer = Demuxer::open(H264).expect("read the fixture's NUT headers");
    let mut packets = Vec::new();
    let mut buf = Vec::new();
    while let Some(packet) = demuxer.read_packet(&mut buf).expect("read a NUT packet") {
        packets.push((packet, buf.clone()));
    }
    packets
}

#[test]
fn real_ffmpeg_accepts_a_stream_write_coded_wrote() {
    // The other tests prove this crate's own demuxer reads back what
    // write_coded writes; this one proves real ffmpeg's NUT demuxer does too,
    // over packets ffmpeg itself encoded and this reader parsed off the
    // fixture - not synthetic bytes.
    if !ffmpeg_on_path() {
        announce_skip("real ffmpeg cannot demux a write_coded stream");
        return;
    }

    // Read order off a NUT stream is decode order, which is exactly the order
    // write_coded wants them handed back in. A prefix that spans both
    // keyframes carries the mid-GOP B-frame reordering as well as the cut
    // from one GOP to the next.
    let packets: Vec<(Packet, Vec<u8>)> = read_fixture().into_iter().take(35).collect();
    let stream = Demuxer::open(H264)
        .expect("read the fixture's NUT headers")
        .stream()
        .clone();

    let mut wire = Vec::new();
    {
        let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
        for (packet, data) in &packets {
            muxer.write_coded(packet, data).expect("write coded packet");
        }
        muxer.finish().expect("finish");
    }
    ffmpeg_reads(&wire);
}

#[test]
fn real_ffmpeg_accepts_a_raw_stream_the_muxer_wrote() {
    if !ffmpeg_on_path() {
        announce_skip("real ffmpeg cannot demux a raw frame stream");
        return;
    }
    let stream =
        Stream::video("yuv420p", 32, 24, TimeBase { num: 1, den: 25 }).expect("yuv420p is carried");
    let mut wire = Vec::new();
    {
        let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
        for index in 0..10i64 {
            let frame = vec![index as u8; 32 * 24 * 3 / 2];
            muxer.write_frame(index, &frame).expect("write frame");
        }
        muxer.finish().expect("finish");
    }
    ffmpeg_reads(&wire);
}

#[test]
fn a_wire_ffmpeg_wrote_is_read_and_written_back_for_ffmpeg_to_read() {
    // The hop a sidecar makes: read a stream ffmpeg muxed, write the output
    // header from the input's, put the packets back on the wire. Nothing the
    // input declared may be lost on the way, and ffmpeg has to accept what
    // comes out.
    if !ffmpeg_on_path() {
        announce_skip("real ffmpeg cannot demux a stream read off its own");
        return;
    }
    let packets = read_fixture();
    let stream = Demuxer::open(H264)
        .expect("read the headers")
        .stream()
        .clone();

    let mut wire = Vec::new();
    {
        let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
        for (packet, data) in &packets {
            muxer.write_coded(packet, data).expect("write coded packet");
        }
        muxer.finish().expect("finish");
    }
    ffmpeg_reads(&wire);

    let read_again = {
        let mut demuxer = Demuxer::open(&wire[..]).expect("read the rewritten headers");
        assert_eq!(demuxer.stream().extradata, stream.extradata);
        assert_eq!(demuxer.stream().decode_delay, stream.decode_delay);
        assert_eq!(demuxer.stream().time_base, stream.time_base);
        assert_eq!(demuxer.stream().media, stream.media);
        let mut out = Vec::new();
        let mut buf = Vec::new();
        while let Some(packet) = demuxer.read_packet(&mut buf).expect("read a packet") {
            out.push((packet, buf.clone()));
        }
        out
    };
    assert_eq!(read_again, packets, "the hop changed a packet");
}

#[test]
fn an_aac_stream_ffmpeg_copied_off_adts_gets_its_config_derived() {
    // The field case, against real ffmpeg output rather than synthetic bytes:
    // AAC in ADTS, `-c copy`'d into NUT the way a compiler does. ADTS states
    // its config on every frame instead of once in the container, so the
    // copied stream has no extradata at all and every packet still wears its
    // own header - the demuxer must derive the AudioSpecificConfig from that
    // header and strip it, or a consumer never sees a usable stream.
    if !ffmpeg_on_path() {
        announce_skip("real ffmpeg cannot write an ADTS stream to copy");
        return;
    }
    let adts = std::env::temp_dir().join(format!("ffrwd_nut_adts_{}.aac", std::process::id()));
    let nut = std::env::temp_dir().join(format!("ffrwd_nut_adts_{}.nut", std::process::id()));

    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000:duration=0.5",
        "-c:a",
        "aac",
        "-f",
        "adts",
        adts.to_str().expect("a UTF-8 path"),
    ]);
    run_ffmpeg(&[
        "-f",
        "aac",
        "-i",
        adts.to_str().expect("a UTF-8 path"),
        "-c",
        "copy",
        "-f",
        "nut",
        nut.to_str().expect("a UTF-8 path"),
    ]);
    let wire = std::fs::read(&nut).expect("read the generated NUT");
    std::fs::remove_file(&adts).ok();
    std::fs::remove_file(&nut).ok();

    let mut demuxer = Demuxer::open(&wire[..]).expect("read the generated NUT headers");
    assert_eq!(demuxer.stream().codec_name(), Some("aac"));
    assert_eq!(
        demuxer.stream().media,
        Media::Audio {
            sample_rate: 48000,
            channels: 1
        }
    );
    // 48 kHz mono AAC-LC: object type 2, frequency index 3, one channel.
    assert_eq!(
        demuxer.stream().extradata,
        vec![0x11, 0x88],
        "the AudioSpecificConfig derived from the first packet's ADTS header"
    );
    let mut buf = Vec::new();
    let mut packets = 0;
    while demuxer
        .read_packet(&mut buf)
        .expect("read a packet")
        .is_some()
    {
        packets += 1;
        assert!(
            !(buf.first() == Some(&0xFF) && buf.get(1).is_some_and(|b| b & 0xF0 == 0xF0)),
            "packet {packets} still starts with the ADTS syncword"
        );
    }
    assert!(packets > 0, "half a second of sine has packets in it");
}

/// Runs ffmpeg over `args`, failing with its own account of what went wrong.
fn run_ffmpeg(args: &[&str]) {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        output.status.success(),
        "ffmpeg {args:?} exited with {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

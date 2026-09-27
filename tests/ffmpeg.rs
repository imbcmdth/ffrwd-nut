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
    on_path("ffmpeg")
}

/// Whether `tool` runs, answering `-version`.
fn on_path(tool: &str) -> bool {
    Command::new(tool)
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

#[test]
fn a_stream_that_opens_before_zero_is_read_with_its_negative_clock() {
    // Reordered pictures whose first pts is 0 have a first dts before it, and
    // `-avoid_negative_ts disabled` keeps it there. ffmpeg's writer casts the
    // signed syncpoint timestamp to unsigned on its way out, so the wire
    // carries a two's complement pattern that an unsigned reader takes for
    // about 1.8e19 ticks and refuses. ffprobe reads such a file; so does this.
    if !ffmpeg_on_path() {
        announce_skip("real ffmpeg cannot write a stream that opens before zero");
        return;
    }
    let mkv = std::env::temp_dir().join(format!("ffrwd_nut_neg_{}.mkv", std::process::id()));
    let nut = std::env::temp_dir().join(format!("ffrwd_nut_neg_{}.nut", std::process::id()));
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x120:rate=15:duration=2",
        "-c:v",
        "libx264",
        "-bf",
        "2",
        "-g",
        "15",
        "-pix_fmt",
        "yuv420p",
        mkv.to_str().expect("a UTF-8 path"),
    ]);
    run_ffmpeg(&[
        "-copyts",
        "-i",
        mkv.to_str().expect("a UTF-8 path"),
        "-map",
        "0:v:0",
        "-c",
        "copy",
        "-avoid_negative_ts",
        "disabled",
        "-f",
        "nut",
        nut.to_str().expect("a UTF-8 path"),
    ]);
    let wire = std::fs::read(&nut).expect("read the generated NUT");
    let _ = std::fs::remove_file(&mkv);
    let _ = std::fs::remove_file(&nut);

    let mut demuxer = Demuxer::open(&wire[..]).expect("read the NUT headers");
    let mut buf = Vec::new();
    let mut packets = Vec::new();
    while let Some(packet) = demuxer.read_packet(&mut buf).expect("read a NUT packet") {
        packets.push(packet);
    }
    assert_eq!(packets.len(), 30, "two seconds at fifteen frames a second");
    assert!(
        packets.iter().all(|packet| packet.pts >= 0),
        "no picture is shown before zero"
    );
    assert_eq!(
        packets.iter().map(|packet| packet.pts).min(),
        Some(0),
        "the first picture shown is at zero"
    );
    let mut shown: Vec<i64> = packets.iter().map(|packet| packet.pts).collect();
    shown.sort_unstable();
    shown.dedup();
    assert_eq!(shown.len(), 30, "every picture has a time of its own");
}

#[test]
fn a_codec_ffmpeg_has_no_name_for_crosses_its_nut_demuxer_and_muxer_untouched() {
    // A stream under a tag no table names, as an encoder module in a codec
    // package writes it: `PYRW`, a few packets of opaque bytes, keyframes
    // where the encoder says. ffmpeg has no decoder for the tag and needs
    // none to copy it, which is what decides whether an ffmpeg may sit
    // between that encoder and the output file.
    //
    // What ffmpeg 9.0.1 did, for the record: its NUT demuxer logs `Unknown
    // codec tag '0x57525950' for stream number 0` and `read_timestamp
    // failed.` at error level and carries on; ffprobe reports
    // codec_name=unknown, codec_tag_string=PYRW, the geometry, the time
    // base and the extradata size. `-c copy -f nut` carries every packet,
    // keyframe flag and the extradata through, and restates the time base
    // as 1/61440 (the input's, doubled until it passes 48000), so the pts
    // come back as the same instants in other ticks. `-c copy` to mkv
    // logs `codec none is not supported by this format` and writes it
    // anyway, as V_MS/VFW/FOURCC with the tag in a BITMAPINFOHEADER and
    // timestamps rounded to milliseconds; to mp4 it fails with `Could not
    // find tag for codec none in stream #0, codec not currently supported in
    // container`. The last two are printed rather than asserted: what an
    // ffmpeg build makes of them is its business, not this crate's.
    if !ffmpeg_on_path() || !on_path("ffprobe") {
        announce_skip("real ffmpeg and ffprobe cannot copy a stream under an unnamed tag");
        return;
    }
    let dir = std::env::temp_dir();
    let name = |ext: &str| dir.join(format!("ffrwd_nut_pyrw_{}.{ext}", std::process::id()));
    let (x, y, mkv, mp4) = (name("x.nut"), name("y.nut"), name("mkv"), name("mp4"));
    let path = |p: &std::path::PathBuf| p.to_str().expect("a UTF-8 path").to_string();

    let mut stream = Stream::coded_fourcc(
        "video",
        b"PYRW",
        (64, 48),
        TimeBase { num: 1, den: 30 },
        vec![1, 2, 3, 4, 5, 6, 7, 8],
        0,
    )
    .expect("PYRW is four printable bytes");
    stream.frame_rate = Some((30, 1));
    let packets: Vec<(Packet, Vec<u8>)> = (0..6i64)
        .map(|index| {
            let packet = Packet {
                pts: index,
                dts: Some(index),
                keyframe: index % 3 == 0,
            };
            (packet, vec![0xA0 + index as u8; 100 + index as usize])
        })
        .collect();
    let mut wire = Vec::new();
    {
        let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
        for (packet, data) in &packets {
            muxer.write_coded(packet, data).expect("write coded packet");
        }
        muxer.finish().expect("finish");
    }
    std::fs::write(&x, &wire).expect("write the NUT file");

    // (a) What ffprobe makes of the header.
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,codec_tag_string,width,height,time_base,extradata_size",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(&x)
        .output()
        .expect("spawn ffprobe");
    assert!(
        probe.status.success(),
        "ffprobe exited with {:?}",
        probe.status.code()
    );
    let fields = String::from_utf8_lossy(&probe.stdout);
    eprintln!("ffprobe:\n{fields}");
    for expected in [
        "codec_tag_string=PYRW",
        "width=64",
        "height=48",
        "time_base=1/30",
        "extradata_size=8",
    ] {
        assert!(
            fields.lines().any(|line| line == expected),
            "ffprobe did not say {expected}:\n{fields}"
        );
    }

    // (b) NUT to NUT, read back by this crate.
    let copied = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&x)
        .args(["-c", "copy", "-f", "nut"])
        .arg(&y)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        copied.status.success(),
        "ffmpeg NUT to NUT exited with {:?}\nstderr:\n{}",
        copied.status.code(),
        String::from_utf8_lossy(&copied.stderr)
    );
    let copy = std::fs::read(&y).expect("read ffmpeg's NUT");
    let mut demuxer = Demuxer::open(&copy[..]).expect("read ffmpeg's NUT headers");
    let got = demuxer.stream().clone();
    assert_eq!(got.codec_name(), Some("PYRW"));
    assert_eq!(got.extradata, stream.extradata);
    assert_eq!(got.media, stream.media);
    assert_eq!(got.decode_delay, 0);
    assert_eq!(got.frame_rate, Some((30, 1)));
    let mut buf = Vec::new();
    let mut read = Vec::new();
    while let Some(packet) = demuxer.read_packet(&mut buf).expect("read a packet") {
        read.push((packet, buf.clone()));
    }
    assert_eq!(read.len(), packets.len());
    let (from, to) = (stream.time_base, got.time_base);
    for (index, ((sent, data), (came, bytes))) in packets.iter().zip(&read).enumerate() {
        assert_eq!(bytes, data, "packet {index} bytes");
        assert_eq!(came.keyframe, sent.keyframe, "packet {index} keyframe");
        // The same instant, in whichever time base ffmpeg chose.
        assert_eq!(
            i128::from(came.pts) * i128::from(to.num) * i128::from(from.den),
            i128::from(sent.pts) * i128::from(from.num) * i128::from(to.den),
            "packet {index} pts {} in {to:?} against {} in {from:?}",
            came.pts,
            sent.pts
        );
    }

    // (c) mkv and mp4, recorded rather than asserted.
    for out in [&mkv, &mp4] {
        let result = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "warning", "-y", "-i"])
            .arg(&x)
            .args(["-c", "copy"])
            .arg(out)
            .output()
            .expect("spawn ffmpeg");
        eprintln!(
            "ffmpeg -c copy {}: exit {:?}\n{}",
            path(out),
            result.status.code(),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    for file in [&x, &y, &mkv, &mp4] {
        let _ = std::fs::remove_file(file);
    }
}

/// A frame's size in bytes at `width` by `height`.
type FrameLen = fn(usize, usize) -> usize;

/// The planar formats ffmpeg carries with every plane 8 bits, and each
/// one's frame size: yuv444p's chroma planes are the picture's size,
/// yuv422p's half its width.
const PLANAR: &[(&str, FrameLen)] = &[
    ("yuv444p", |width, height| width * height * 3),
    ("yuv422p", |width, height| width * height * 2),
];

/// Three frames of `testsrc2` at 64x48 in `pix_fmt`, as ffmpeg muxes them
/// into `format`, off its stdout.
fn testsrc2_as(pix_fmt: &str, format: &str) -> Vec<u8> {
    ffmpeg_stdout(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48:rate=30",
            "-frames:v",
            "3",
            "-pix_fmt",
            pix_fmt,
            "-c:v",
            "rawvideo",
            "-f",
            format,
            "-",
        ],
        &[],
    )
}

#[test]
fn a_planar_raw_stream_ffmpeg_wrote_is_read_frame_for_frame() {
    // yuv444p and yuv422p as ffmpeg's NUT muxer writes them: the tag it
    // chose, the pixel format this crate names it, and every frame the same
    // bytes ffmpeg writes to `-f rawvideo`.
    if !ffmpeg_on_path() {
        announce_skip("real ffmpeg cannot write a planar raw stream");
        return;
    }
    for &(pix_fmt, frame_len) in PLANAR {
        let raw = testsrc2_as(pix_fmt, "rawvideo");
        let wire = testsrc2_as(pix_fmt, "nut");

        let mut demuxer = Demuxer::open(&wire[..]).expect("read ffmpeg's NUT headers");
        let stream = demuxer.stream();
        assert_eq!(stream.pix_fmt(), Some(pix_fmt));
        assert_eq!(stream.codec_name(), None, "{pix_fmt} is raw");
        assert_eq!(stream.video_geometry(), Some((64, 48)));
        let tb = stream.time_base;
        let mut frames = Vec::new();
        let mut buf = Vec::new();
        while let Some(pts) = demuxer.read_frame(&mut buf).expect("read a frame") {
            frames.push((pts, buf.clone()));
        }
        assert_eq!(frames.len(), 3, "{pix_fmt}");
        for (index, (pts, frame)) in frames.iter().enumerate() {
            // Frame `index` at 30 a second, in whichever time base ffmpeg
            // chose.
            assert_eq!(
                i128::from(*pts) * i128::from(tb.num) * 30,
                index as i128 * i128::from(tb.den),
                "{pix_fmt} frame {index} pts {pts} in {tb:?}"
            );
            assert_eq!(
                frame.len(),
                frame_len(64, 48),
                "{pix_fmt} frame {index} size"
            );
        }
        let bytes: Vec<u8> = frames.into_iter().flat_map(|(_, frame)| frame).collect();
        assert_eq!(
            bytes, raw,
            "{pix_fmt}: the frames are what ffmpeg writes to -f rawvideo"
        );
    }
}

#[test]
fn a_planar_raw_stream_the_muxer_wrote_decodes_as_ffmpeg_decodes_it_raw() {
    // The other way: frames of real picture content written here, read by
    // ffmpeg's NUT demuxer and hashed frame by frame, against the same
    // frames handed to ffmpeg as bare rawvideo with the format stated.
    if !ffmpeg_on_path() {
        announce_skip("real ffmpeg cannot decode a planar raw stream");
        return;
    }
    for &(pix_fmt, frame_len) in PLANAR {
        let raw = testsrc2_as(pix_fmt, "rawvideo");
        let stream =
            Stream::video(pix_fmt, 64, 48, TimeBase { num: 1, den: 30 }).expect("carried raw");
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
            for (index, frame) in raw.chunks(frame_len(64, 48)).enumerate() {
                muxer.write_frame(index as i64, frame).expect("write frame");
            }
            muxer.finish().expect("finish");
        }

        let from_nut = frame_hashes(&ffmpeg_stdout(
            &["-f", "nut", "-i", "-", "-f", "framemd5", "-"],
            &wire,
        ));
        let from_raw = frame_hashes(&ffmpeg_stdout(
            &[
                "-f",
                "rawvideo",
                "-pix_fmt",
                pix_fmt,
                "-video_size",
                "64x48",
                "-framerate",
                "30",
                "-i",
                "-",
                "-f",
                "framemd5",
                "-",
            ],
            &raw,
        ));
        assert_eq!(from_nut.len(), 3, "{pix_fmt}");
        let size = frame_len(64, 48).to_string();
        assert!(
            from_nut.iter().all(|(decoded, _)| *decoded == size),
            "{pix_fmt}: ffmpeg decoded frames of another size: {from_nut:?}"
        );
        assert_eq!(from_nut, from_raw, "{pix_fmt}");
    }
}

/// Each frame's size and hash out of a framemd5 listing. The time base and
/// timestamps are left out: a bare rawvideo input has its own.
fn frame_hashes(framemd5: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(framemd5)
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            assert_eq!(fields.len(), 6, "a framemd5 line: {line}");
            (fields[4].to_string(), fields[5].to_string())
        })
        .collect()
}

/// Runs ffmpeg over `args` with `input` on its stdin, failing with its own
/// account of anything that went wrong, and hands back its stdout. The input
/// is written from a thread of its own, so a pipe that fills on either side
/// cannot stall the other.
fn ffmpeg_stdout(args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg");
    let mut stdin = child.stdin.take().expect("ffmpeg stdin");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output().expect("wait for ffmpeg");
    let written = writer.join().expect("the stdin writer");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "ffmpeg {args:?} exited with {:?}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(stderr.is_empty(), "ffmpeg {args:?} complained:\n{stderr}");
    written.expect("write ffmpeg's stdin");
    output.stdout
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

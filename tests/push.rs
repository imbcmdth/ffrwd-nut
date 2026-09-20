//! The push front end, over the wires ffmpeg wrote and over wires built here
//! to be wrong.
//!
//! Two things are proven. One: how the bytes arrive changes nothing - the
//! same stream fed whole, a byte at a time, or in arbitrary pieces yields the
//! same events in the same order, which is what a consumer reading a socket
//! depends on. Two: nothing a hostile or truncated wire can say makes this
//! panic, allocate what it was told to, or read past what it was handed.
//!
//! `h264.nut` and `av.nut` were generated once with real ffmpeg:
//!
//! ```text
//! ffmpeg -f lavfi -i testsrc2=size=64x48:rate=25:duration=2 -c:v libx264 \
//!        -g 30 -pix_fmt yuv420p -f nut tests/h264.nut
//! ffmpeg -f lavfi -i testsrc2=size=32x24:rate=10:duration=0.4 \
//!        -f lavfi -i sine=frequency=440:sample_rate=48000:duration=0.4 \
//!        -c:v rawvideo -pix_fmt yuv420p -c:a pcm_s16le -f nut tests/av.nut
//! ```

use ffrwd_nut::{Demuxer, Event, Limits, Media, PushDemuxer, TimeBase};

/// One encoded h264 stream, B-frames and all.
const H264: &[u8] = include_bytes!("h264.nut");

/// Raw yuv420p video and interleaved pcm, the two streams a feeder sends.
const AV: &[u8] = include_bytes!("av.nut");

const MAIN_STARTCODE: u64 = 0x4E4D_7A56_1F5F_04AD;
const STREAM_STARTCODE: u64 = 0x4E53_1140_5BF2_F9DB;
const SYNCPOINT_STARTCODE: u64 = 0x4E4B_E4AD_EECA_4569;
const INFO_STARTCODE: u64 = 0x4E49_AB68_B596_BA78;
const INDEX_STARTCODE: u64 = 0x4E58_DD67_2F23_E64E;

/// One event, and the payload that came with it.
type Seen = (Event, Vec<u8>);

/// Takes every event that is ready, stopping at the end of the input.
fn drain(core: &mut PushDemuxer, out: &mut Vec<Seen>) -> Result<(), String> {
    loop {
        match core.next_event() {
            Ok(None) => return Ok(()),
            Ok(Some(Event::EndOfInput)) => {
                out.push((Event::EndOfInput, Vec::new()));
                return Ok(());
            }
            Ok(Some(event)) => {
                let payload = match event {
                    Event::Frame { .. } => core.payload().to_vec(),
                    _ => Vec::new(),
                };
                out.push((event, payload));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// Every event `wire` makes, fed in pieces of the given sizes, taken in turn.
fn read_in(wire: &[u8], sizes: &[usize]) -> Result<Vec<Seen>, String> {
    let mut core = PushDemuxer::new(Limits::default());
    let mut out = Vec::new();
    let mut at = 0;
    let mut which = 0;
    while at < wire.len() {
        let take = sizes[which % sizes.len()].clamp(1, wire.len() - at);
        which += 1;
        core.feed(&wire[at..at + take]);
        at += take;
        drain(&mut core, &mut out)?;
    }
    core.finish();
    drain(&mut core, &mut out)?;
    Ok(out)
}

/// One frame as a consumer sees it, for comparing two readings of a wire.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    stream: usize,
    pts: i64,
    dts: Option<i64>,
    keyframe: bool,
    data: Vec<u8>,
}

/// The frames alone, with their payloads.
fn frames(events: &[Seen]) -> Vec<Frame> {
    events
        .iter()
        .filter_map(|(event, payload)| match event {
            Event::Frame { stream, packet } => Some(Frame {
                stream: *stream,
                pts: packet.pts,
                dts: packet.dts,
                keyframe: packet.keyframe,
                data: payload.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// Sizes that step through a wire unevenly, from a seed, so a split is
/// arbitrary but the same on every run.
fn scattered(seed: u32, count: usize) -> Vec<usize> {
    let mut state = seed;
    (0..count)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            1 + (state >> 16) as usize % 4096
        })
        .collect()
}

#[test]
fn how_the_bytes_arrive_changes_nothing() {
    for (name, wire) in [("h264", H264), ("av", AV)] {
        let whole = read_in(wire, &[wire.len()]).expect("the whole wire parses");
        assert!(
            !frames(&whole).is_empty(),
            "{name}: the wire should carry frames"
        );

        let dribbled = read_in(wire, &[1]).expect("a byte at a time parses");
        assert_eq!(dribbled, whole, "{name}: a byte at a time read differently");

        for seed in [0xfeed_beefu32, 0x0bad_c0de, 7] {
            let sizes = scattered(seed, 64);
            let scattered = read_in(wire, &sizes).expect("arbitrary pieces parse");
            assert_eq!(
                scattered, whole,
                "{name}: pieces from seed {seed} read differently"
            );
        }
    }
}

#[test]
fn the_two_front_ends_read_the_same_packets() {
    // The blocking reader carries one media stream, so this is the h264 wire;
    // what it hands out has to be what the push parser announces.
    let pushed = frames(&read_in(H264, &[64]).expect("the fixture parses"));

    let mut demuxer = Demuxer::open(H264).expect("read the headers");
    let mut buf = Vec::new();
    let mut read = Vec::new();
    while let Some(packet) = demuxer.read_packet(&mut buf).expect("read a packet") {
        read.push(Frame {
            stream: 0,
            pts: packet.pts,
            dts: packet.dts,
            keyframe: packet.keyframe,
            data: buf.clone(),
        });
    }
    assert_eq!(pushed, read);
}

#[test]
fn coded_h264_arrives_with_its_reordering_intact() {
    // ffprobe's account of the fixture: 50 packets, keyframes at 0 and 30,
    // has_b_frames=2 so the first two packets' dts are not settled.
    let events = read_in(H264, &[97]).expect("the fixture parses");
    let frames = frames(&events);
    assert_eq!(frames.len(), 50);

    let keyframes: Vec<(usize, i64)> = frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| frame.keyframe)
        .map(|(index, frame)| (index, frame.pts))
        .collect();
    assert_eq!(keyframes, vec![(0, 4096), (30, 65536)]);

    let timestamps: Vec<(i64, Option<i64>)> = frames
        .iter()
        .map(|frame| (frame.pts, frame.dts))
        .take(4)
        .collect();
    assert_eq!(
        timestamps,
        vec![
            (4096, None),
            (12288, None),
            (8192, Some(4096)),
            (6144, Some(6144)),
        ]
    );
    let total: usize = frames.iter().map(|frame| frame.data.len()).sum();
    assert_eq!(total, 4250);

    let stream = match &events[1] {
        (Event::StreamHeader { stream, .. }, _) => stream.clone(),
        other => panic!("the second event is the stream header, got {other:?}"),
    };
    assert_eq!(stream.codec_name(), Some("h264"));
    assert_eq!(stream.decode_delay, 2);
    assert_eq!(stream.video_geometry(), Some((64, 48)));
}

#[test]
fn raw_video_and_pcm_arrive_on_their_own_streams() {
    let events = read_in(AV, &[1024]).expect("the fixture parses");
    let mut core_streams = Vec::new();
    for (event, _) in &events {
        if let Event::StreamHeader { index, stream } = event {
            core_streams.push((*index, stream.clone()));
        }
    }
    assert_eq!(core_streams.len(), 2);
    assert_eq!(core_streams[0].1.pix_fmt(), Some("yuv420p"));
    assert_eq!(core_streams[0].1.video_geometry(), Some((32, 24)));
    assert_eq!(core_streams[0].1.time_base, TimeBase { num: 1, den: 81920 });
    assert_eq!(core_streams[1].1.sample_fmt(), Some("s16"));
    assert_eq!(core_streams[1].1.audio_geometry(), Some((48000, 1)));

    // ffprobe: 4 video packets 8192 ticks apart, 19 audio packets 1024
    // samples apart.
    let frames = frames(&events);
    let video: Vec<i64> = frames
        .iter()
        .filter(|frame| frame.stream == 0)
        .map(|frame| frame.pts)
        .collect();
    let audio: Vec<i64> = frames
        .iter()
        .filter(|frame| frame.stream == 1)
        .map(|frame| frame.pts)
        .collect();
    assert_eq!(video, vec![0, 8192, 16384, 24576]);
    assert_eq!(audio.len(), 19);
    assert_eq!(audio[..3], [0, 1024, 2048]);
    for frame in frames.iter().filter(|frame| frame.stream == 0) {
        assert_eq!(frame.data.len(), 32 * 24 * 3 / 2, "one yuv420p frame");
        assert_eq!(frame.dts, Some(frame.pts), "raw video does not reorder");
        assert!(frame.keyframe, "every raw frame is a keyframe");
    }
}

#[test]
fn the_frame_rate_an_info_packet_states_reaches_the_stream() {
    let events = read_in(AV, &[4096]).expect("the fixture parses");
    let core = {
        let mut core = PushDemuxer::new(Limits::default());
        core.feed(AV);
        core.finish();
        let mut out = Vec::new();
        drain(&mut core, &mut out).expect("the fixture parses");
        core
    };
    assert!(
        events
            .iter()
            .any(|(event, _)| matches!(event, Event::Info(_))),
        "ffmpeg writes info packets on this wire"
    );
    // ffprobe: r_frame_rate 10/1 on the video stream.
    assert_eq!(
        core.stream(0).expect("the video stream").frame_rate,
        Some((10, 1))
    );
}

#[test]
fn the_header_section_ends_once_and_before_the_first_frame() {
    for wire in [H264, AV] {
        let events = read_in(wire, &[64]).expect("the fixture parses");
        let ends: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, (event, _))| matches!(event, Event::EndOfHeaders))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(ends.len(), 1, "the header section ends once");
        let first_frame = events
            .iter()
            .position(|(event, _)| matches!(event, Event::Frame { .. }))
            .expect("there are frames");
        assert!(ends[0] < first_frame, "it ends before the frames start");
    }
}

#[test]
fn every_truncation_asks_for_more_rather_than_erroring() {
    for wire in [H264, AV] {
        for cut in 0..wire.len() {
            let mut core = PushDemuxer::new(Limits::default());
            core.feed(&wire[..cut]);
            let mut out = Vec::new();
            if let Err(error) = drain(&mut core, &mut out) {
                panic!("a truncation at {cut} was an error: {error}");
            }
        }
    }
}

#[test]
fn every_truncation_that_ends_says_what_it_stopped_inside() {
    // The same cuts, with the input closed: either the wire happened to end
    // at a packet boundary, or the parser names what it was in the middle of.
    // Neither is a panic.
    let wire = AV;
    let mut ended_cleanly = 0;
    for cut in 0..wire.len() {
        let mut core = PushDemuxer::new(Limits::default());
        core.feed(&wire[..cut]);
        core.finish();
        let mut out = Vec::new();
        match drain(&mut core, &mut out) {
            Ok(()) => ended_cleanly += 1,
            Err(error) => assert!(
                error.contains("NUT") || error.contains("ADTS"),
                "a truncation at {cut} was refused without saying what: {error}"
            ),
        }
    }
    assert!(
        ended_cleanly > 0,
        "some cuts land on a packet boundary and end cleanly"
    );
}

#[test]
fn garbage_after_a_good_header_is_refused_and_never_panics() {
    let opening = opening(4, 4);
    let mut seed = 0xfeed_beefu32;
    for _ in 0..3_000 {
        let mut wire = opening.clone();
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let len = (seed >> 16) as usize % 96;
        for _ in 0..len {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            wire.push((seed >> 16) as u8);
        }
        let mut core = PushDemuxer::new(Limits::default());
        core.feed(&wire);
        core.finish();
        // Whatever it decides, it decides without unwinding.
        let mut out = Vec::new();
        let _ = drain(&mut core, &mut out);
    }
}

#[test]
fn garbage_from_the_very_first_byte_is_refused_and_never_panics() {
    let mut seed = 0x0bad_c0deu32;
    for _ in 0..3_000 {
        let mut wire = Vec::new();
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let len = (seed >> 16) as usize % 128;
        for _ in 0..len {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            wire.push((seed >> 16) as u8);
        }
        let mut core = PushDemuxer::new(Limits::default());
        core.feed(&wire);
        core.finish();
        let mut out = Vec::new();
        let _ = drain(&mut core, &mut out);
    }
}

#[test]
fn a_byte_flipped_anywhere_in_a_wire_is_refused_or_read_but_never_panics() {
    // Every hundredth byte of the encoded fixture, with one bit moved: the
    // reader either refuses it or carries on, and does either without
    // unwinding.
    for at in (0..H264.len()).step_by(100) {
        let mut wire = H264.to_vec();
        wire[at] ^= 0x40;
        let mut core = PushDemuxer::new(Limits::default());
        core.feed(&wire);
        core.finish();
        let mut out = Vec::new();
        let _ = drain(&mut core, &mut out);
    }
}

#[test]
fn a_stream_that_does_not_open_as_nut_is_refused_at_once() {
    let mut core = PushDemuxer::new(Limits::default());
    core.feed(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n         ");
    let mut out = Vec::new();
    let error = drain(&mut core, &mut out).expect_err("refused");
    assert!(error.contains("not NUT"), "got: {error}");
}

#[test]
fn a_version_this_crate_does_not_speak_is_refused_by_number() {
    let mut body = Vec::new();
    write::v(&mut body, 4);
    let mut wire = ffrwd_nut::FILE_ID.to_vec();
    write::packet(&mut wire, MAIN_STARTCODE, &body);
    let error = refusal(&wire);
    assert!(error.contains("version 4"), "got: {error}");
}

#[test]
fn a_header_whose_fields_run_off_the_end_of_it_is_refused() {
    let mut body = Vec::new();
    write::v(&mut body, 3);
    write::v(&mut body, 2);
    // and then nothing
    let mut wire = ffrwd_nut::FILE_ID.to_vec();
    write::packet(&mut wire, MAIN_STARTCODE, &body);
    let error = refusal(&wire);
    assert!(error.contains("ends"), "got: {error}");
}

#[test]
fn a_time_base_of_zero_is_refused_rather_than_divided_by() {
    let mut body = Vec::new();
    write::v(&mut body, 3);
    write::v(&mut body, 1);
    write::v(&mut body, 32_767);
    write::v(&mut body, 1);
    write::v(&mut body, 1);
    write::v(&mut body, 0);
    let mut wire = ffrwd_nut::FILE_ID.to_vec();
    write::packet(&mut wire, MAIN_STARTCODE, &body);
    let error = refusal(&wire);
    assert!(error.contains("not a rate"), "got: {error}");
}

#[test]
fn a_packet_whose_checksum_does_not_match_is_refused() {
    let mut wire = opening(4, 4);
    // The last four bytes of the audio stream header are its checksum.
    let last = wire.len() - 1;
    wire[last] ^= 0xff;
    let error = refusal(&wire);
    assert!(error.contains("checksum"), "got: {error}");
}

#[test]
fn a_framecode_the_table_does_not_define_is_refused_by_byte() {
    let mut wire = opening(4, 4);
    wire.extend(syncpoint(0, 0));
    // Byte 2 is in the run the main header marks invalid.
    wire.push(2);
    let error = refusal(&wire);
    assert!(error.contains("byte 2"), "got: {error}");
}

#[test]
fn a_frame_claiming_more_than_the_stream_carries_is_refused() {
    let mut wire = opening(4, 4);
    wire.extend(syncpoint(0, 0));
    let mut huge = vec![EXPLICIT];
    write::v(&mut huge, (CODED_PTS | STREAM_ID | SIZE_MSB) ^ CODED);
    write::v(&mut huge, 0);
    write::v(&mut huge, 1 << 14);
    write::v(&mut huge, 1 << 40);
    wire.extend(huge);

    let mut core = PushDemuxer::new(Limits::default());
    core.set_stream_limit(0, 1 << 20);
    core.feed(&wire);
    let mut out = Vec::new();
    let error = drain(&mut core, &mut out).expect_err("refused");
    assert!(
        error.contains("1048576"),
        "the message should say what the ceiling is, got: {error}"
    );
}

#[test]
fn a_frame_larger_than_the_default_ceiling_is_refused_before_it_is_allocated() {
    let mut wire = opening(4, 4);
    wire.extend(syncpoint(0, 0));
    let mut huge = vec![EXPLICIT];
    write::v(&mut huge, (CODED_PTS | STREAM_ID | SIZE_MSB) ^ CODED);
    write::v(&mut huge, 0);
    write::v(&mut huge, 1 << 14);
    write::v(&mut huge, u64::MAX / 2);
    wire.extend(huge);
    let error = refusal(&wire);
    assert!(error.contains("more than"), "got: {error}");
}

#[test]
fn headers_restated_midstream_are_read_again_and_cost_no_frame() {
    // ffmpeg restates its headers every megabyte or so. The parser reads them
    // again and says so; what it must not do is lose the frames around them.
    let mut wire = opening(4, 4);
    wire.extend(syncpoint(0, 0));
    wire.extend(frame(0, 0, &[1u8; 24]));
    wire.extend(main_header());
    wire.extend(video_header(4, 4));
    wire.extend(audio_header(48_000, 2));
    wire.extend(frame(0, 1, &[2u8; 24]));

    let events = read_in(&wire, &[3]).expect("the wire parses");
    assert_eq!(frames(&events).len(), 2, "the restated headers ate a frame");
    let mains = events
        .iter()
        .filter(|(event, _)| matches!(event, Event::MainHeader(_)))
        .count();
    assert_eq!(mains, 2, "both main headers are announced");
}

#[test]
fn the_blocking_reader_refuses_the_headers_the_parser_reads_again() {
    // The same restatement, through the front end that carries one stream:
    // it has already handed out a frame against the first set of headers, so
    // a second set is a stream description it cannot honour.
    let mut wire = ffrwd_nut::FILE_ID.to_vec();
    wire.extend(main_header_for(1));
    wire.extend(video_header(4, 4));
    wire.extend(syncpoint(0, 0));
    wire.extend(frame(0, 0, &[1u8; 24]));
    wire.extend(main_header_for(1));
    wire.extend(video_header(4, 4));
    wire.extend(frame(0, 1, &[2u8; 24]));

    let mut demuxer = Demuxer::open(&wire[..]).expect("the first headers are read");
    let mut buf = Vec::new();
    assert_eq!(
        demuxer.read_frame(&mut buf).expect("the first frame"),
        Some(0)
    );
    let error = demuxer
        .read_frame(&mut buf)
        .expect_err("the restated headers are refused")
        .to_string();
    assert!(error.contains("restates its headers mid-stream"), "{error}");
}

#[test]
fn a_packet_this_parser_does_not_read_is_stepped_over_by_its_length() {
    let mut wire = opening(4, 4);
    write::packet(&mut wire, INFO_STARTCODE, &[0xcd; 900]);
    wire.extend(syncpoint(0, 0));
    wire.extend(frame(0, 0, &[3u8; 24]));

    let events = read_in(&wire, &[7]).expect("the wire parses");
    assert_eq!(frames(&events).len(), 1);
}

#[test]
fn a_packet_too_wide_to_hold_is_stepped_over_without_buffering_it() {
    let mut wire = opening(4, 4);
    wire.extend(syncpoint(0, 0));
    wire.extend(frame(0, 0, &[4u8; 24]));
    // Wider than the parser will hold in one piece, and wide enough to carry
    // a header checksum of its own.
    let big = vec![0x5a; (Limits::default().max_header as usize) + 1_000];
    wire.extend(INDEX_STARTCODE.to_be_bytes());
    let mut length = Vec::new();
    write::v(&mut length, (big.len() + 4) as u64);
    wire.extend(length);
    wire.extend([0, 0, 0, 0]); // the header checksum
    wire.extend(&big);
    wire.extend([0, 0, 0, 0]);
    wire.extend(frame(0, 1, &[5u8; 24]));

    let mut core = PushDemuxer::new(Limits::default());
    let mut out = Vec::new();
    for chunk in wire.chunks(7_000) {
        core.feed(chunk);
        drain(&mut core, &mut out).expect("the wire parses");
        assert!(
            core.buffered() < 64 * 1024,
            "the big packet was buffered after all"
        );
    }
    assert_eq!(frames(&out).len(), 2);
}

#[test]
fn a_timestamp_carrying_only_its_low_bits_is_lifted_onto_the_last_one() {
    let mut wire = opening(4, 4);
    wire.extend(syncpoint(1_000, 0));
    let mut short = vec![EXPLICIT];
    write::v(&mut short, (CODED_PTS | STREAM_ID | SIZE_MSB) ^ CODED);
    write::v(&mut short, 0);
    write::v(&mut short, 1_001 & ((1 << 14) - 1));
    write::v(&mut short, 24);
    short.extend_from_slice(&[0u8; 24]);
    wire.extend(short);

    let events = read_in(&wire, &[5]).expect("the wire parses");
    assert_eq!(frames(&events)[0].pts, 1_001);
}

#[test]
fn a_frame_before_the_stream_headers_is_refused() {
    let mut wire = ffrwd_nut::FILE_ID.to_vec();
    wire.extend(main_header());
    wire.extend(frame(0, 0, &[0u8; 24]));
    let error = refusal(&wire);
    assert!(error.contains("before its stream header"), "got: {error}");
}

#[test]
fn a_subtitle_stream_is_described_by_its_class_and_its_frames_still_arrive() {
    // Nothing here reads subtitles, and a consumer that does not either still
    // has to get past them without losing the stream.
    let mut body = Vec::new();
    write::v(&mut body, 1); // stream id
    write::v(&mut body, 2); // class: subtitles
    write::vb(&mut body, b"UTF8");
    write::v(&mut body, 1); // time base index
    write::v(&mut body, 14);
    write::v(&mut body, 0);
    write::v(&mut body, 0);
    write::v(&mut body, 0);
    write::vb(&mut body, &[]);
    let mut wire = ffrwd_nut::FILE_ID.to_vec();
    wire.extend(main_header());
    wire.extend(video_header(4, 4));
    write::packet(&mut wire, STREAM_STARTCODE, &body);
    wire.extend(syncpoint(0, 0));
    wire.extend(frame(1, 0, b"a line of dialogue"));
    wire.extend(frame(0, 0, &[9u8; 24]));

    let events = read_in(&wire, &[11]).expect("the wire parses");
    let streams: Vec<Media> = events
        .iter()
        .filter_map(|(event, _)| match event {
            Event::StreamHeader { stream, .. } => Some(stream.media),
            _ => None,
        })
        .collect();
    assert_eq!(streams[1], Media::Other { class: 2 });
    let frames = frames(&events);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].stream, 1);
    assert_eq!(frames[0].data, b"a line of dialogue");
}

/// The message a whole wire is refused with.
fn refusal(wire: &[u8]) -> String {
    let mut core = PushDemuxer::new(Limits::default());
    core.feed(wire);
    core.finish();
    let mut out = Vec::new();
    drain(&mut core, &mut out).expect_err("this wire should have been refused")
}

// The wire the hardening cases are built on, written here rather than by the
// muxer: what they need to say is not something a muxer would write.

const VIDEO_BASE: (u64, u64) = (1, 25);
const AUDIO_BASE: (u64, u64) = (1, 48_000);

/// The framecode ffmpeg writes for a stream whose frames all state their own
/// flags: one usable code, everything else invalid.
const EXPLICIT: u8 = 1;

const CODED_PTS: u64 = 8;
const STREAM_ID: u64 = 16;
const SIZE_MSB: u64 = 32;
const CODED: u64 = 4096;
const INVALID: u64 = 8192;

mod write {
    use ffrwd_nut::crc32;

    pub fn v(out: &mut Vec<u8>, value: u64) {
        let mut groups = [0u8; 10];
        let mut count = 0;
        let mut left = value;
        loop {
            groups[count] = (left & 0x7f) as u8;
            count += 1;
            left >>= 7;
            if left == 0 {
                break;
            }
        }
        while count > 0 {
            count -= 1;
            out.push(groups[count] | if count > 0 { 0x80 } else { 0 });
        }
    }

    pub fn s(out: &mut Vec<u8>, value: i64) {
        let magnitude = value.unsigned_abs();
        v(out, magnitude.saturating_mul(2) - u64::from(value > 0));
    }

    pub fn vb(out: &mut Vec<u8>, bytes: &[u8]) {
        v(out, bytes.len() as u64);
        out.extend_from_slice(bytes);
    }

    /// A packet: its startcode, the length of what follows, its body and the
    /// checksum the body ends with.
    pub fn packet(out: &mut Vec<u8>, startcode: u64, fields: &[u8]) {
        out.extend_from_slice(&startcode.to_be_bytes());
        v(out, (fields.len() + 4) as u64);
        out.extend_from_slice(fields);
        out.extend_from_slice(&crc32(fields).to_be_bytes());
    }
}

fn main_header() -> Vec<u8> {
    main_header_for(2)
}

fn main_header_for(streams: u64) -> Vec<u8> {
    let mut body = Vec::new();
    write::v(&mut body, 3); // version
    write::v(&mut body, streams); // stream count
    write::v(&mut body, 32_767); // max distance
    write::v(&mut body, 2); // time base count
    write::v(&mut body, VIDEO_BASE.0);
    write::v(&mut body, VIDEO_BASE.1);
    write::v(&mut body, AUDIO_BASE.0);
    write::v(&mut body, AUDIO_BASE.1);
    // Three groups: index 0 invalid, index 1 the explicit code, the rest
    // invalid.
    for (flags, size_mul, count) in [(INVALID, 0u64, 1u64), (CODED, 1, 1), (INVALID, 0, 253)] {
        write::v(&mut body, flags);
        write::v(&mut body, 6);
        write::s(&mut body, 0);
        write::v(&mut body, size_mul);
        write::v(&mut body, 0);
        write::v(&mut body, 0);
        write::v(&mut body, 0);
        write::v(&mut body, count);
    }
    write::v(&mut body, 0); // no elision headers
    let mut out = Vec::new();
    write::packet(&mut out, MAIN_STARTCODE, &body);
    out
}

fn video_header(width: u64, height: u64) -> Vec<u8> {
    let mut body = Vec::new();
    write::v(&mut body, 0); // stream id
    write::v(&mut body, 0); // class: video
    write::vb(&mut body, b"I420");
    write::v(&mut body, 0); // time base index
    write::v(&mut body, 14); // msb pts shift
    write::v(&mut body, 0); // max pts distance
    write::v(&mut body, 0); // decode delay
    write::v(&mut body, 0); // stream flags
    write::vb(&mut body, &[]); // extradata
    write::v(&mut body, width);
    write::v(&mut body, height);
    write::v(&mut body, 1);
    write::v(&mut body, 1);
    write::v(&mut body, 0); // colorspace
    let mut out = Vec::new();
    write::packet(&mut out, STREAM_STARTCODE, &body);
    out
}

fn audio_header(rate: u64, channels: u64) -> Vec<u8> {
    let mut body = Vec::new();
    write::v(&mut body, 1); // stream id
    write::v(&mut body, 1); // class: audio
    write::vb(&mut body, b"PSD\x10");
    write::v(&mut body, 1); // time base index
    write::v(&mut body, 14);
    write::v(&mut body, 0);
    write::v(&mut body, 0);
    write::v(&mut body, 0);
    write::vb(&mut body, &[]);
    write::v(&mut body, rate);
    write::v(&mut body, 1);
    write::v(&mut body, channels);
    let mut out = Vec::new();
    write::packet(&mut out, STREAM_STARTCODE, &body);
    out
}

fn syncpoint(ticks: u64, base_index: u64) -> Vec<u8> {
    let mut body = Vec::new();
    write::v(&mut body, ticks * 2 + base_index);
    write::v(&mut body, 0);
    let mut out = Vec::new();
    write::packet(&mut out, SYNCPOINT_STARTCODE, &body);
    out
}

fn frame(stream: u64, pts: u64, data: &[u8]) -> Vec<u8> {
    let mut out = vec![EXPLICIT];
    write::v(&mut out, (CODED_PTS | STREAM_ID | SIZE_MSB) ^ CODED);
    write::v(&mut out, stream);
    // Absolute, so the reader never has to lift the high bits.
    write::v(&mut out, pts + (1 << 14));
    write::v(&mut out, data.len() as u64);
    out.extend_from_slice(data);
    out
}

fn opening(width: u64, height: u64) -> Vec<u8> {
    let mut wire = ffrwd_nut::FILE_ID.to_vec();
    wire.extend(main_header());
    wire.extend(video_header(width, height));
    wire.extend(audio_header(48_000, 2));
    wire
}

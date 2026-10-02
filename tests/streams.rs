//! Several streams on one wire, written here and read back through the push
//! front end: a picture, its sound and a data stream beside them, each in a
//! time base of its own, and the annotation stream after the last of them.
//!
//! The ffmpeg half needs ffprobe on PATH and skips, saying so, where it is
//! not.

use std::io::Write;
use std::process::{Command, Stdio};

use ffrwd_nut::{Event, Limits, Muxer, Packet, PushDemuxer, Stream, TimeBase};

const VIDEO_TB: TimeBase = TimeBase { num: 1, den: 25 };
const MILLIS: TimeBase = TimeBase { num: 1, den: 1000 };

/// What one stream of the wire carries, in the order it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sent {
    stream: usize,
    pts: i64,
    data: Vec<u8>,
}

fn streams() -> Vec<Stream> {
    let mut video = Stream::video("rgba", 32, 32, VIDEO_TB).expect("rgba is carried");
    video.frame_rate = Some((25, 1));
    let audio = Stream::audio("s16", 48000, 2).expect("s16 is carried");
    vec![video, audio, Stream::json(MILLIS)]
}

/// One second of the three interleaved by time: 25 pictures, an audio packet
/// of 1920 samples beside each, and a message every fifth picture. A picture
/// is four kilobytes, so syncpoints fall between streams as well as on them.
fn second() -> Vec<Sent> {
    let mut sent = Vec::new();
    for index in 0..25i64 {
        sent.push(Sent {
            stream: 0,
            pts: index,
            data: vec![index as u8; 32 * 32 * 4],
        });
        sent.push(Sent {
            stream: 1,
            pts: index * 1920,
            data: vec![(index as u8).wrapping_add(100); 1920 * 4],
        });
        if index % 5 == 0 {
            sent.push(Sent {
                stream: 2,
                pts: index * 40,
                data: format!(r#"{{"at":{index}}}"#).into_bytes(),
            });
        }
    }
    sent
}

fn wire(streams: &[Stream], sent: &[Sent]) -> Vec<u8> {
    let mut wire = Vec::new();
    {
        let mut muxer = Muxer::with_streams(&mut wire, streams).expect("write headers");
        write_all(&mut muxer, sent);
        muxer.finish().expect("finish");
    }
    wire
}

fn write_all<W: Write>(muxer: &mut Muxer<W>, sent: &[Sent]) {
    for item in sent {
        if item.stream == 2 {
            let packet = Packet {
                pts: item.pts,
                dts: Some(item.pts),
                keyframe: true,
            };
            muxer
                .write_coded_to(item.stream, &packet, &item.data)
                .expect("write a message");
        } else {
            muxer
                .write_frame_to(item.stream, item.pts, &item.data)
                .expect("write a frame");
        }
    }
}

/// The stream headers and every frame of `wire`, fed in pieces of `size`.
fn read(wire: &[u8], size: usize) -> (Vec<Stream>, Vec<Sent>) {
    let mut core = PushDemuxer::new(Limits::default());
    let mut frames = Vec::new();
    let mut ended = false;
    for piece in wire.chunks(size) {
        core.feed(piece);
        drain(&mut core, &mut frames, &mut ended);
    }
    core.finish();
    drain(&mut core, &mut frames, &mut ended);
    assert!(ended, "the wire ends at a packet boundary");
    let streams = core
        .streams()
        .iter()
        .map(|stream| {
            stream
                .clone()
                .expect("every declared stream has its header")
        })
        .collect();
    (streams, frames)
}

fn drain(core: &mut PushDemuxer, frames: &mut Vec<Sent>, ended: &mut bool) {
    while let Some(event) = core.next_event().expect("the wire parses") {
        match event {
            Event::Frame { stream, packet } => frames.push(Sent {
                stream,
                pts: packet.pts,
                data: core.payload().to_vec(),
            }),
            Event::EndOfInput => {
                *ended = true;
                return;
            }
            _ => {}
        }
    }
}

#[test]
fn video_audio_and_data_round_trip_through_the_push_demuxer() {
    let declared = streams();
    let sent = second();
    let wire = wire(&declared, &sent);
    for size in [1, 777, wire.len()] {
        let (streams, got) = read(&wire, size);
        assert_eq!(streams.len(), 3, "fed {size} at a time");
        assert_eq!(streams[0].pix_fmt(), Some("rgba"));
        assert_eq!(streams[0].time_base, VIDEO_TB);
        assert_eq!(streams[0].frame_rate, Some((25, 1)));
        assert_eq!(streams[1].sample_fmt(), Some("s16"));
        assert_eq!(streams[1].audio_geometry(), Some((48000, 2)));
        assert_eq!(streams[1].time_base, TimeBase { num: 1, den: 48000 });
        assert_eq!(
            streams[1].frame_rate, None,
            "only the picture states a rate"
        );
        assert!(streams[2].is_json());
        assert_eq!(streams[2].time_base, MILLIS);
        assert_eq!(
            got, sent,
            "every frame on its stream, in order, fed {size} at a time"
        );
    }
}

#[test]
fn a_time_base_two_streams_share_is_declared_once() {
    let video = Stream::video("rgba", 2, 2, VIDEO_TB).expect("rgba is carried");
    let shared = [video.clone(), Stream::json(VIDEO_TB)];
    let mut one = Vec::new();
    let mut two = Vec::new();
    Muxer::with_streams(&mut one, &shared).expect("write headers");
    Muxer::with_streams(&mut two, &[video, Stream::json(MILLIS)]).expect("write headers");
    assert!(
        one.len() < two.len(),
        "the shared time base is stated once: {} against {}",
        one.len(),
        two.len()
    );
    let sent = vec![
        Sent {
            stream: 0,
            pts: 3,
            data: vec![1; 16],
        },
        Sent {
            stream: 1,
            pts: 3,
            data: b"{}".to_vec(),
        },
    ];
    let mut wire = Vec::new();
    {
        let mut muxer = Muxer::with_streams(&mut wire, &shared).expect("write headers");
        muxer
            .write_frame_to(0, 3, &sent[0].data)
            .expect("write a frame");
        let packet = Packet {
            pts: 3,
            dts: Some(3),
            keyframe: true,
        };
        muxer
            .write_coded_to(1, &packet, &sent[1].data)
            .expect("write a message");
    }
    assert_eq!(read(&wire, 64).1, sent);
}

#[test]
fn one_stream_given_as_a_list_writes_what_new_writes() {
    let stream = streams().remove(0);
    let frames = [(0i64, vec![7u8; 32 * 32 * 4]), (1, vec![8u8; 32 * 32 * 4])];
    let mut alone = Vec::new();
    let mut listed = Vec::new();
    {
        let mut muxer = Muxer::new(&mut alone, &stream).expect("write headers");
        for (pts, data) in &frames {
            muxer.write_frame(*pts, data).expect("write a frame");
        }
    }
    {
        let mut muxer = Muxer::with_streams(&mut listed, &[stream]).expect("write headers");
        for (pts, data) in &frames {
            muxer.write_frame_to(0, *pts, data).expect("write a frame");
        }
    }
    assert_eq!(alone, listed);
}

#[test]
fn a_stream_that_was_never_declared_is_refused() {
    let mut wire = Vec::new();
    let mut muxer = Muxer::with_streams(&mut wire, &streams()).expect("write headers");
    let err = muxer.write_frame_to(3, 0, b"x").unwrap_err().to_string();
    assert!(err.contains("no stream 3"), "{err}");
    let err = muxer.write_frame_to(2, 0, b"{}").unwrap_err().to_string();
    assert!(err.contains("use write_coded"), "{err}");
    assert!(Muxer::with_streams(&mut Vec::new(), &[]).is_err());
}

#[cfg(feature = "annotations")]
#[test]
fn the_annotation_stream_follows_the_last_media_stream() {
    use ffrwd_nut::{ANNOTATION_CLASS, ANNOTATION_FOURCC};

    let declared = streams();
    let sent = second();
    let mut wire = Vec::new();
    {
        let mut muxer = Muxer::with_streams_annotated(&mut wire, &declared).expect("write headers");
        for item in &sent {
            if item.stream == 0 && item.pts % 10 == 0 {
                muxer
                    .write_rows(item.pts, &[format!(r#"{{"frame":{}}}"#, item.pts)])
                    .expect("write rows");
            }
            write_all(&mut muxer, std::slice::from_ref(item));
        }
        muxer.finish().expect("finish");
    }
    let (streams, got) = read(&wire, 999);
    assert_eq!(streams.len(), 4);
    assert_eq!(streams[3].class(), ANNOTATION_CLASS);
    assert_eq!(streams[3].fourcc, ANNOTATION_FOURCC);
    assert_eq!(
        streams[3].time_base, VIDEO_TB,
        "rows count in the first stream's clock"
    );
    let rows: Vec<(i64, String)> = got
        .iter()
        .filter(|frame| frame.stream == 3)
        .map(|frame| (frame.pts, String::from_utf8(frame.data.clone()).unwrap()))
        .collect();
    assert_eq!(
        rows,
        vec![
            (0, r#"{"frame":0}"#.to_string()),
            (10, r#"{"frame":10}"#.to_string()),
            (20, r#"{"frame":20}"#.to_string()),
        ]
    );
    let media: Vec<Sent> = got.into_iter().filter(|frame| frame.stream != 3).collect();
    assert_eq!(
        media, sent,
        "the media streams are untouched beside the rows"
    );
}

fn ffprobe_on_path() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn ffprobe_reads_every_stream_with_its_clock() {
    if !ffprobe_on_path() {
        eprintln!("SKIPPED: ffprobe is not on PATH.");
        return;
    }
    let sent = second();
    let wire = wire(&streams(), &sent);
    let mut child = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "nut",
            "-i",
            "-",
            "-show_entries",
            "packet=stream_index,pts",
            "-of",
            "csv=p=0",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start ffprobe");
    let mut stdin = child.stdin.take().expect("ffprobe's stdin");
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&wire);
    });
    let output = child.wait_with_output().expect("ffprobe runs");
    feeder.join().expect("the wire is handed over");
    assert!(
        output.status.success(),
        "ffprobe refused the wire: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut got: Vec<(usize, i64)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (stream, pts) = line.trim().split_once(',').expect("index,pts");
            (stream.parse().unwrap(), pts.parse().unwrap())
        })
        .collect();
    let mut want: Vec<(usize, i64)> = sent.iter().map(|item| (item.stream, item.pts)).collect();
    got.sort();
    want.sort();
    assert_eq!(got, want);
}

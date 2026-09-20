//! What the demuxer and the muxer say about each other, over wires this
//! crate wrote. The push parser's own tests are in `tests/push.rs`, where
//! they read the same wires through the other front end.

/// Whole streams through both ends.
mod wire {
    use crate::bytes::crc32;
    use crate::*;
    use std::io::Cursor;

    fn a_stream() -> Stream {
        Stream::video("rgba", 8, 8, TimeBase { num: 1, den: 65536 }).expect("rgba is carried")
    }

    /// Writes `frames` as NUT and reads them back through the demuxer.
    fn round_trip(stream: &Stream, frames: &[(i64, Vec<u8>)]) -> Vec<(i64, Vec<u8>)> {
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::new(&mut wire, stream).expect("write headers");
            for (pts, data) in frames {
                muxer.write_frame(*pts, data).expect("write frame");
            }
            muxer.finish().expect("finish");
        }
        let mut demuxer = Demuxer::open(Cursor::new(wire)).expect("read headers");
        assert_eq!(demuxer.stream().media, stream.media);
        assert_eq!(demuxer.stream().time_base, stream.time_base);
        assert_eq!(demuxer.stream().fourcc, stream.fourcc);

        let mut out = Vec::new();
        let mut buf = Vec::new();
        while let Some(pts) = demuxer.read_frame(&mut buf).expect("read frame") {
            out.push((pts, buf.clone()));
        }
        out
    }

    #[test]
    fn frames_and_their_timestamps_survive_a_round_trip() {
        let stream = a_stream();
        let frames: Vec<(i64, Vec<u8>)> = (0..5i64)
            .map(|i| (i * 65536, vec![i as u8; 8 * 8 * 4]))
            .collect();
        assert_eq!(round_trip(&stream, &frames), frames);
    }

    #[test]
    fn colorspace_and_aspect_ratio_survive_a_round_trip() {
        // What an upstream sidecar's header declared: Rec 709 full range,
        // anamorphic 4:3 samples. The output header is built from the
        // input's, so both must come back exactly - `round_trip` compares
        // the whole `media`.
        let mut stream = a_stream();
        stream.media = Media::Video {
            width: 8,
            height: 8,
            sample_width: 4,
            sample_height: 3,
            colorspace_type: 18,
        };
        let frames: Vec<(i64, Vec<u8>)> = (0..2i64)
            .map(|i| (i * 65536, vec![i as u8; 8 * 8 * 4]))
            .collect();
        assert_eq!(round_trip(&stream, &frames), frames);
    }

    #[test]
    fn timestamps_need_not_be_evenly_spaced() {
        let stream = a_stream();
        let frames: Vec<(i64, Vec<u8>)> = [0i64, 7, 1_000_000, 1_000_001]
            .iter()
            .map(|pts| (*pts, vec![0xAB; 8 * 8 * 4]))
            .collect();
        assert_eq!(round_trip(&stream, &frames), frames);
    }

    #[test]
    fn frames_larger_than_the_syncpoint_distance_round_trip() {
        let stream =
            Stream::video("rgba", 128, 128, TimeBase { num: 1, den: 25 }).expect("rgba is carried");
        let frames: Vec<(i64, Vec<u8>)> = (0..3i64)
            .map(|i| (i, vec![i as u8; 128 * 128 * 4]))
            .collect();
        assert_eq!(round_trip(&stream, &frames), frames);
    }

    /// Writes `frames` with their rows on the annotation stream and reads
    /// both back, pairing each frame's rows with it by PTS.
    #[cfg(feature = "annotations")]
    fn round_trip_annotated(
        stream: &Stream,
        frames: &[(i64, Vec<u8>, Vec<String>)],
    ) -> Vec<(i64, Vec<u8>, Vec<String>)> {
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::with_annotations(&mut wire, stream).expect("write headers");
            for (pts, data, rows) in frames {
                muxer.write_rows(*pts, rows).expect("write rows");
                muxer.write_frame(*pts, data).expect("write frame");
            }
            muxer.finish().expect("finish");
        }
        let mut demuxer = Demuxer::open_annotated(Cursor::new(wire)).expect("read headers");
        assert!(demuxer.has_annotations(), "the second stream was declared");

        let mut out = Vec::new();
        let mut buf = Vec::new();
        while let Some(pts) = demuxer.read_frame(&mut buf).expect("read frame") {
            out.push((pts, buf.clone(), demuxer.take_rows(pts)));
        }
        out
    }

    #[test]
    #[cfg(feature = "annotations")]
    fn rows_come_back_attached_to_their_own_frame() {
        let stream = a_stream();
        let frame = |i: u8| vec![i; 8 * 8 * 4];
        let frames = vec![
            (
                0i64,
                frame(0),
                vec![r#"{"x":1,"y":2,"w":3,"h":4}"#.to_string()],
            ),
            // No rows: this frame gets no packet at all, and must still come
            // back with an empty list rather than its neighbour's rows.
            (65536, frame(1), vec![]),
            (
                131_072,
                frame(2),
                vec![
                    r#"{"x":5,"y":6,"w":7,"h":8}"#.to_string(),
                    r#"{"x":9,"y":10,"w":11,"h":12}"#.to_string(),
                ],
            ),
        ];
        assert_eq!(round_trip_annotated(&stream, &frames), frames);
    }

    #[test]
    #[cfg(feature = "annotations")]
    fn the_trailing_record_comes_back_belonging_to_no_frame() {
        let stream = a_stream();
        let trailing = vec![r#"{"frames":2}"#.to_string(), r#"{"cuts":1}"#.to_string()];
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::with_annotations(&mut wire, &stream).expect("write headers");
            for pts in [0i64, 65536] {
                muxer
                    .write_rows(pts, &[r#"{"x":1,"y":2,"w":3,"h":4}"#.to_string()])
                    .expect("write rows");
                muxer
                    .write_frame(pts, &vec![7u8; 8 * 8 * 4])
                    .expect("write frame");
            }
            muxer
                .write_trailing(65536, &trailing)
                .expect("write the trailing record");
            muxer.finish().expect("finish");
        }

        let mut demuxer = Demuxer::open_annotated(Cursor::new(wire)).expect("read headers");
        let mut buf = Vec::new();
        while let Some(pts) = demuxer.read_frame(&mut buf).expect("read frame") {
            assert_eq!(
                demuxer.take_rows(pts).len(),
                1,
                "the record is no frame's rows"
            );
        }
        assert_eq!(
            demuxer.take_trailing(),
            trailing,
            "the rows come back as they were written"
        );
    }

    #[test]
    #[cfg(feature = "annotations")]
    fn a_trailing_row_that_is_not_json_has_no_record_to_ride_in() {
        let stream = a_stream();
        let mut wire = Vec::new();
        let mut muxer = Muxer::with_annotations(&mut wire, &stream).expect("write headers");
        let err = muxer
            .write_trailing(0, &["not json".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not JSON"), "{err}");
    }

    #[test]
    #[cfg(feature = "annotations")]
    fn an_annotated_stream_read_as_a_plain_one_is_refused() {
        let stream = a_stream();
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::with_annotations(&mut wire, &stream).expect("write headers");
            muxer
                .write_rows(0, &[r#"{"x":0,"y":0,"w":1,"h":1}"#.to_string()])
                .expect("write rows");
            muxer.write_frame(0, &vec![0u8; 8 * 8 * 4]).expect("frame");
            muxer.finish().expect("finish");
        }
        let err = match Demuxer::open(Cursor::new(wire)) {
            Ok(_) => panic!("the ffmpeg-facing wire carries one stream"),
            Err(e) => e.to_string(),
        };
        assert_eq!(err, "NUT input carries 2 streams; this wire carries one");
    }

    #[test]
    #[cfg(feature = "annotations")]
    fn a_plain_stream_is_still_read_when_annotations_are_asked_for() {
        let stream = a_stream();
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
            muxer.write_frame(7, &vec![3u8; 8 * 8 * 4]).expect("frame");
            muxer.finish().expect("finish");
        }
        let mut demuxer = Demuxer::open_annotated(Cursor::new(wire)).expect("read headers");
        assert!(!demuxer.has_annotations());
        let mut buf = Vec::new();
        assert_eq!(demuxer.read_frame(&mut buf).expect("read frame"), Some(7));
        assert!(demuxer.take_rows(7).is_empty());
    }

    #[test]
    fn a_codec_tag_this_wire_does_not_carry_is_named() {
        let mut stream = a_stream();
        stream.fourcc = b"XYZW".to_vec();
        assert_eq!(stream.pix_fmt(), None);
        assert_eq!(stream.fourcc_name(), "XYZW");
    }

    /// An encoded AAC stream header, as ffmpeg's NUT muxer writes one: the
    /// codec tag `ff 00 00 00`, and the AudioSpecificConfig as extradata.
    fn an_aac_stream() -> Stream {
        Stream {
            fourcc: b"\xff\x00\x00\x00".to_vec(),
            time_base: TimeBase { num: 1, den: 48000 },
            msb_pts_shift: 14,
            max_pts_distance: 48000,
            decode_delay: 0,
            // 48 kHz mono AAC-LC, as ffprobe reported it.
            extradata: vec![0x11, 0x88, 0x56, 0xe5, 0x00],
            frame_rate: None,
            media: Media::Audio {
                sample_rate: 48000,
                channels: 1,
            },
        }
    }

    #[test]
    fn an_encoded_audio_stream_is_named_by_its_tag() {
        let stream = an_aac_stream();
        assert_eq!(stream.codec_name(), Some("aac"));
        assert_eq!(stream.sample_fmt(), None);
        assert_eq!(stream.audio_geometry(), Some((48000, 1)));
    }

    #[test]
    fn an_audio_tag_this_wire_does_not_carry_names_no_codec() {
        let mut stream = an_aac_stream();
        stream.fourcc = b"OPUS".to_vec();
        assert_eq!(stream.codec_name(), None);
        assert_eq!(stream.fourcc_name(), "OPUS");
    }

    #[test]
    fn a_video_tag_never_names_an_audio_codec() {
        let mut stream = an_aac_stream();
        stream.fourcc = b"H264".to_vec();
        assert_eq!(stream.codec_name(), None);
    }

    #[test]
    fn encoded_audio_packets_and_their_extradata_survive_a_round_trip() {
        let stream = an_aac_stream();
        // Each packet is one AAC frame of 1024 samples, so the timestamps
        // step by 1024 in the stream's own ticks.
        let frames: Vec<(i64, Vec<u8>)> = (0..4i64)
            .map(|i| (i * 1024, vec![0x21, i as u8, 0x10, 0x04]))
            .collect();
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
            for (pts, data) in &frames {
                let packet = Packet {
                    pts: *pts,
                    dts: None,
                    keyframe: true,
                };
                muxer.write_coded(&packet, data).expect("write packet");
            }
            muxer.finish().expect("finish");
        }
        let mut demuxer = Demuxer::open(Cursor::new(wire)).expect("read headers");
        assert_eq!(demuxer.stream().codec_name(), Some("aac"));
        assert_eq!(demuxer.stream().extradata, stream.extradata);
        let mut out = Vec::new();
        let mut buf = Vec::new();
        while let Some(pts) = demuxer.read_frame(&mut buf).expect("read packet") {
            out.push((pts, buf.clone()));
        }
        assert_eq!(out, frames);
    }

    #[test]
    fn yuv420p_uses_the_tag_ffmpeg_writes() {
        assert_eq!(fourcc_for_pix_fmt("yuv420p"), Some(b"I420"));
        assert_eq!(fourcc_for_pix_fmt("rgba"), Some(b"RGBA"));
        assert_eq!(fourcc_for_pix_fmt("gray"), None);
    }

    #[test]
    fn fourcc_for_coded_reads_the_muxers_own_tag() {
        assert_eq!(fourcc_for_coded("video", "h264"), Some(b"H264"));
        assert_eq!(fourcc_for_coded("video", "hevc"), Some(b"HEVC"));
        assert_eq!(fourcc_for_coded("video", "av1"), Some(b"AV01"));
        assert_eq!(fourcc_for_coded("audio", "aac"), Some(b"\xff\x00\x00\x00"));
        assert_eq!(fourcc_for_coded("video", "aac"), None, "wrong kind");
        assert_eq!(fourcc_for_coded("video", "vp9"), None, "not carried");
        assert_eq!(fourcc_for_coded("subtitle", "h264"), None, "not a kind");
    }

    /// The message `open` refuses `wire` with.
    fn refusal(wire: &[u8]) -> String {
        match Demuxer::open(wire) {
            Ok(_) => panic!("this stream should have been refused"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn a_wire_that_is_not_nut_is_refused_by_the_identifier() {
        let err = refusal(b"rawvideo bytes, no header at all");
        assert!(err.contains("not NUT"), "{err}");
    }

    #[test]
    fn a_truncated_frame_is_an_error() {
        let stream = a_stream();
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
            muxer
                .write_frame(0, &[0u8; 8 * 8 * 4])
                .expect("write frame");
            muxer.finish().expect("finish");
        }
        let mut demuxer = Demuxer::open(&wire[..wire.len() - 4]).expect("headers are intact");
        let err = demuxer
            .read_frame(&mut Vec::new())
            .expect_err("the frame is short")
            .to_string();
        assert!(err.contains("ends inside a frame"), "{err}");
    }

    #[test]
    fn a_corrupt_header_checksum_is_an_error() {
        let stream = Stream::video("rgba", 2, 2, TimeBase { num: 1, den: 25 }).expect("rgba");
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
            muxer.write_frame(0, &[0u8; 16]).expect("write frame");
            muxer.finish().expect("finish");
        }
        // The main header's body starts right after the identifier, the
        // startcode and a one-byte forward pointer; flip a bit in it.
        wire[FILE_ID.len() + 9] ^= 0x01;
        let err = refusal(&wire);
        assert!(err.contains("checksum"), "{err}");
    }

    #[test]
    fn a_main_header_naming_two_streams_is_refused() {
        let stream = Stream::video("rgba", 2, 2, TimeBase { num: 1, den: 25 }).expect("rgba");
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
            muxer.write_frame(0, &[0u8; 16]).expect("write frame");
            muxer.finish().expect("finish");
        }
        // The main header's length follows the identifier and the startcode,
        // and its body starts after that: version, then the stream count.
        let length_at = FILE_ID.len() + 8;
        assert!(wire[length_at] < 0x80, "the main header body is small");
        let len = usize::from(wire[length_at]);
        let body = length_at + 1;
        wire[body + 1] = 2;
        let checksum = crc32(&wire[body..body + len - 4]).to_be_bytes();
        wire[body + len - 4..body + len].copy_from_slice(&checksum);

        let err = refusal(&wire);
        assert!(err.contains("2 streams"), "{err}");
    }

    #[test]
    fn a_codec_tag_survives_the_headers() {
        let stream = Stream::video("yuv420p", 4, 4, TimeBase { num: 1, den: 30 })
            .expect("yuv420p is carried");
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
            muxer.write_frame(3, &[7u8; 24]).expect("write frame");
            muxer.finish().expect("finish");
        }

        let demuxer = Demuxer::open(&wire[..]).expect("read headers");
        assert_eq!(
            demuxer.stream().fourcc,
            fourcc_for_pix_fmt("yuv420p").unwrap()
        );
        assert_eq!(demuxer.stream().pix_fmt(), Some("yuv420p"));
        assert_eq!(demuxer.stream().video_geometry(), Some((4, 4)));
        assert_eq!(demuxer.stream().time_base, TimeBase { num: 1, den: 30 });
    }
}

/// The ADTS header an aac stream hides its config in, through a whole wire.
mod adts_over_the_wire {
    use crate::adts;
    use crate::*;

    /// An aac stream header with `extradata`, the shape ffmpeg's NUT muxer
    /// writes off a plain `-c copy`: empty when the source was ADTS, filled
    /// when it was already mp4 or another container with a real esds.
    fn a_coded_aac_stream(extradata: Vec<u8>) -> Stream {
        Stream {
            fourcc: b"\xff\x00\x00\x00".to_vec(),
            time_base: TimeBase { num: 1, den: 48000 },
            msb_pts_shift: 14,
            max_pts_distance: 48000,
            decode_delay: 0,
            extradata,
            frame_rate: None,
            media: Media::Audio {
                sample_rate: 48000,
                channels: 2,
            },
        }
    }

    /// Writes `packets` as coded aac frames onto `stream` and reads them back.
    fn round_trip_coded(stream: &Stream, packets: &[Vec<u8>]) -> Result<(Stream, Vec<Vec<u8>>)> {
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::new(&mut wire, stream).expect("write headers");
            for (index, data) in packets.iter().enumerate() {
                let packet = Packet {
                    pts: index as i64 * 1024,
                    dts: None,
                    keyframe: true,
                };
                muxer
                    .write_coded(&packet, data)
                    .expect("write coded packet");
            }
            muxer.finish().expect("finish");
        }
        let mut demuxer = Demuxer::open(&wire[..])?;
        let opened_stream = demuxer.stream().clone();
        let mut buf = Vec::new();
        let mut out = Vec::new();
        while demuxer.read_packet(&mut buf)?.is_some() {
            out.push(buf.clone());
        }
        Ok((opened_stream, out))
    }

    #[test]
    fn adts_derives_extradata_and_is_stripped_from_every_packet() {
        let stream = a_coded_aac_stream(Vec::new());
        // LC 48000 stereo, the `11 90` case: two packets, so the fix is
        // proven on the buffered first packet and on one read normally.
        let packets = vec![
            adts::test_packet(1, 3, 2, false, 4),
            adts::test_packet(1, 3, 2, false, 6),
        ];

        let (opened, got) = round_trip_coded(&stream, &packets).expect("adts primes cleanly");
        assert_eq!(
            opened.extradata,
            vec![0x11, 0x90],
            "the AudioSpecificConfig this ADTS header codes"
        );
        assert_eq!(
            got,
            vec![vec![0xEE; 4], vec![0xEE; 6]],
            "the ADTS header is gone, the payload is not"
        );
        for (index, data) in got.iter().enumerate() {
            assert!(
                !(data.first() == Some(&0xFF) && data.get(1).is_some_and(|b| b & 0xF0 == 0xF0)),
                "packet {index} still starts with the ADTS syncword"
            );
        }
    }

    #[test]
    fn a_stream_that_already_has_extradata_is_left_alone_even_over_adts_bytes() {
        let stream = a_coded_aac_stream(vec![0x11, 0x88, 0x56, 0xe5, 0x00]);
        let packet = adts::test_packet(1, 3, 2, false, 4);

        let (opened, got) = round_trip_coded(&stream, std::slice::from_ref(&packet))
            .expect("a stream with extradata already opens");
        assert_eq!(opened.extradata, stream.extradata, "not overwritten");
        assert_eq!(got, vec![packet], "the packet is handed through untouched");
    }

    #[test]
    fn packets_that_do_not_start_with_the_syncword_are_untouched() {
        let stream = a_coded_aac_stream(Vec::new());
        let packet = vec![0x21, 0x00, 0x10, 0x04];

        let (opened, got) = round_trip_coded(&stream, std::slice::from_ref(&packet))
            .expect("a non-ADTS first packet is not an error");
        assert!(opened.extradata.is_empty(), "nothing to derive it from");
        assert_eq!(got, vec![packet]);
    }

    #[test]
    fn a_packet_carrying_two_adts_frames_is_refused_not_split() {
        let stream = a_coded_aac_stream(Vec::new());
        let mut two_frames = adts::test_packet(1, 3, 2, false, 4);
        two_frames.extend(adts::test_packet(1, 3, 2, false, 4));
        let packets = vec![adts::test_packet(1, 3, 2, false, 4), two_frames];

        let err = round_trip_coded(&stream, &packets)
            .expect_err("a packet with two ADTS frames must be refused");
        assert!(
            err.to_string().contains("more than one ADTS frame"),
            "{err}"
        );
    }
}

/// What the muxer puts on the wire.
mod writing {
    use crate::*;

    fn a_stream() -> Stream {
        Stream::video("rgba", 2, 2, TimeBase { num: 1, den: 25 }).expect("rgba is carried")
    }

    /// An h264 stream header shaped like a real encoded one: SPS/PPS as
    /// extradata, and a decode delay of two - a reorder buffer of three pts -
    /// which is what a B-frame GOP needs.
    fn a_coded_h264_stream() -> Stream {
        Stream {
            fourcc: b"H264".to_vec(),
            time_base: TimeBase { num: 1, den: 25 },
            msb_pts_shift: 14,
            max_pts_distance: 25,
            decode_delay: 2,
            extradata: vec![0x67, 0x42, 0x00, 0x1e],
            frame_rate: Some((25, 1)),
            media: Media::Video {
                width: 16,
                height: 16,
                sample_width: 1,
                sample_height: 1,
                colorspace_type: 0,
            },
        }
    }

    /// An AAC stream header: ffmpeg's own codec tag for it, and no
    /// reordering.
    fn a_coded_aac_stream() -> Stream {
        Stream {
            fourcc: b"\xff\x00\x00\x00".to_vec(),
            time_base: TimeBase { num: 1, den: 48000 },
            msb_pts_shift: 14,
            max_pts_distance: 48000,
            decode_delay: 0,
            extradata: vec![0x11, 0x88, 0x56, 0xe5, 0x00],
            frame_rate: None,
            media: Media::Audio {
                sample_rate: 48000,
                channels: 1,
            },
        }
    }

    fn wire_with(frames: &[(i64, Vec<u8>)]) -> Vec<u8> {
        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::new(&mut wire, &a_stream()).expect("write headers");
            for (pts, data) in frames {
                muxer.write_frame(*pts, data).expect("write frame");
            }
            muxer.finish().expect("finish");
        }
        wire
    }

    #[test]
    fn the_stream_starts_with_the_identifier_and_the_main_startcode() {
        let wire = wire_with(&[]);
        assert_eq!(&wire[..FILE_ID.len()], FILE_ID);
        assert_eq!(
            u64::from_be_bytes(wire[FILE_ID.len()..FILE_ID.len() + 8].try_into().unwrap()),
            MAIN_STARTCODE
        );
    }

    #[test]
    fn a_syncpoint_precedes_the_first_frame() {
        let wire = wire_with(&[(0, vec![0u8; 16])]);
        let found = wire
            .windows(8)
            .position(|w| u64::from_be_bytes(w.try_into().unwrap()) == SYNCPOINT_STARTCODE);
        assert!(
            found.is_some(),
            "the first frame needs a syncpoint before it"
        );
    }

    #[test]
    fn syncpoints_come_no_further_apart_than_the_maximum_distance() {
        // Frames well under the distance, so several share one syncpoint.
        let frames: Vec<(i64, Vec<u8>)> = (0..64i64).map(|i| (i, vec![0u8; 1024])).collect();
        let wire = wire_with(&frames);
        let mut previous: Option<usize> = None;
        for start in 0..wire.len().saturating_sub(8) {
            let code = u64::from_be_bytes(wire[start..start + 8].try_into().unwrap());
            if code != SYNCPOINT_STARTCODE {
                continue;
            }
            if let Some(prev) = previous {
                assert!(
                    start - prev <= MAX_DISTANCE as usize + 2048,
                    "syncpoints {prev} and {start} are too far apart"
                );
            }
            previous = Some(start);
        }
        assert!(previous.is_some(), "there should be syncpoints");
    }

    #[test]
    fn a_negative_timestamp_is_refused() {
        let mut wire = Vec::new();
        let mut muxer = Muxer::new(&mut wire, &a_stream()).expect("write headers");
        let err = muxer.write_frame(-1, &[0u8; 16]).unwrap_err().to_string();
        assert!(err.contains("negative PTS"), "{err}");
    }

    #[test]
    fn write_coded_is_refused_on_a_raw_stream() {
        let mut wire = Vec::new();
        let mut muxer = Muxer::new(&mut wire, &a_stream()).expect("write headers");
        let packet = Packet {
            pts: 0,
            dts: None,
            keyframe: true,
        };
        let err = muxer
            .write_coded(&packet, &[0u8; 4])
            .unwrap_err()
            .to_string();
        assert!(err.contains("raw") && err.contains("write_frame"), "{err}");
    }

    #[test]
    fn write_frame_is_refused_on_a_coded_stream() {
        let mut wire = Vec::new();
        let mut muxer = Muxer::new(&mut wire, &a_coded_h264_stream()).expect("write headers");
        let err = muxer.write_frame(0, &[0u8; 4]).unwrap_err().to_string();
        assert!(
            err.contains("encoded") && err.contains("write_coded"),
            "{err}"
        );
    }

    #[test]
    fn coded_packets_round_trip_through_the_decode_order_reorder_buffer() {
        let stream = a_coded_h264_stream();
        // Decode order: I0 P3 B1 B2 P6 B4 B5, the shape one B-frame GOP
        // takes. Only the leading I frame is a keyframe.
        let packets: Vec<(i64, bool)> = vec![
            (0, true),
            (3, false),
            (1, false),
            (2, false),
            (6, false),
            (4, false),
            (5, false),
        ];
        // Worked out by hand from the reorder buffer of `decode_delay + 1`
        // entries: None until the buffer fills, then the smallest pts seen so
        // far that has not already come out.
        let expected_dts: [Option<i64>; 7] =
            [None, None, Some(0), Some(1), Some(2), Some(3), Some(4)];

        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
            for (index, (pts, keyframe)) in packets.iter().enumerate() {
                let packet = Packet {
                    pts: *pts,
                    dts: None,
                    keyframe: *keyframe,
                };
                muxer
                    .write_coded(&packet, &[index as u8; 4])
                    .expect("write coded packet");
            }
            muxer.finish().expect("finish");
        }

        let mut demuxer = Demuxer::open(&wire[..]).expect("read headers");
        assert_eq!(demuxer.stream().decode_delay, 2);
        assert_eq!(demuxer.stream().extradata, stream.extradata);
        assert_eq!(demuxer.stream().codec_name(), Some("h264"));
        assert_eq!(
            demuxer.stream().frame_rate,
            Some((25, 1)),
            "the info packet the muxer wrote says the rate"
        );

        let mut buf = Vec::new();
        let mut got = Vec::new();
        while let Some(packet) = demuxer.read_packet(&mut buf).expect("read coded packet") {
            got.push((packet, buf.clone()));
        }
        assert_eq!(got.len(), packets.len());
        for (index, ((packet, data), (pts, keyframe))) in got.iter().zip(&packets).enumerate() {
            assert_eq!(packet.pts, *pts, "packet {index} pts");
            assert_eq!(packet.keyframe, *keyframe, "packet {index} keyframe");
            assert_eq!(packet.dts, expected_dts[index], "packet {index} dts");
            assert_eq!(data, &vec![index as u8; 4], "packet {index} data");
        }
    }

    #[test]
    fn coded_aac_packets_round_trip_with_no_reordering() {
        let stream = a_coded_aac_stream();
        // Each packet is one AAC frame of 1024 samples; audio has no B-frames
        // so every packet is a keyframe and dts is pts.
        let packets: Vec<(i64, bool)> = (0..4i64).map(|i| (i * 1024, true)).collect();

        let mut wire = Vec::new();
        {
            let mut muxer = Muxer::new(&mut wire, &stream).expect("write headers");
            for (index, (pts, keyframe)) in packets.iter().enumerate() {
                let packet = Packet {
                    pts: *pts,
                    dts: None,
                    keyframe: *keyframe,
                };
                muxer
                    .write_coded(&packet, &[0x21, index as u8, 0x10, 0x04])
                    .expect("write coded packet");
            }
            muxer.finish().expect("finish");
        }

        let mut demuxer = Demuxer::open(&wire[..]).expect("read headers");
        assert_eq!(demuxer.stream().decode_delay, 0);
        assert_eq!(demuxer.stream().codec_name(), Some("aac"));

        let mut buf = Vec::new();
        let mut got = Vec::new();
        while let Some(packet) = demuxer.read_packet(&mut buf).expect("read coded packet") {
            got.push((packet, buf.clone()));
        }
        assert_eq!(got.len(), packets.len());
        for (index, ((packet, data), (pts, keyframe))) in got.iter().zip(&packets).enumerate() {
            assert_eq!(packet.pts, *pts, "packet {index} pts");
            assert_eq!(packet.dts, Some(*pts), "packet {index} dts: no reordering");
            assert_eq!(packet.keyframe, *keyframe, "packet {index} keyframe");
            assert_eq!(
                data,
                &vec![0x21, index as u8, 0x10, 0x04],
                "packet {index} data"
            );
        }
    }

    #[test]
    fn the_raw_path_writes_the_same_bytes_it_always_did() {
        // A pin over `write_frame`'s exact output, so a refactor that shares
        // machinery with `write_coded` cannot silently change it.
        let frames: Vec<(i64, Vec<u8>)> = (0..3i64).map(|i| (i, vec![i as u8; 16])).collect();
        let wire = wire_with(&frames);
        assert_eq!(
            Demuxer::open(&wire[..])
                .and_then(|mut d| {
                    let mut buf = Vec::new();
                    let mut out = Vec::new();
                    while let Some(pts) = d.read_frame(&mut buf)? {
                        out.push((pts, buf.clone()));
                    }
                    Ok(out)
                })
                .expect("round trip the raw wire"),
            frames
        );
    }
}

//! What a stream header says, and the codec tags this wire carries.

use crate::{AUDIO_CLASS, DATA_CLASS, JSON_FOURCC, SUBTITLE_CLASS, VIDEO_CLASS};

/// The unit PTS are counted in, as a rational number of seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeBase {
    pub num: u64,
    pub den: u64,
}

impl TimeBase {
    /// A timestamp in seconds.
    pub fn seconds(&self, pts: i64) -> f64 {
        pts as f64 * self.num as f64 / self.den as f64
    }
}

/// What one frame's own header said about it, past its bytes. For an encoded
/// stream a frame is a packet; the names differ, the wire does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
    /// Presentation timestamp, as the frame coded it. Frames arrive in
    /// decode order, so where the stream reorders this is not monotonic.
    pub pts: i64,
    /// Decode timestamp, worked out from the pts stream through a reorder
    /// buffer of `decode_delay + 1` entries, the way ffmpeg's own demuxing
    /// does. None for the first `decode_delay` frames, where the wire does
    /// not settle it. With no reordering it is the pts itself.
    pub dts: Option<i64>,
    /// The frame header's keyframe flag.
    pub keyframe: bool,
}

/// What a stream on this wire carries, past the fields every kind shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Video {
        width: u32,
        height: u32,
        sample_width: u64,
        sample_height: u64,
        colorspace_type: u64,
    },
    Audio {
        sample_rate: u32,
        channels: u32,
    },
    /// A stream of some other NUT class - subtitles, or data, which is what
    /// the annotation stream rides on. Its header carries no geometry, so
    /// this names the class and nothing else; its frames still arrive, with
    /// their timestamps, for whoever knows what is in them.
    Other {
        class: u64,
    },
}

/// One stream on this wire, as the stream header describes it. The muxer
/// writes an output header from an input's, so everything a consumer needs
/// survives the hop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    /// The codec tag. `RGBA`, `I420`, `Y42B`, `444P`, the two pcm tags and
    /// the coded tags of [`CODED_VIDEO_FOURCCS`] and [`CODED_AUDIO_FOURCCS`]
    /// are what this crate knows by name. Any other tag of four printable
    /// ASCII bytes on a video or audio stream is a coded stream named by its
    /// own text (see [`Stream::coded_fourcc`]); anything else is carried, not
    /// read.
    pub fourcc: Vec<u8>,
    pub time_base: TimeBase,
    /// How many low bits of a PTS a frame may code, instead of all of it.
    pub msb_pts_shift: u32,
    pub max_pts_distance: u64,
    /// How many packets the decoder holds back before the first picture
    /// leaves it. Zero for every raw stream; a coded stream with B-frames
    /// reorders, and this is by how much.
    pub decode_delay: u64,
    /// The codec's out-of-band header, from the stream header's
    /// codec-specific field: for h264, the SPS and PPS. Empty for raw
    /// streams.
    pub extradata: Vec<u8>,
    /// Frames per second as `num/den`, from the `r_frame_rate` an info
    /// packet states. None where nothing on the wire said it.
    ///
    /// NUT has no per-frame duration field, so this is the only thing that
    /// carries one: a reader works each packet's duration out of the rate.
    /// Without it a reordering stream loses its durations entirely, since
    /// the next packet READ is not the next picture SHOWN and no pair of
    /// timestamps settles the gap.
    pub frame_rate: Option<(u64, u64)>,
    pub media: Media,
}

/// The pixel formats this wire carries, and the codec tag ffmpeg's NUT muxer
/// writes for each: every plane 8 bits, the chroma planes of `yuv420p` half
/// the luma's width and height, of `yuv422p` half its width, of `yuv444p` the
/// same size. `gray` is the one plane.
const PIX_FMT_FOURCCS: &[(&str, &[u8; 4])] = &[
    ("rgba", b"RGBA"),
    ("yuv420p", b"I420"),
    ("yuv422p", b"Y42B"),
    ("yuv444p", b"444P"),
    ("gray", b"Y800"),
];

/// The sample formats this wire carries, and the codec tag ffmpeg gives each:
/// `pcm_f32le` and `pcm_s16le`, both interleaved.
const SAMPLE_FMT_FOURCCS: &[(&str, &[u8; 4])] = &[("f32", b"PFD\x20"), ("s16", b"PSD\x10")];

/// The coded video streams this wire knows by name, and the tags ffmpeg
/// gives each in NUT (its muxer writes the first; a demuxer accepts the
/// aliases too). Payloads stay opaque: a packet is handed through exactly as
/// it arrived. A coded stream under a tag no table names is carried too, and
/// named by the tag's own text; see [`Stream::coded_fourcc`].
pub const CODED_VIDEO_FOURCCS: &[(&str, &[&[u8; 4]])] = &[
    ("h264", &[b"H264", b"h264", b"avc1", b"AVC1"]),
    ("hevc", &[b"HEVC", b"hevc", b"hev1", b"hvc1"]),
    ("av1", &[b"AV01", b"av01"]),
];

/// The coded audio streams this wire carries, and the tags ffmpeg gives each
/// in NUT. Its muxer writes `ff 00 00 00` for AAC, and puts the codec's
/// AudioSpecificConfig in the stream header's extradata; `mp4a` is the tag
/// the same codec takes in mp4, accepted here as an alias. An aac stream
/// remuxed `-c copy` off ADTS (HLS, mpegts) carries no extradata at all - the
/// blocking [`Demuxer`](crate::Demuxer) derives it from the first packet's
/// ADTS header instead, and strips that header from every packet; see
/// [`adts`](crate::adts).
pub const CODED_AUDIO_FOURCCS: &[(&str, &[&[u8; 4]])] =
    &[("aac", &[b"\xff\x00\x00\x00", b"mp4a", b"MP4A"])];

impl Stream {
    /// A video stream of `pix_fmt` frames, for building a header from nothing
    /// but geometry. `pix_fmt` is one of [`supported_pix_fmts`]: `rgba`,
    /// `yuv420p`, `yuv422p`, `yuv444p` or `gray`. None for any other.
    pub fn video(pix_fmt: &str, width: u32, height: u32, time_base: TimeBase) -> Option<Stream> {
        Some(Stream {
            fourcc: fourcc_for_pix_fmt(pix_fmt)?.to_vec(),
            time_base,
            msb_pts_shift: 14,
            max_pts_distance: time_base.den.div_ceil(time_base.num.max(1)),
            decode_delay: 0,
            extradata: Vec::new(),
            frame_rate: None,
            media: Media::Video {
                width,
                height,
                sample_width: 1,
                sample_height: 1,
                colorspace_type: 0,
            },
        })
    }

    /// An audio stream of interleaved `sample_fmt` samples. The time base is
    /// the natural one, where a tick is a sample.
    pub fn audio(sample_fmt: &str, sample_rate: u32, channels: u32) -> Option<Stream> {
        let time_base = TimeBase {
            num: 1,
            den: u64::from(sample_rate),
        };
        Some(Stream {
            fourcc: fourcc_for_sample_fmt(sample_fmt)?.to_vec(),
            time_base,
            msb_pts_shift: 14,
            max_pts_distance: u64::from(sample_rate),
            decode_delay: 0,
            extradata: Vec::new(),
            frame_rate: None,
            media: Media::Audio {
                sample_rate,
                channels,
            },
        })
    }

    /// A data stream of JSON messages; see [`JSON_FOURCC`]. Messages are
    /// sparse, so the time base is whatever the caller counts them in -
    /// microseconds, say - and a jump between two of them codes its PTS in
    /// full.
    pub fn json(time_base: TimeBase) -> Stream {
        Stream {
            fourcc: JSON_FOURCC.to_vec(),
            time_base,
            msb_pts_shift: 14,
            max_pts_distance: time_base.den.div_ceil(time_base.num.max(1)),
            decode_delay: 0,
            extradata: Vec::new(),
            frame_rate: None,
            media: Media::Other { class: DATA_CLASS },
        }
    }

    /// A coded stream under a tag of the caller's own, for a codec no table
    /// names: `PYRW`, say. The tag is written as given and read back as the
    /// codec's name, so it must be exactly four printable ASCII bytes.
    /// `kind` is `"video"` or `"audio"`, and `geometry` is what
    /// [`video_geometry`](Stream::video_geometry) or
    /// [`audio_geometry`](Stream::audio_geometry) hands back for that kind:
    /// width and height, or rate and channel count. The time base, the
    /// codec's out-of-band header and how far it reorders are the codec's
    /// own, so they are taken as given too.
    ///
    /// A tag the tables name for `kind` builds that named stream, since it
    /// is one. None for a tag that is not four printable ASCII bytes, one
    /// this wire carries raw (`RGBA`, say), one the other kind's table
    /// names, or a `kind` that names neither.
    pub fn coded_fourcc(
        kind: &str,
        fourcc: &[u8],
        geometry: (u32, u32),
        time_base: TimeBase,
        extradata: Vec<u8>,
        decode_delay: u64,
    ) -> Option<Stream> {
        fourcc_text(fourcc)?;
        let (first, second) = geometry;
        let media = match kind {
            "video" => Media::Video {
                width: first,
                height: second,
                sample_width: 1,
                sample_height: 1,
                colorspace_type: 0,
            },
            "audio" => Media::Audio {
                sample_rate: first,
                channels: second,
            },
            _ => return None,
        };
        let stream = Stream {
            fourcc: fourcc.to_vec(),
            time_base,
            msb_pts_shift: 14,
            max_pts_distance: time_base.den.div_ceil(time_base.num.max(1)),
            decode_delay,
            extradata,
            frame_rate: None,
            media,
        };
        stream.codec_name().is_some().then_some(stream)
    }

    /// Whether this is a data stream of JSON messages.
    pub fn is_json(&self) -> bool {
        self.media == (Media::Other { class: DATA_CLASS }) && self.fourcc == JSON_FOURCC
    }

    /// The NUT stream class this stream's media is written as.
    pub fn class(&self) -> u64 {
        match self.media {
            Media::Video { .. } => VIDEO_CLASS,
            Media::Audio { .. } => AUDIO_CLASS,
            Media::Other { class } => class,
        }
    }

    /// The pixel format the codec tag names, or None for anything that is not
    /// a video stream this crate knows.
    pub fn pix_fmt(&self) -> Option<&'static str> {
        matches!(self.media, Media::Video { .. })
            .then(|| named(PIX_FMT_FOURCCS, &self.fourcc))
            .flatten()
    }

    /// ffmpeg's name for the coded codec the tag names, from the table for
    /// this stream's own kind, or `json` for a data stream of JSON messages.
    /// A video or audio tag no table names is a coded stream all the same,
    /// named by its own text (`PYRW`), when it is four printable ASCII bytes
    /// and not a tag this wire carries raw. None for a raw stream, for a tag
    /// the other kind's table names, and for any other tag.
    pub fn codec_name(&self) -> Option<&str> {
        let (table, other, raw) = match self.media {
            Media::Video { .. } => (CODED_VIDEO_FOURCCS, CODED_AUDIO_FOURCCS, self.pix_fmt()),
            Media::Audio { .. } => (CODED_AUDIO_FOURCCS, CODED_VIDEO_FOURCCS, self.sample_fmt()),
            Media::Other { .. } => return self.is_json().then_some("json"),
        };
        if let Some(name) = coded_name(table, &self.fourcc) {
            return Some(name);
        }
        if raw.is_some() || coded_name(other, &self.fourcc).is_some() {
            return None;
        }
        fourcc_text(&self.fourcc)
    }

    /// The sample format the codec tag names, or None for anything that is not
    /// an audio stream this crate knows.
    pub fn sample_fmt(&self) -> Option<&'static str> {
        matches!(self.media, Media::Audio { .. })
            .then(|| named(SAMPLE_FMT_FOURCCS, &self.fourcc))
            .flatten()
    }

    /// The frame geometry, for a video stream.
    pub fn video_geometry(&self) -> Option<(u32, u32)> {
        match self.media {
            Media::Video { width, height, .. } => Some((width, height)),
            _ => None,
        }
    }

    /// The rate and channel count, for an audio stream.
    pub fn audio_geometry(&self) -> Option<(u32, u32)> {
        match self.media {
            Media::Audio {
                sample_rate,
                channels,
            } => Some((sample_rate, channels)),
            _ => None,
        }
    }

    /// `video` or `audio`, for a message.
    pub fn kind(&self) -> &'static str {
        match self.media {
            Media::Video { .. } => "video",
            Media::Audio { .. } => "audio",
            Media::Other { class } if class == SUBTITLE_CLASS => "subtitle",
            Media::Other { class } if class == DATA_CLASS => "data",
            Media::Other { .. } => "unknown",
        }
    }

    /// The codec tag as something safe to put in an error message.
    pub fn fourcc_name(&self) -> String {
        String::from_utf8_lossy(&self.fourcc)
            .chars()
            .map(|c| {
                if c.is_ascii_graphic() || c == ' ' {
                    c
                } else {
                    '?'
                }
            })
            .collect()
    }
}

/// The name a coded table gives a codec tag, among its aliases.
fn coded_name(table: &[(&'static str, &[&[u8; 4]])], fourcc: &[u8]) -> Option<&'static str> {
    table
        .iter()
        .find(|(_, tags)| tags.iter().any(|tag| fourcc == tag.as_slice()))
        .map(|(name, _)| *name)
}

/// A codec tag as the text it spells, when it is exactly four printable
/// ASCII bytes.
fn fourcc_text(fourcc: &[u8]) -> Option<&str> {
    if fourcc.len() != 4 || !fourcc.iter().all(|b| (0x20..=0x7e).contains(b)) {
        return None;
    }
    std::str::from_utf8(fourcc).ok()
}

/// The name a table gives a codec tag.
fn named(table: &[(&'static str, &[u8; 4])], fourcc: &[u8]) -> Option<&'static str> {
    table
        .iter()
        .find(|(_, tag)| fourcc == tag.as_slice())
        .map(|(name, _)| *name)
}

/// The codec tag ffmpeg gives `pix_fmt` in NUT.
pub fn fourcc_for_pix_fmt(pix_fmt: &str) -> Option<&'static [u8; 4]> {
    PIX_FMT_FOURCCS
        .iter()
        .find(|(name, _)| *name == pix_fmt)
        .map(|(_, tag)| *tag)
}

/// The codec tag ffmpeg gives the pcm codec of `sample_fmt` in NUT.
pub fn fourcc_for_sample_fmt(sample_fmt: &str) -> Option<&'static [u8; 4]> {
    SAMPLE_FMT_FOURCCS
        .iter()
        .find(|(name, _)| *name == sample_fmt)
        .map(|(_, tag)| *tag)
}

/// The codec tag ffmpeg gives `codec` in NUT, from the table for `kind`
/// (`"video"` or `"audio"`) - the muxer's own tag, the first of the aliases
/// a demuxer also accepts. None for a codec this wire does not carry, or a
/// `kind` that names neither.
pub fn fourcc_for_coded(kind: &str, codec: &str) -> Option<&'static [u8; 4]> {
    let table = match kind {
        "video" => CODED_VIDEO_FOURCCS,
        "audio" => CODED_AUDIO_FOURCCS,
        _ => return None,
    };
    table
        .iter()
        .find(|(name, _)| *name == codec)
        .map(|(_, tags)| tags[0])
}

/// The pixel formats this wire carries, most common first.
pub fn supported_pix_fmts() -> Vec<&'static str> {
    PIX_FMT_FOURCCS.iter().map(|(name, _)| *name).collect()
}

/// The sample formats this wire carries, most common first.
pub fn supported_sample_fmts() -> Vec<&'static str> {
    SAMPLE_FMT_FOURCCS.iter().map(|(name, _)| *name).collect()
}

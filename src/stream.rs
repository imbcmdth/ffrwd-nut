//! What a stream header says, and the codec tags this wire carries.

use crate::{AUDIO_CLASS, DATA_CLASS, SUBTITLE_CLASS, VIDEO_CLASS};

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
    /// The codec tag. `RGBA`, `I420`, the two pcm tags and the coded tags of
    /// [`CODED_VIDEO_FOURCCS`] and [`CODED_AUDIO_FOURCCS`] are what this
    /// crate knows by name; any other tag is carried, not read.
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

/// The pixel formats this wire carries, and the codec tag ffmpeg gives each.
const PIX_FMT_FOURCCS: &[(&str, &[u8; 4])] = &[("rgba", b"RGBA"), ("yuv420p", b"I420")];

/// The sample formats this wire carries, and the codec tag ffmpeg gives each:
/// `pcm_f32le` and `pcm_s16le`, both interleaved.
const SAMPLE_FMT_FOURCCS: &[(&str, &[u8; 4])] = &[("f32", b"PFD\x20"), ("s16", b"PSD\x10")];

/// The coded video streams this wire carries, and the tags ffmpeg gives each
/// in NUT (its muxer writes the first; a demuxer accepts the aliases too).
/// Payloads stay opaque: a packet is handed through exactly as it arrived.
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
    /// but geometry.
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
    /// this stream's own kind. None for a raw stream and for any codec this
    /// wire does not carry.
    pub fn codec_name(&self) -> Option<&'static str> {
        let table = match self.media {
            Media::Video { .. } => CODED_VIDEO_FOURCCS,
            Media::Audio { .. } => CODED_AUDIO_FOURCCS,
            Media::Other { .. } => return None,
        };
        table
            .iter()
            .find(|(_, tags)| tags.iter().any(|tag| self.fourcc == tag.as_slice()))
            .map(|(name, _)| *name)
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

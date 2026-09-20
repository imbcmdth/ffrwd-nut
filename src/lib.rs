//! NUT, the container ffrwd puts on the pipes between its processes.
//!
//! Raw frames cannot carry a timestamp; NUT can, ffmpeg reads and writes it
//! natively on a pipe, and it costs a few dozen bytes per frame. Only the
//! subset that goes on those pipes is implemented: NUT version 3, carrying
//! uncompressed video frames, interleaved pcm, or the encoded packets of
//! [`CODED_VIDEO_FOURCCS`] and [`CODED_AUDIO_FOURCCS`]. Anything else is
//! refused by name rather than guessed at.
//!
//! What is read: the main header (version, stream count, time bases, the
//! framecode table, elision headers), the stream headers (fourcc, the
//! geometry the class calls for, time base, pts coding, codec extradata),
//! syncpoints, the `r_frame_rate` an info packet states, and frames with
//! their coded PTS and keyframe flag. Index packets are stepped over, as is
//! any startcode this subset does not know.
//!
//! Payloads are opaque whichever kind the stream is: a video packet is a
//! frame - encoded bytes or raw pixels - and an audio packet is however many
//! samples the producer chose, and none is looked inside.
//!
//! What is written: the same headers, one framecode that codes every field
//! explicitly, a syncpoint whenever the last one is further back than
//! [`MAX_DISTANCE`], and frames carrying their PTS and a header checksum. A
//! raw stream's frames are always keyframes; an encoded stream's packets
//! state their own keyframe flag and must be handed over in decode order, so
//! a reader can work their dts back out the way it works a demuxed input's.
//!
//! # The two front ends
//!
//! [`PushDemuxer`] is the parser: bytes go in as they arrive, in pieces of
//! any size, and events come out as they complete. It never reads, blocks or
//! seeks, which is what a wasm module with one non-blocking socket and no
//! threads needs.
//!
//! [`Demuxer`] is that parser with a loop around it that fills it from a
//! `Read`. It carries the one media stream this wire puts on an ffmpeg-facing
//! pipe, and the two things a reader that can wait knows how to do: deriving
//! an aac stream's config from the ADTS header its first packet carries, and
//! reading the annotation stream described below.
//!
//! # The annotation stream
//!
//! A sidecar that emits rows can put them on the wire beside the frames, so
//! the next sidecar reads both from one pipe. That is stream 1: codec tag
//! [`ANNOTATION_FOURCC`], stream class [`ANNOTATION_CLASS`], the media
//! stream's time base, one packet per frame that has rows, its payload the
//! rows as NDJSON and its PTS the frame's. A frame with no rows gets no
//! packet, and the packet is written before the frame it belongs to.
//!
//! Rows a module had no frame to put them on ride one further packet, after
//! every frame's: a single JSON object `{"trailing": [...]}` rather than
//! NDJSON, which is what tells the two apart. It comes last and there is at
//! most one.
//!
//! **This stream is private to these two ends.** ffmpeg has no codec for the
//! tag and no reason to be handed it: the annotated form belongs on a
//! sidecar-to-sidecar pipe only, and both ends opt into it. Every
//! ffmpeg-facing edge stays the single-stream subset above, and a second
//! stream arriving there is still refused by name. Telling the record from
//! the rows means parsing JSON, which the format itself never needs, so the
//! whole stream sits behind the `annotations` feature and the crate has no
//! dependencies without it.

pub mod adts;
mod bytes;
mod demux;
mod error;
mod mux;
mod read;
mod stream;

pub use bytes::crc32;
pub use demux::{Event, Info, Limits, MainHeader, PushDemuxer};
pub use error::{Error, ErrorKind, Result};
pub use mux::Muxer;
pub use read::Demuxer;
pub use stream::{
    fourcc_for_coded, fourcc_for_pix_fmt, fourcc_for_sample_fmt, supported_pix_fmts,
    supported_sample_fmts, Media, Packet, Stream, TimeBase, CODED_AUDIO_FOURCCS,
    CODED_VIDEO_FOURCCS,
};

/// The 25 bytes every NUT file starts with.
pub const FILE_ID: &[u8] = b"nut/multimedia container\0";

/// The NUT version read and written. Version 4 adds per-frame side data and
/// broadcast timestamps, neither of which belongs on this wire.
pub const VERSION: u64 = 3;

/// How far apart syncpoints may be, in bytes. ffmpeg's own value; the format
/// caps it at 65536.
pub const MAX_DISTANCE: u64 = 32767;

/// The stream the annotation packets ride on, beside the media stream's
/// stream 0.
pub const ANNOTATION_STREAM_ID: u64 = 1;

/// NUT's stream class for data that is neither video, audio nor subtitles.
pub const ANNOTATION_CLASS: u64 = 3;

/// The codec tag on the annotation stream. Private to this crate's two ends:
/// no ffmpeg build has a decoder for it, by design.
pub const ANNOTATION_FOURCC: &[u8; 4] = b"FRWD";

/// The only key of the record trailing rows ride in.
pub const TRAILING_KEY: &str = "trailing";

pub const MAIN_STARTCODE: u64 = 0x4E4D_7A56_1F5F_04AD;
pub const STREAM_STARTCODE: u64 = 0x4E53_1140_5BF2_F9DB;
pub const SYNCPOINT_STARTCODE: u64 = 0x4E4B_E4AD_EECA_4569;
pub const INDEX_STARTCODE: u64 = 0x4E58_DD67_2F23_E64E;
pub const INFO_STARTCODE: u64 = 0x4E49_AB68_B596_BA78;

/// NUT's stream classes.
pub const VIDEO_CLASS: u64 = 0;
pub const AUDIO_CLASS: u64 = 1;
pub const SUBTITLE_CLASS: u64 = 2;
pub const DATA_CLASS: u64 = 3;

/// Frame flags, as they appear in the framecode table and in a frame's own
/// `coded_flags`.
pub mod flags {
    pub const KEY: u64 = 1;
    pub const EOR: u64 = 2;
    pub const CODED_PTS: u64 = 8;
    pub const STREAM_ID: u64 = 16;
    pub const SIZE_MSB: u64 = 32;
    pub const CHECKSUM: u64 = 64;
    pub const RESERVED: u64 = 128;
    pub const SM_DATA: u64 = 256;
    pub const HEADER_IDX: u64 = 1024;
    pub const MATCH_TIME: u64 = 2048;
    pub const CODED: u64 = 4096;
    pub const INVALID: u64 = 8192;
}

#[cfg(test)]
mod tests;

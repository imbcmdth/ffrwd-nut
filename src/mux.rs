//! Writing NUT: the headers that describe the stream, and frames that carry
//! their PTS. What comes out is what real ffmpeg reads with `-f nut`.

use std::io::Write;

use crate::bytes::{crc32, put_s, put_u32, put_u64, put_v, put_vb};
#[cfg(feature = "annotations")]
use crate::error::Error;
use crate::error::{bail, Result};
use crate::SYNCPOINT_STARTCODE;
use crate::{flags, Media, Packet, Stream, TimeBase};
#[cfg(feature = "annotations")]
use crate::{ANNOTATION_CLASS, ANNOTATION_FOURCC, TRAILING_KEY};
use crate::{FILE_ID, INFO_STARTCODE, MAIN_STARTCODE, MAX_DISTANCE, STREAM_STARTCODE, VERSION};

/// The framecode every frame uses. It sets only `CODED`, so each frame states
/// the rest of its flags itself: one table entry serves every frame, and the
/// table needs no tuning to the stream - a raw frame's fixed flags and a
/// coded packet's per-packet flags both ride through it unchanged.
const EXPLICIT_FRAME_CODE: u8 = 1;

/// What each raw frame states. Uncompressed frames are all keyframes; the PTS
/// and the size are always coded, and the header checksum keeps the frame
/// readable however far it sits from a syncpoint.
const FRAME_FLAGS: u64 = flags::KEY | flags::CODED_PTS | flags::SIZE_MSB | flags::CHECKSUM;

/// What an encoded packet states, past its own keyframe flag: the PTS and the
/// size are always coded, and the header checksum keeps the frame readable
/// however far it sits from a syncpoint. `write_coded` adds `flags::KEY`
/// itself, per packet, since only the packet knows.
const CODED_FRAME_FLAGS: u64 = flags::CODED_PTS | flags::SIZE_MSB | flags::CHECKSUM;

/// The framecode table's size multiplier. With it at 1 a frame's coded size
/// is its byte count.
const SIZE_MUL: u64 = 1;

/// A packet body this wide would need a checksum over its own header. The
/// headers written here are tens of bytes, so it is a guard, not a case.
const HEADER_CHECKSUM_THRESHOLD: usize = 4096;

/// Writes NUT: headers from one [`Stream`] or several, then frames. With
/// annotations on it writes one more stream after the media ones, for the
/// rows a module emitted; see the crate documentation for who may read it.
pub struct Muxer<W> {
    out: W,
    streams: Vec<Stream>,
    /// The time bases the main header declares, each once, in the order the
    /// streams first name them; and which of them each stream counts in.
    time_bases: Vec<TimeBase>,
    time_base_of: Vec<u64>,
    /// The annotation stream's id, where it was declared. Nothing reads it
    /// without the feature that writes to it.
    #[cfg_attr(not(feature = "annotations"), allow(dead_code))]
    annotations: Option<u64>,
    pos: u64,
    last_syncpoint: Option<u64>,
}

impl<W: Write> Muxer<W> {
    /// Writes the identifier and both headers and flushes them, so the reader
    /// on the far side knows the geometry before the first frame arrives.
    pub fn new(out: W, stream: &Stream) -> Result<Muxer<W>> {
        Muxer::open(out, std::slice::from_ref(stream), false)
    }

    /// `new`, plus the annotation stream. Only another ffrwd sidecar reads
    /// what this writes.
    #[cfg(feature = "annotations")]
    pub fn with_annotations(out: W, stream: &Stream) -> Result<Muxer<W>> {
        Muxer::open(out, std::slice::from_ref(stream), true)
    }

    /// Several streams on one wire, numbered in the order given: what one
    /// edge carrying a picture, its sound and a data stream beside them is.
    /// The frames of all of them interleave in the order they are written,
    /// which a reader takes as the order to hand them on in.
    pub fn with_streams(out: W, streams: &[Stream]) -> Result<Muxer<W>> {
        Muxer::open(out, streams, false)
    }

    /// `with_streams`, plus the annotation stream after the last of them.
    #[cfg(feature = "annotations")]
    pub fn with_streams_annotated(out: W, streams: &[Stream]) -> Result<Muxer<W>> {
        Muxer::open(out, streams, true)
    }

    fn open(out: W, streams: &[Stream], annotations: bool) -> Result<Muxer<W>> {
        if streams.is_empty() {
            bail!(unsupported: "NUT output with no stream to carry");
        }
        let mut time_bases: Vec<TimeBase> = Vec::new();
        let mut time_base_of = Vec::with_capacity(streams.len());
        for stream in streams {
            if stream.msb_pts_shift >= 63 {
                bail!(
                    unsupported: "NUT output would shift PTS by {} bits",
                    stream.msb_pts_shift
                );
            }
            let index = match time_bases.iter().position(|tb| *tb == stream.time_base) {
                Some(index) => index,
                None => {
                    time_bases.push(stream.time_base);
                    time_bases.len() - 1
                }
            };
            time_base_of.push(index as u64);
        }
        let mut muxer = Muxer {
            out,
            streams: streams.to_vec(),
            time_bases,
            time_base_of,
            annotations: annotations.then_some(streams.len() as u64),
            pos: 0,
            last_syncpoint: None,
        };
        muxer.write_bytes(FILE_ID)?;
        let main = main_header(&muxer.time_bases, streams.len() + usize::from(annotations));
        muxer.write_packet(MAIN_STARTCODE, &main)?;
        for (id, stream) in streams.iter().enumerate() {
            let header = stream_header(id as u64, muxer.time_base_of[id], stream);
            muxer.write_packet(STREAM_STARTCODE, &header)?;
        }
        #[cfg(feature = "annotations")]
        if let Some(id) = muxer.annotations {
            let header = annotation_stream_header(id, muxer.time_base_of[0], &muxer.streams[0]);
            muxer.write_packet(STREAM_STARTCODE, &header)?;
        }
        // The frame rate, where a stream carries one. NUT has no duration
        // field, so this info packet is the only thing that tells a reader
        // how long a packet is shown for - and on a reordering stream it is
        // the only thing that CAN, since the next packet read is not the
        // next picture shown. It goes out with the headers, before the first
        // frame, because a reader that learns the rate later has already
        // handed out packets without it.
        for id in 0..muxer.streams.len() {
            if let Some((num, den)) = muxer.streams[id].frame_rate {
                let info = frame_rate_info(id as u64, num, den);
                muxer.write_packet(INFO_STARTCODE, &info)?;
            }
        }
        // A buffered writer would otherwise hold the headers until a
        // megabyte of frames pushed them out, and a reader waiting on them
        // waits that long.
        muxer.out.flush()?;
        Ok(muxer)
    }

    /// The first stream these headers describe, which is the only one on a
    /// wire opened with [`Muxer::new`].
    pub fn stream(&self) -> &Stream {
        &self.streams[0]
    }

    /// Every stream these headers describe, in id order.
    pub fn streams(&self) -> &[Stream] {
        &self.streams
    }

    /// One frame, at `pts` in the stream's time base. Refused on an encoded
    /// stream: an encoded packet states its own keyframe flag, which
    /// `write_coded` carries and this fixed-flags path does not.
    pub fn write_frame(&mut self, pts: i64, data: &[u8]) -> Result<()> {
        self.write_frame_to(0, pts, data)
    }

    /// `write_frame`, onto stream `stream` of several.
    pub fn write_frame_to(&mut self, stream: usize, pts: i64, data: &[u8]) -> Result<()> {
        let Some(declared) = self.streams.get(stream) else {
            bail!(unsupported: "NUT output has no stream {stream}");
        };
        if declared.codec_name().is_some() {
            bail!(
                unsupported: "write_frame on an encoded {} stream; use write_coded",
                declared.fourcc_name()
            );
        }
        self.write_packet_for(stream as u64, pts, FRAME_FLAGS, data)
    }

    /// One encoded packet, in the decode order it must be handed to this
    /// method in. Refused on a raw stream: use `write_frame` there instead.
    ///
    /// `packet.pts` is written as `write_frame` writes a raw frame's; NUT
    /// carries no dts field at all, so `packet.dts` is not written, and a
    /// reader - this crate's own demuxer included - works dts back out of
    /// the pts values it receives, in the order it receives them, through a
    /// reorder buffer of `decode_delay + 1` entries. That is also why the
    /// packets must arrive here in decode order: the buffer that recovers
    /// dts on the way out depends on it. `packet.keyframe` sets `flags::KEY`
    /// on this packet alone, which is what lets key and non-key packets share
    /// the wire. Nothing here carries a duration: what NUT implies from the
    /// next packet's pts is all a reader gets back.
    pub fn write_coded(&mut self, packet: &Packet, data: &[u8]) -> Result<()> {
        self.write_coded_to(0, packet, data)
    }

    /// `write_coded`, onto stream `stream` of several.
    pub fn write_coded_to(&mut self, stream: usize, packet: &Packet, data: &[u8]) -> Result<()> {
        let Some(declared) = self.streams.get(stream) else {
            bail!(unsupported: "NUT output has no stream {stream}");
        };
        if declared.codec_name().is_none() {
            bail!(
                unsupported: "write_coded on a raw {} stream; use write_frame",
                declared.fourcc_name()
            );
        }
        let frame_flags = CODED_FRAME_FLAGS | if packet.keyframe { flags::KEY } else { 0 };
        self.write_packet_for(stream as u64, packet.pts, frame_flags, data)
    }

    /// The rows a module produced for the frame at `pts`, as NDJSON, on the
    /// annotation stream. Nothing is written for a frame with no rows, and
    /// the packet precedes its frame so a reader has it in hand when the
    /// frame arrives.
    #[cfg(feature = "annotations")]
    pub fn write_rows(&mut self, pts: i64, rows: &[String]) -> Result<()> {
        let Some(id) = self.annotations else {
            bail!(unsupported: "NUT output carries no annotation stream, so rows have nowhere to go");
        };
        if rows.is_empty() {
            return Ok(());
        }
        self.write_packet_for(id, pts, FRAME_FLAGS, rows.join("\n").as_bytes())
    }

    /// The rows a module had no frame to put them on, as one record after
    /// every frame's. `pts` is where the stream got to, which is the last
    /// frame's timestamp.
    #[cfg(feature = "annotations")]
    pub fn write_trailing(&mut self, pts: i64, rows: &[String]) -> Result<()> {
        let Some(id) = self.annotations else {
            bail!(
                unsupported: "NUT output carries no annotation stream, so trailing rows have \
                 nowhere to go"
            );
        };
        if rows.is_empty() {
            return Ok(());
        }
        let record = trailing_record(rows)?;
        self.write_packet_for(id, pts, FRAME_FLAGS, record.as_bytes())
    }

    /// One frame on `stream_id`, at `pts` in the stream's time base, stating
    /// `frame_flags` - the fixed flags of a raw frame, or the per-packet
    /// flags `write_coded` works out from the packet's own keyframe bit. The
    /// framecode table's one entry states `CODED` for every frame, so
    /// whichever flags are passed in are what a reader gets back out.
    fn write_packet_for(
        &mut self,
        stream_id: u64,
        pts: i64,
        frame_flags: u64,
        data: &[u8],
    ) -> Result<()> {
        if pts < 0 {
            bail!(unsupported: "NUT output cannot carry the negative PTS {pts}");
        }
        // The annotation stream counts in the first media stream's time base
        // and codes its PTS the same way.
        let media = if (stream_id as usize) < self.streams.len() {
            stream_id as usize
        } else {
            0
        };
        if self
            .last_syncpoint
            .is_none_or(|prev| self.pos - prev >= MAX_DISTANCE)
        {
            self.write_syncpoint(pts, self.time_base_of[media])?;
        }

        // The whole PTS, offset past the range reserved for frames coding
        // only its low bits. Absolute means a frame never depends on how far
        // the reader has got, which is what keeps a filtered stream's
        // timestamps exactly the ones that came in.
        let coded_pts = (pts as u64) + (1u64 << self.streams[media].msb_pts_shift);
        // The framecode table's entry names stream 0, so every other stream
        // states its id itself.
        let frame_flags = if stream_id == 0 {
            frame_flags
        } else {
            frame_flags | flags::STREAM_ID
        };

        let mut header = Vec::with_capacity(16);
        header.push(EXPLICIT_FRAME_CODE);
        put_v(&mut header, frame_flags ^ flags::CODED);
        if stream_id != 0 {
            put_v(&mut header, stream_id);
        }
        put_v(&mut header, coded_pts);
        // The table's size multiplier is one, so a frame's coded size is its
        // byte count.
        put_v(&mut header, data.len() as u64);
        let checksum = crc32(&header);
        put_u32(&mut header, checksum);

        self.write_bytes(&header)?;
        self.write_bytes(data)
    }

    /// Hands everything written so far to the writer underneath and flushes
    /// it. NUT has no trailer, so this ends nothing: a writer on a pipe calls
    /// it after each batch of frames, because a reader cannot finish opening
    /// the stream until the first syncpoint has reached it, and that rides
    /// the first frame.
    pub fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }

    /// The end of the stream, which for NUT is only a flush. Kept for callers
    /// that mean "I am done"; [`Muxer::flush`] is the same thing said mid
    /// stream.
    pub fn finish(&mut self) -> Result<()> {
        self.flush()
    }

    /// The writer underneath, once the stream is written.
    pub fn into_inner(self) -> W {
        self.out
    }

    /// A syncpoint states where the stream has got to, and how far back the
    /// previous one was. For a raw stream every frame is a keyframe, so the
    /// last one before it is this syncpoint itself; an encoded stream places
    /// one wherever `MAX_DISTANCE` is crossed regardless of which packet that
    /// lands on, since nothing on this wire ever demuxes by seeking to one.
    fn write_syncpoint(&mut self, pts: i64, time_base: u64) -> Result<()> {
        let here = self.pos;
        let back_ptr = self.last_syncpoint.map_or(0, |prev| (here - prev) / 16);
        let mut body = Vec::with_capacity(8);
        // The timestamp in one declared time base, as `ticks * count +
        // index`; with one time base that is the ticks themselves.
        let count = self.time_bases.len() as u64;
        put_v(&mut body, (pts as u64).wrapping_mul(count) + time_base);
        put_v(&mut body, back_ptr);
        self.write_packet(SYNCPOINT_STARTCODE, &body)?;
        self.last_syncpoint = Some(here);
        Ok(())
    }

    /// A startcode, the length of what follows, and the body with its
    /// checksum appended.
    fn write_packet(&mut self, startcode: u64, fields: &[u8]) -> Result<()> {
        let mut body = fields.to_vec();
        put_u32(&mut body, crc32(fields));
        if body.len() > HEADER_CHECKSUM_THRESHOLD {
            bail!(
                limit: "NUT output packet is {} bytes, past the {HEADER_CHECKSUM_THRESHOLD} at \
                 which a packet must carry a checksum over its own header",
                body.len()
            );
        }
        let mut packet = Vec::with_capacity(body.len() + 16);
        put_u64(&mut packet, startcode);
        put_v(&mut packet, body.len() as u64);
        packet.extend_from_slice(&body);
        self.write_bytes(&packet)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.out.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }
}

/// The rows written verbatim into the one record trailing rows ride in. Each
/// is checked to be JSON first, so what goes on the wire is one well-formed
/// object rather than a record a reader cannot parse.
#[cfg(feature = "annotations")]
fn trailing_record(rows: &[String]) -> Result<String> {
    let mut record = format!("{{\"{TRAILING_KEY}\":[");
    for (index, row) in rows.iter().enumerate() {
        let row = row.trim();
        serde_json::from_str::<serde::de::IgnoredAny>(row).map_err(|e| {
            Error::format(format!(
                "trailing row {index} is not JSON, so it cannot ride in one record: {e}"
            ))
        })?;
        if index > 0 {
            record.push(',');
        }
        record.push_str(row);
    }
    record.push_str("]}");
    Ok(record)
}

/// The main header: how many streams, the time bases they count in, and a
/// framecode table with a single usable entry.
fn main_header(time_bases: &[TimeBase], streams: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(64);
    put_v(&mut body, VERSION);
    put_v(&mut body, streams as u64);
    put_v(&mut body, MAX_DISTANCE);
    put_v(&mut body, time_bases.len() as u64);
    for time_base in time_bases {
        put_v(&mut body, time_base.num);
        put_v(&mut body, time_base.den);
    }

    // Index 0, then index 1 - the one `EXPLICIT_FRAME_CODE` names - then
    // everything above it. A group's count leaves out index `N`, which is
    // reserved and always invalid: the last group covers 254 entries and
    // counts 253 of them.
    put_frame_code_group(&mut body, flags::INVALID, 0, 1);
    put_frame_code_group(&mut body, flags::CODED, SIZE_MUL, 1);
    put_frame_code_group(&mut body, flags::INVALID, 0, 253);

    put_v(&mut body, 0); // no elision headers
    body
}

/// One run of framecode table entries, with every field stated.
fn put_frame_code_group(body: &mut Vec<u8>, code_flags: u64, size_mul: u64, count: u64) {
    put_v(body, code_flags);
    put_v(body, 6); // fields stated: pts delta, size multiplier, stream, size, reserved, count
    put_s(body, 0); // pts delta, unused since every frame codes its own
    put_v(body, size_mul);
    put_v(body, 0); // stream
    put_v(body, 0); // size lsb
    put_v(body, 0); // reserved fields
    put_v(body, count);
}

/// The info packet that states a stream's frame rate: one field named
/// `r_frame_rate`, whose value is `num/den` as a UTF-8 string. It is written
/// against the stream rather than the file, since the rate is the stream's.
fn frame_rate_info(stream_id: u64, num: u64, den: u64) -> Vec<u8> {
    let mut body = Vec::with_capacity(32);
    put_v(&mut body, stream_id + 1);
    put_s(&mut body, 0); // chapter id: the whole stream
    put_v(&mut body, 0); // chapter start
    put_v(&mut body, 0); // chapter length
    put_v(&mut body, 1); // one field
    put_vb(&mut body, b"r_frame_rate");
    put_s(&mut body, -1); // the value that follows is a UTF-8 string
    put_vb(&mut body, format!("{num}/{den}").as_bytes());
    body
}

/// The stream header: the codec tag, the geometry its class calls for, and
/// how PTS are coded.
fn stream_header(id: u64, time_base: u64, stream: &Stream) -> Vec<u8> {
    let mut body = Vec::with_capacity(32);
    put_v(&mut body, id);
    put_v(&mut body, stream.class());
    put_vb(&mut body, &stream.fourcc);
    put_v(&mut body, time_base);
    put_v(&mut body, u64::from(stream.msb_pts_shift));
    put_v(&mut body, stream.max_pts_distance);
    put_v(&mut body, stream.decode_delay);
    put_v(&mut body, 0); // stream flags
    put_vb(&mut body, &stream.extradata); // empty for every raw stream
    match stream.media {
        Media::Video {
            width,
            height,
            sample_width,
            sample_height,
            colorspace_type,
        } => {
            put_v(&mut body, u64::from(width));
            put_v(&mut body, u64::from(height));
            put_v(&mut body, sample_width);
            put_v(&mut body, sample_height);
            put_v(&mut body, colorspace_type);
        }
        Media::Audio {
            sample_rate,
            channels,
        } => {
            put_v(&mut body, u64::from(sample_rate)); // sample rate numerator
            put_v(&mut body, 1); // sample rate denominator
            put_v(&mut body, u64::from(channels));
        }
        // Subtitles and data carry no geometry at all: the header ends here.
        Media::Other { .. } => {}
    }
    body
}

/// The annotation stream's header: the same time base and PTS coding as the
/// first media stream, so a row packet's timestamp compares directly with a
/// frame's, and a data class, which carries no geometry. On a wire of one
/// media stream its id is [`crate::ANNOTATION_STREAM_ID`].
#[cfg(feature = "annotations")]
fn annotation_stream_header(id: u64, time_base: u64, stream: &Stream) -> Vec<u8> {
    let mut body = Vec::with_capacity(24);
    put_v(&mut body, id);
    put_v(&mut body, ANNOTATION_CLASS);
    put_vb(&mut body, ANNOTATION_FOURCC);
    put_v(&mut body, time_base);
    put_v(&mut body, u64::from(stream.msb_pts_shift));
    put_v(&mut body, stream.max_pts_distance);
    put_v(&mut body, 0); // decode delay
    put_v(&mut body, 0); // stream flags
    put_vb(&mut body, &[]); // no codec specific data
    body
}

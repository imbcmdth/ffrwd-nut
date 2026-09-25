//! Reading NUT off something that blocks: a pipe, a socket, a file.
//!
//! This is [`PushDemuxer`] with a loop around it. The parsing is the same
//! parsing; what is here is the reading, the one media stream this wire
//! carries, and the two things that only a reader which can wait knows how to
//! do - the ADTS header an aac stream hides its config in, and the annotation
//! stream beside the frames.

#[cfg(feature = "annotations")]
use std::collections::HashMap;
use std::io::{ErrorKind, Read};

use crate::demux::{Event, Limits, PushDemuxer};
use crate::error::{bail, Error, Result};
use crate::{adts, Media, Packet, Stream, AUDIO_CLASS, DATA_CLASS, JSON_FOURCC, VIDEO_CLASS};
#[cfg(feature = "annotations")]
use crate::{ANNOTATION_CLASS, ANNOTATION_FOURCC, ANNOTATION_STREAM_ID, TRAILING_KEY};

/// How much is read off the reader at a time, for everything but a frame's
/// own payload, which is read straight into the frame.
const SCRATCH: usize = 64 * 1024;

/// Largest NDJSON payload one annotation packet may carry.
#[cfg(feature = "annotations")]
const MAX_ANNOTATION_BYTES: u64 = 1 << 20;

/// How many frames' rows may sit unclaimed before the annotation stream is
/// treated as unmatched to the video rather than merely ahead of it. Rows are
/// written just before their frame, so one is the working depth.
#[cfg(feature = "annotations")]
const MAX_UNCLAIMED_ROWS: usize = 1024;

/// Reads one NUT stream off a reader: one media stream, and the annotation
/// stream where a caller asked for it.
pub struct Demuxer<R> {
    inner: R,
    core: PushDemuxer,
    scratch: Vec<u8>,
    stream: Stream,
    /// Whether the input declared the annotation stream. Nothing reads it
    /// without the feature that reads that stream.
    #[cfg_attr(not(feature = "annotations"), allow(dead_code))]
    annotations: bool,
    /// The first packet, held while `detect_adts` decided whether this
    /// stream needs ADTS stripped, and handed back by the next call to
    /// `read_packet`.
    first_packet: Option<(Packet, Vec<u8>)>,
    /// Whether every packet (the buffered first one included) has its ADTS
    /// header stripped before it reaches a caller. Set only for an aac
    /// stream that opened with no extradata of its own, whose first packet
    /// turned out to carry one.
    strip_adts: bool,
    /// Rows read off the annotation stream, keyed by the PTS of the frame
    /// they belong to, until that frame arrives.
    #[cfg(feature = "annotations")]
    unclaimed_rows: HashMap<i64, Vec<String>>,
    /// Rows the trailing record carried, which belong to no frame.
    #[cfg(feature = "annotations")]
    trailing: Vec<String>,
}

impl<R: Read> Demuxer<R> {
    /// Reads up to and including the stream header, so geometry is known
    /// before the first frame arrives. One media stream and nothing else.
    pub fn open(inner: R) -> Result<Demuxer<R>> {
        Demuxer::read_headers(inner, false, Limits::default())
    }

    /// `open`, with ceilings of a caller's own on what the stream may ask
    /// for.
    pub fn open_with(inner: R, limits: Limits) -> Result<Demuxer<R>> {
        Demuxer::read_headers(inner, false, limits)
    }

    /// `open`, accepting the optional annotation stream beside the media.
    /// An input carrying only the media stream is still read.
    #[cfg(feature = "annotations")]
    pub fn open_annotated(inner: R) -> Result<Demuxer<R>> {
        Demuxer::read_headers(inner, true, Limits::default())
    }

    fn read_headers(inner: R, annotations: bool, limits: Limits) -> Result<Demuxer<R>> {
        let mut inner = inner;
        let mut core = PushDemuxer::new(limits);
        let mut scratch = vec![0u8; SCRATCH];
        #[cfg(feature = "annotations")]
        if annotations {
            // The annotation stream is rows of JSON, not pictures, and a
            // packet of them is small however large a frame may be.
            core.set_stream_limit(ANNOTATION_STREAM_ID as usize, MAX_ANNOTATION_BYTES);
        }

        let mut declared = 0usize;
        let mut headers = 0usize;
        let mut media = false;
        let mut annotation_stream = false;
        loop {
            match pump(&mut inner, &mut core, &mut scratch)? {
                Event::MainHeader(main) => {
                    if declared != 0 {
                        if headers >= declared {
                            bail!(
                                format: "NUT input restates its headers before its first frame; \
                                 this wire carries one stream with one set of headers"
                            );
                        }
                        bail!(format: "NUT input has a second main header");
                    }
                    let count = main.stream_count;
                    if annotations {
                        if count > 2 {
                            bail!(
                                unsupported: "NUT input carries {count} streams; this wire carries \
                                 a video stream and an annotation stream"
                            );
                        }
                    } else if count != 1 {
                        bail!(unsupported: "NUT input carries {count} streams; this wire carries one");
                    }
                    declared = count;
                }
                Event::StreamHeader { index, stream } => {
                    if headers >= declared {
                        bail!(
                            format: "NUT input restates its headers before its first frame; this \
                             wire carries one stream with one set of headers"
                        );
                    }
                    headers += 1;
                    if index == 0 {
                        if media {
                            bail!(
                                unsupported: "NUT input carries more than one media stream; this \
                                 wire carries one"
                            );
                        }
                        check_media(&stream)?;
                        media = true;
                    } else {
                        check_annotations(&stream)?;
                        annotation_stream = true;
                    }
                }
                Event::EndOfHeaders => break,
                Event::EndOfInput => {
                    if media {
                        break;
                    }
                    bail!(format: "NUT input ends before its stream header");
                }
                _ => {}
            }
        }

        let stream = core
            .stream(0)
            .cloned()
            .ok_or_else(|| Error::format("NUT input ends before its stream header".to_string()))?;
        let mut demuxer = Demuxer {
            inner,
            core,
            scratch,
            stream,
            annotations: annotation_stream,
            first_packet: None,
            strip_adts: false,
            #[cfg(feature = "annotations")]
            unclaimed_rows: HashMap::new(),
            #[cfg(feature = "annotations")]
            trailing: Vec::new(),
        };
        demuxer.detect_adts()?;
        Ok(demuxer)
    }

    /// Primes ADTS handling for a stream that opened with no extradata of
    /// its own: reads the first packet, and - only when it starts with the
    /// ADTS syncword - lifts an AudioSpecificConfig out of its header into
    /// the stream's extradata before anything downstream sees it, and marks
    /// every packet (this one included) to have its header stripped the way
    /// `aac_adtstoasc` would. A stream that already carries extradata, or
    /// whose first packet does not start with the syncword, is untouched.
    fn detect_adts(&mut self) -> Result<()> {
        if self.stream.codec_name() != Some("aac") || !self.stream.extradata.is_empty() {
            return Ok(());
        }
        let mut buf = Vec::new();
        let Some(packet) = self.read_packet(&mut buf)? else {
            return Ok(());
        };
        if adts::Header::parse(&buf).is_none() {
            self.first_packet = Some((packet, buf));
            return Ok(());
        }
        let header = adts::strip(&mut buf)?;
        self.stream.extradata = header.audio_specific_config().to_vec();
        self.strip_adts = true;
        self.first_packet = Some((packet, buf));
        Ok(())
    }

    /// What the stream header said. An output header is built from this, so
    /// geometry and time base survive the hop.
    pub fn stream(&self) -> &Stream {
        &self.stream
    }

    /// Whether the input actually carried an annotation stream. False for an
    /// input that declared only the media stream, even when annotations were
    /// asked for.
    #[cfg(feature = "annotations")]
    pub fn has_annotations(&self) -> bool {
        self.annotations
    }

    /// The rows that arrived for the frame at `pts`, taken out of the
    /// demuxer. Empty when that frame carried none.
    #[cfg(feature = "annotations")]
    pub fn take_rows(&mut self, pts: i64) -> Vec<String> {
        self.unclaimed_rows.remove(&pts).unwrap_or_default()
    }

    /// The rows the trailing record carried, taken out of the demuxer. The
    /// record comes after every frame, so this is empty until the stream has
    /// been read to its end.
    #[cfg(feature = "annotations")]
    pub fn take_trailing(&mut self) -> Vec<String> {
        std::mem::take(&mut self.trailing)
    }

    /// The next frame's PTS, with its bytes in `data`, or None at the end of
    /// the stream. Packets between frames are consumed on the way.
    pub fn read_frame(&mut self, data: &mut Vec<u8>) -> Result<Option<i64>> {
        Ok(self.read_packet(data)?.map(|packet| packet.pts))
    }

    /// `read_frame`, keeping everything the frame's own header said: the PTS,
    /// the DTS the reorder buffer settles, and the keyframe flag. For an
    /// encoded stream this is the read that loses nothing.
    ///
    /// An aac stream `detect_adts` primed has its ADTS header stripped here,
    /// the buffered first packet included, so a caller sees exactly what an
    /// `aac_adtstoasc`'d wire would have carried.
    pub fn read_packet(&mut self, data: &mut Vec<u8>) -> Result<Option<Packet>> {
        if let Some((packet, buffered)) = self.first_packet.take() {
            *data = buffered;
            return Ok(Some(packet));
        }
        let packet = self.read_packet_inner(data)?;
        if packet.is_some() && self.strip_adts {
            adts::strip(data)?;
        }
        Ok(packet)
    }

    /// The read `read_packet` wraps: past the headers, off the wire,
    /// unaware of ADTS.
    fn read_packet_inner(&mut self, data: &mut Vec<u8>) -> Result<Option<Packet>> {
        loop {
            match pump(&mut self.inner, &mut self.core, &mut self.scratch)? {
                Event::Frame { stream, packet } => {
                    #[cfg(feature = "annotations")]
                    if stream == ANNOTATION_STREAM_ID as usize {
                        self.read_annotation(packet.pts)?;
                        continue;
                    }
                    let _ = stream;
                    self.core.take_payload(data);
                    return Ok(Some(packet));
                }
                Event::EndOfInput => return Ok(None),
                // ffmpeg restates its headers every so often, which is a
                // second stream description this wire has no way to honour
                // once frames have been handed out against the first.
                Event::MainHeader(_) | Event::StreamHeader { .. } => bail!(
                    unsupported: "NUT input restates its headers mid-stream; this wire carries one \
                     stream with one set of headers"
                ),
                _ => {}
            }
        }
    }

    /// One annotation packet: the trailing record, or NDJSON with one row per
    /// line, held until the frame at `pts` asks for it.
    #[cfg(feature = "annotations")]
    fn read_annotation(&mut self, pts: i64) -> Result<()> {
        if self.unclaimed_rows.len() >= MAX_UNCLAIMED_ROWS {
            bail!(
                limit: "NUT annotation stream has {MAX_UNCLAIMED_ROWS} packets no frame claimed; \
                 its timestamps do not match the video's"
            );
        }
        let text = std::str::from_utf8(self.core.payload()).map_err(|_| {
            Error::format(format!("NUT annotation packet at PTS {pts} is not UTF-8"))
        })?;

        if let Some(rows) = trailing_rows(text) {
            if !self.trailing.is_empty() {
                bail!(format: "NUT annotation stream carries a second trailing record; there is one");
            }
            self.trailing = rows;
            return Ok(());
        }

        let rows: Vec<String> = text
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        self.unclaimed_rows.entry(pts).or_default().extend(rows);
        Ok(())
    }
}

/// The one media stream this wire carries has to be one whose geometry the
/// stream header describes, or a data stream of JSON messages, which needs
/// none.
fn check_media(stream: &Stream) -> Result<()> {
    match stream.media {
        Media::Video { .. } | Media::Audio { .. } => Ok(()),
        Media::Other { .. } if stream.is_json() => Ok(()),
        Media::Other { class } => Err(Error::unsupported(format!(
            "NUT input carries stream class {class} codec {}; this wire carries video (class \
             {VIDEO_CLASS}), audio (class {AUDIO_CLASS}) and JSON messages (class \
             {DATA_CLASS}, codec {})",
            stream.fourcc_name(),
            String::from_utf8_lossy(JSON_FOURCC)
        ))),
    }
}

/// Stream 1 is the annotation stream or it is nothing: a reader that asked
/// for rows and was handed a second media stream is being handed a wire it
/// cannot read.
#[cfg(feature = "annotations")]
fn check_annotations(stream: &Stream) -> Result<()> {
    let class = stream.class();
    if class != ANNOTATION_CLASS || stream.fourcc != ANNOTATION_FOURCC {
        return Err(Error::unsupported(format!(
            "NUT stream 1 carries class {class} codec {}; the annotation stream is class \
             {ANNOTATION_CLASS} codec {}",
            stream.fourcc_name(),
            String::from_utf8_lossy(ANNOTATION_FOURCC)
        )));
    }
    Ok(())
}

#[cfg(not(feature = "annotations"))]
fn check_annotations(_stream: &Stream) -> Result<()> {
    Err(Error::unsupported(
        "NUT input carries a second stream; this wire carries one".to_string(),
    ))
}

/// The record trailing rows ride in. Its one key is what tells it from the
/// NDJSON a frame's rows are written as.
#[cfg(feature = "annotations")]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TrailingRecord<'a> {
    #[serde(borrow, rename = "trailing")]
    rows: Vec<&'a serde_json::value::RawValue>,
}

/// The rows a trailing record carries, verbatim, or None when `text` is a
/// frame's rows instead.
#[cfg(feature = "annotations")]
fn trailing_rows(text: &str) -> Option<Vec<String>> {
    debug_assert_eq!(TRAILING_KEY, "trailing", "the record's one key is renamed");
    let record: TrailingRecord = serde_json::from_str(text).ok()?;
    Some(
        record
            .rows
            .into_iter()
            .map(|row| row.get().to_string())
            .collect(),
    )
}

/// The next event, reading as much as it takes to reach one. A frame's
/// payload goes straight into the frame rather than through `scratch`, so a
/// large frame is not copied twice.
fn pump<R: Read>(inner: &mut R, core: &mut PushDemuxer, scratch: &mut [u8]) -> Result<Event> {
    loop {
        if let Some(event) = core.next_event()? {
            return Ok(event);
        }
        let read = match core.payload_space() {
            Some(space) if !space.is_empty() => {
                let read = read_some(inner, space)?;
                core.payload_filled(read);
                read
            }
            _ => {
                let read = read_some(inner, scratch)?;
                core.feed(&scratch[..read]);
                read
            }
        };
        if read == 0 {
            core.finish();
        }
    }
}

/// Whatever the reader has, or zero at the end of it.
fn read_some<R: Read>(inner: &mut R, into: &mut [u8]) -> Result<usize> {
    loop {
        match inner.read(into) {
            Ok(read) => return Ok(read),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

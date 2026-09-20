//! Reading NUT with no I/O under it: bytes go in as they arrive, events come
//! out as they complete.
//!
//! This is the whole demuxer. [`Demuxer`](crate::Demuxer), the blocking
//! reader, is a loop around it that fills it from a `Read`, so a stream reads
//! the same whichever end a consumer holds.
//!
//! Nothing here is allocated from a length the wire gave until that length
//! has been checked against [`Limits`], nothing that a wire value goes into
//! is allowed to wrap, and a packet this crate does not read is stepped over
//! by its own length rather than held.

use crate::bytes::{crc32, Cursor, Halt, Parse};
use crate::error::{bail, Error, Result};
use crate::{
    flags, Media, Packet, Stream, TimeBase, AUDIO_CLASS, FILE_ID, INFO_STARTCODE, MAIN_STARTCODE,
    STREAM_STARTCODE, SYNCPOINT_STARTCODE, VERSION, VIDEO_CLASS,
};

const FILE_ID_LEN: usize = FILE_ID.len();

/// A packet body wider than this carries its own header checksum ahead of the
/// data.
const HEADER_CHECKSUM_THRESHOLD: u64 = 4096;

/// The most elision headers a main header may declare, and the widest one.
/// ffmpeg's own bounds.
const MAX_ELISION_HEADERS: usize = 128;
const MAX_ELISION_BYTES: usize = 256;

/// The most time bases a main header may declare.
const MAX_TIME_BASES: u64 = 64;

/// The most reserved fields one frame header may carry.
const MAX_RESERVED_FIELDS: u64 = 256;

/// The deepest frame reordering this crate carries. ffmpeg's own bound.
const MAX_DECODE_DELAY: u64 = 16;

/// How much the demuxer will hold or hand over, for a stream whose lengths
/// something else chose. Every one of these is a ceiling on an allocation the
/// wire asks for, so a hostile length is refused by name instead of tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The largest frame, in bytes. The default is what uncompressed 8K RGBA
    /// fits inside ten times over; a consumer that knows its geometry should
    /// say so, and [`PushDemuxer::set_stream_limit`] narrows it per stream.
    pub max_frame: u64,
    /// The largest packet body held whole to parse: the main header, a stream
    /// header, a syncpoint, an info packet. These are tens of bytes in
    /// practice. An info packet past this is stepped over rather than read,
    /// since an info packet is advisory; a header past it is refused.
    pub max_header: u64,
    /// The most unparsed bytes that may sit waiting for one packet to
    /// complete. A frame's payload does not count: it is written straight
    /// into the frame as it arrives.
    pub max_buffered: usize,
    /// The most streams a main header may declare.
    pub max_streams: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_frame: 1 << 31,
            max_header: 1 << 20,
            max_buffered: 1 << 21,
            max_streams: 16,
        }
    }
}

/// What the main header settles for the whole stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainHeader {
    /// The NUT version. Always [`VERSION`]: another is refused.
    pub version: u64,
    pub stream_count: usize,
    /// How far apart the writer promised to put syncpoints, in bytes.
    pub max_distance: u64,
    pub time_bases: Vec<TimeBase>,
}

/// What an info packet said that this crate understands. Everything else in
/// one is read past: an info packet is advisory, and a writer may put
/// anything in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    /// The stream it was written against, or None for the whole file.
    pub stream: Option<usize>,
    /// `r_frame_rate` as `num/den`. NUT has no per-frame duration field, so
    /// this is the only thing on the wire a duration can be worked out of.
    pub frame_rate: Option<(u64, u64)>,
}

/// What the parser found. The order over one stream is: the main header, a
/// stream header for each stream, the info packets and syncpoints that
/// follow, [`Event::EndOfHeaders`] once the last of those is behind it, then
/// a frame for every frame, then [`Event::EndOfInput`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// The main header, which settles the framecode table every frame header
    /// is read through.
    MainHeader(MainHeader),
    /// One stream's header. `stream` is what it said at that moment; an info
    /// packet after it may still fill in the frame rate, so a consumer that
    /// wants the settled description reads [`PushDemuxer::stream`] at
    /// [`Event::EndOfHeaders`].
    StreamHeader { index: usize, stream: Stream },
    /// An info packet, already applied to whichever stream it named.
    Info(Info),
    /// A syncpoint, which restates where the stream has got to. Every
    /// stream's clock is set from it.
    Syncpoint,
    /// Every declared stream has its header and the header section is over:
    /// what follows is frames. It comes once, unless a restated main header
    /// declares a different set of streams, which starts the section again.
    EndOfHeaders,
    /// One frame, its payload in [`PushDemuxer::payload`] until the next call
    /// to [`PushDemuxer::next_event`].
    Frame { stream: usize, packet: Packet },
    /// The input ended at a packet boundary. Every call after this answers
    /// with it again.
    EndOfInput,
}

/// One entry of the framecode table: what a single frame-header byte says
/// about the frame that follows it.
#[derive(Debug, Clone, Copy, Default)]
struct FrameCode {
    flags: u64,
    stream_id: u64,
    size_mul: u64,
    size_lsb: u64,
    pts_delta: i64,
    reserved_count: u64,
    header_idx: u64,
}

/// What the main header left behind for reading frames.
struct Tables {
    time_bases: Vec<TimeBase>,
    frame_codes: Box<[FrameCode; 256]>,
    /// The elision headers, entry 0 the empty one no frame names. A frame
    /// whose `header_idx` names another starts with that entry's bytes, and
    /// the wire carries only the rest.
    elision: Vec<Vec<u8>>,
}

/// Where one stream has got to.
struct Clock {
    /// The last PTS seen, which a frame coding only the low bits of its own
    /// is measured from.
    last_pts: i64,
    /// The reorder buffer DTS falls out of: the `decode_delay + 1` most
    /// recent PTS, ascending, seeded with `None`. The smallest entry after a
    /// frame's PTS lands is that frame's DTS - `None` while a seed remains,
    /// which is the wire not settling it.
    pts_buffer: Vec<Option<i64>>,
}

/// What the parser is in the middle of.
#[derive(Debug, Clone, Copy)]
enum State {
    /// The 25 bytes every NUT stream opens with.
    Identifier,
    /// At a packet boundary.
    Unit,
    /// Stepping over a body this crate does not read.
    Skip { remaining: u64, what: &'static str },
    /// Filling a frame's payload.
    Payload {
        stream: usize,
        packet: Packet,
        /// How much of the frame is on the wire, past any elided prefix.
        want: usize,
        filled: usize,
    },
    /// The input ended at a packet boundary.
    Ended,
}

/// A NUT demuxer that is handed bytes rather than reading them.
///
/// Feed it whatever arrives, in pieces of any size, and take events until it
/// asks for more. It never blocks, never reads, never seeks and never panics
/// on any input.
pub struct PushDemuxer {
    limits: Limits,
    /// Unparsed bytes, `at` of which are behind the parser.
    buf: Vec<u8>,
    at: usize,
    /// Where `buf[at]` sits in the stream, for a message that names a place.
    pos: u64,
    state: State,
    eof: bool,
    /// An event to hand over before the parser runs again.
    pending: Option<Event>,
    main: Option<Tables>,
    stream_count: usize,
    streams: Vec<Option<Stream>>,
    clocks: Vec<Clock>,
    /// The per-stream frame ceilings a caller set, which may arrive before
    /// the streams do.
    ceilings: Vec<Option<u64>>,
    headers_done: bool,
    /// The frame being filled, and the last frame handed over.
    payload: Vec<u8>,
}

impl PushDemuxer {
    /// A demuxer that has been handed nothing yet.
    pub fn new(limits: Limits) -> PushDemuxer {
        PushDemuxer {
            limits,
            buf: Vec::new(),
            at: 0,
            pos: 0,
            state: State::Identifier,
            eof: false,
            pending: None,
            main: None,
            stream_count: 0,
            streams: Vec::new(),
            clocks: Vec::new(),
            ceilings: Vec::new(),
            headers_done: false,
            payload: Vec::new(),
        }
    }

    /// The limits this was built with.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Narrows [`Limits::max_frame`] for one stream, for a consumer that
    /// knows what that stream carries: a frame past the ceiling is refused
    /// before anything is allocated for it. It may be set before the stream's
    /// header arrives.
    pub fn set_stream_limit(&mut self, stream: usize, max_frame: u64) {
        if self.ceilings.len() <= stream {
            self.ceilings.resize(stream + 1, None);
        }
        self.ceilings[stream] = Some(max_frame);
    }

    /// Bytes off the wire, in pieces of any size. They are parsed by
    /// [`PushDemuxer::next_event`], not here.
    pub fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.compact();
        self.buf.extend_from_slice(bytes);
    }

    /// There are no more bytes coming. After this, the parser either ends
    /// cleanly with [`Event::EndOfInput`] or says what it was in the middle
    /// of.
    pub fn finish(&mut self) {
        self.eof = true;
    }

    /// How many bytes are waiting to be parsed. A frame's payload is not
    /// among them: it is written into the frame as it arrives.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.at
    }

    /// How far into the stream the parser has read.
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// The streams whose headers have arrived, in their own order. An entry
    /// is None until that stream's header does.
    pub fn streams(&self) -> &[Option<Stream>] {
        &self.streams
    }

    /// One stream's header, once it has arrived.
    pub fn stream(&self, index: usize) -> Option<&Stream> {
        self.streams.get(index).and_then(Option::as_ref)
    }

    /// The payload of the frame [`Event::Frame`] just announced. It stands
    /// until the next call to [`PushDemuxer::next_event`].
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// The same payload, swapped out into `into` rather than copied. What was
    /// in `into` becomes the buffer the next frame is read into, so a
    /// consumer that hands the same vector back pays for one allocation
    /// rather than one a frame.
    pub fn take_payload(&mut self, into: &mut Vec<u8>) {
        std::mem::swap(&mut self.payload, into);
        self.payload.clear();
    }

    /// Where the frame being read still has room, for a reader that fills it
    /// directly instead of copying through [`PushDemuxer::feed`]. None unless
    /// the parser is waiting on a frame's payload with nothing else buffered.
    /// Whatever is written there is announced with
    /// [`PushDemuxer::payload_filled`].
    pub fn payload_space(&mut self) -> Option<&mut [u8]> {
        match self.state {
            State::Payload { want, filled, .. } if self.at == self.buf.len() => {
                let from = self.payload.len() - want + filled;
                Some(&mut self.payload[from..])
            }
            _ => None,
        }
    }

    /// How much of [`PushDemuxer::payload_space`] was filled.
    pub fn payload_filled(&mut self, count: usize) {
        if let State::Payload {
            want,
            ref mut filled,
            ..
        } = self.state
        {
            let count = count.min(want - *filled);
            *filled += count;
            self.pos += count as u64;
        }
    }

    /// The next event, or None where what has arrived does not make one yet.
    ///
    /// An error means the stream is not what it claims to be. There is no
    /// resynchronizing: a caller closes the connection or drops the file.
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        if let Some(event) = self.pending.take() {
            return Ok(Some(event));
        }
        loop {
            match self.state {
                State::Ended => return Ok(Some(Event::EndOfInput)),

                State::Skip { remaining, what } => {
                    let took = self.drop_bytes(remaining);
                    if took < remaining {
                        let short = remaining - took;
                        self.state = State::Skip {
                            remaining: short,
                            what,
                        };
                        return self.more(|| {
                            Error::format(format!(
                                "NUT stream ends inside {what}: {short} bytes short"
                            ))
                        });
                    }
                    self.state = State::Unit;
                }

                State::Payload {
                    stream,
                    packet,
                    want,
                    filled,
                } => {
                    let from = self.payload.len() - want + filled;
                    let take = (want - filled).min(self.buffered());
                    self.payload[from..from + take]
                        .copy_from_slice(&self.buf[self.at..self.at + take]);
                    self.advance(take);
                    let filled = filled + take;
                    if filled < want {
                        self.state = State::Payload {
                            stream,
                            packet,
                            want,
                            filled,
                        };
                        return self.more(|| {
                            Error::format(format!(
                                "NUT stream ends inside a frame: got {filled} of {want} bytes"
                            ))
                        });
                    }
                    self.state = State::Unit;
                    return Ok(Some(Event::Frame { stream, packet }));
                }

                State::Identifier => {
                    if self.buffered() < FILE_ID_LEN {
                        let got = self.buffered();
                        return self.more(|| {
                            Error::format(format!(
                                "NUT stream ends inside the NUT identifier: got {got} of \
                                 {FILE_ID_LEN} bytes"
                            ))
                        });
                    }
                    if &self.buf[self.at..self.at + FILE_ID_LEN] != FILE_ID {
                        bail!(format: "input is not NUT: it does not begin with the NUT identifier");
                    }
                    self.advance(FILE_ID_LEN);
                    self.state = State::Unit;
                }

                State::Unit => {
                    if self.buffered() == 0 {
                        if self.eof {
                            self.state = State::Ended;
                            if let Some(event) = self.end_of_headers() {
                                return Ok(Some(event));
                            }
                            return Ok(Some(Event::EndOfInput));
                        }
                        return Ok(None);
                    }
                    // The header section ends where the frames start, which
                    // is the first byte that opens no packet.
                    if self.buf[self.at] != b'N' {
                        if let Some(event) = self.end_of_headers() {
                            return Ok(Some(event));
                        }
                    }
                    match self.unit() {
                        Ok(Some(event)) => return Ok(Some(event)),
                        Ok(None) => {}
                        Err(Halt::Bad(error)) => return Err(error),
                        Err(Halt::Need(need)) => return self.more(|| need.ended()),
                    }
                }
            }
        }
    }

    /// One packet or one frame, off the front of the buffer.
    fn unit(&mut self) -> Parse<Option<Event>> {
        let first = self.buf[self.at];
        if first == b'N' {
            self.packet()
        } else {
            self.frame(first)
        }
    }

    /// One packet, introduced by a startcode.
    fn packet(&mut self) -> Parse<Option<Event>> {
        let mut cursor = Cursor::new(&self.buf[self.at..], self.pos);
        cursor.u8()?;
        let rest = cursor.take(7, "a startcode")?;
        let mut startcode = u64::from(b'N');
        for byte in rest {
            startcode = (startcode << 8) | u64::from(*byte);
        }

        let what = match startcode {
            MAIN_STARTCODE => "the main header",
            STREAM_STARTCODE => "a stream header",
            SYNCPOINT_STARTCODE => "a syncpoint",
            INFO_STARTCODE => "an info packet",
            _ => "a packet",
        };
        let body_len = cursor.v()?;
        // A packet this wide carries a checksum over its own header. Only
        // info and index packets ever reach that size, and those are read
        // past whole, so the field is stepped over rather than checked.
        if body_len > HEADER_CHECKSUM_THRESHOLD {
            cursor.u32()?;
        }
        if body_len < 4 {
            bail!(format: "NUT packet is {body_len} bytes, too short to hold its checksum ({what})");
        }

        let read = match startcode {
            MAIN_STARTCODE | STREAM_STARTCODE | SYNCPOINT_STARTCODE => {
                if body_len > self.limits.max_header {
                    bail!(
                        limit: "NUT packet claims {body_len} bytes, more than {} ({what})",
                        self.limits.max_header
                    );
                }
                true
            }
            // An info packet is advisory. One too wide to hold is stepped
            // over rather than refused, the way an index packet is.
            INFO_STARTCODE => body_len <= self.limits.max_header,
            _ => false,
        };

        if !read {
            let consumed = cursor.at();
            self.advance(consumed);
            self.state = State::Skip {
                remaining: body_len,
                what,
            };
            return Ok(None);
        }

        // Copied out, so that reading it can write to the parser's own state.
        // These are headers: a few hundred bytes, once a stream.
        let body = cursor.take(body_len as usize, what)?.to_vec();
        let consumed = cursor.at();
        let split = body.len() - 4;
        let want = u32::from_be_bytes([
            body[split],
            body[split + 1],
            body[split + 2],
            body[split + 3],
        ]);
        let body = &body[..split];
        let got = crc32(body);
        if got != want {
            bail!(
                format: "NUT packet checksum is {got:#010x}, not the {want:#010x} it carries ({what})"
            );
        }

        let event = match startcode {
            MAIN_STARTCODE => Some(Event::MainHeader(whole(self.read_main(body))?)),
            STREAM_STARTCODE => {
                let (index, stream) = whole(self.read_stream(body))?;
                Some(Event::StreamHeader { index, stream })
            }
            SYNCPOINT_STARTCODE => {
                whole(self.read_syncpoint(body))?;
                if let Some(end) = self.end_of_headers() {
                    self.pending = Some(end);
                }
                Some(Event::Syncpoint)
            }
            _ => read_info(body).map(Event::Info),
        };
        if let Some(Event::Info(info)) = &event {
            self.apply_info(*info);
        }
        self.advance(consumed);
        Ok(event)
    }

    /// [`Event::EndOfHeaders`], the once, where every declared stream has its
    /// header and the header section is behind the parser.
    fn end_of_headers(&mut self) -> Option<Event> {
        if self.headers_done || !self.headers_complete() {
            return None;
        }
        self.headers_done = true;
        Some(Event::EndOfHeaders)
    }

    /// Whether the main header and every stream header it declared are read.
    fn headers_complete(&self) -> bool {
        self.main.is_some() && self.streams.iter().all(Option::is_some)
    }

    fn read_main(&mut self, body: &[u8]) -> Parse<MainHeader> {
        let mut r = Cursor::new(body, 0);
        let version = r.v()?;
        if version != VERSION {
            bail!(unsupported: "NUT input is version {version}; this wire speaks version {VERSION}");
        }
        let stream_count = r.v()?;
        if stream_count == 0 || stream_count > self.limits.max_streams as u64 {
            bail!(limit: "NUT main header declares {stream_count} streams");
        }
        let stream_count = stream_count as usize;
        let max_distance = r.v()?;

        let time_base_count = r.v()?;
        if time_base_count == 0 || time_base_count > MAX_TIME_BASES {
            bail!(limit: "NUT main header declares {time_base_count} time bases");
        }
        let mut time_bases = Vec::with_capacity(time_base_count as usize);
        for _ in 0..time_base_count {
            let num = r.v()?;
            let den = r.v()?;
            if num == 0 || den == 0 {
                bail!(format: "NUT time base {num}/{den} is not a rate");
            }
            time_bases.push(TimeBase { num, den });
        }

        let frame_codes = read_frame_codes(&mut r)?;

        // Entry 0 is the empty header no frame names; the ones declared here
        // follow it, so a frame's `header_idx` indexes this list directly.
        let header_count = r.v()?;
        if header_count as usize >= MAX_ELISION_HEADERS {
            bail!(limit: "NUT main header declares {header_count} elision headers");
        }
        let mut elision: Vec<Vec<u8>> = Vec::with_capacity(header_count as usize + 1);
        elision.push(Vec::new());
        for index in 0..header_count {
            let header = r.vb("an elision header")?;
            if header.is_empty() || header.len() > MAX_ELISION_BYTES {
                bail!(limit: "NUT elision header {} is {} bytes", index + 1, header.len());
            }
            elision.push(header);
        }

        // ffmpeg restates its headers every so often, so this may be the same
        // main header again. The tables are taken as read either way; a
        // consumer that refuses a restatement does it on the event.
        if self.stream_count != stream_count {
            self.streams = vec![None; stream_count];
            self.clocks = Vec::new();
            for _ in 0..stream_count {
                self.clocks.push(Clock {
                    last_pts: 0,
                    pts_buffer: vec![None],
                });
            }
            self.stream_count = stream_count;
            self.headers_done = false;
        }
        self.main = Some(Tables {
            time_bases: time_bases.clone(),
            frame_codes,
            elision,
        });
        Ok(MainHeader {
            version,
            stream_count,
            max_distance,
            time_bases,
        })
    }

    fn read_stream(&mut self, body: &[u8]) -> Parse<(usize, Stream)> {
        let Some(main) = self.main.as_ref() else {
            bail!(format: "NUT input has a stream header before its main header");
        };
        let mut r = Cursor::new(body, 0);
        let stream_id = r.v()?;
        if stream_id >= self.stream_count as u64 {
            bail!(
                format: "NUT stream header is for stream {stream_id}, which the main header does \
                 not declare"
            );
        }
        let index = stream_id as usize;
        let stream_class = r.v()?;
        let fourcc = r.vb("a codec tag")?;
        let time_base_id = r.v()?;
        let time_base = *main
            .time_bases
            .get(usize::try_from(time_base_id).unwrap_or(usize::MAX))
            .ok_or_else(|| {
                Error::format(format!(
                    "NUT stream header names time base {time_base_id}, which the main header does \
                     not declare"
                ))
            })?;
        let msb_pts_shift = r.v()?;
        if msb_pts_shift >= 63 {
            bail!(format: "NUT stream header shifts PTS by {msb_pts_shift} bits");
        }
        let max_pts_distance = r.v()?;
        let decode_delay = r.v()?;
        if decode_delay > MAX_DECODE_DELAY {
            bail!(
                limit: "NUT stream reorders frames by {decode_delay}, deeper than the \
                 {MAX_DECODE_DELAY} this wire carries"
            );
        }
        let _stream_flags = r.v()?;
        let extradata = r.vb("codec specific data")?;

        let media = match stream_class {
            AUDIO_CLASS => read_audio_geometry(&mut r)?,
            VIDEO_CLASS => read_video_geometry(&mut r)?,
            // Subtitles and data carry no geometry, and nothing past the
            // codec tag says anything about their frames.
            class => Media::Other { class },
        };

        let stream = Stream {
            fourcc,
            time_base,
            msb_pts_shift: msb_pts_shift as u32,
            max_pts_distance,
            decode_delay,
            extradata,
            // What an info packet states, which is read after this header.
            frame_rate: None,
            media,
        };
        if decode_delay != 0 && stream.codec_name().is_none() {
            bail!(
                unsupported: "NUT stream of {} declares decode delay {decode_delay}; only an \
                 encoded stream reorders frames",
                stream.fourcc_name()
            );
        }

        // A restated header keeps the clock; a header that changes how deep
        // the stream reorders gets a reorder buffer the new size.
        let buffered = decode_delay as usize + 1;
        if self.clocks[index].pts_buffer.len() != buffered {
            self.clocks[index].pts_buffer = vec![None; buffered];
        }
        self.streams[index] = Some(stream.clone());
        Ok((index, stream))
    }

    /// A syncpoint restates where the stream is, which is what a frame coding
    /// only the low bits of its PTS is measured from. Every stream's clock is
    /// set, each in its own time base.
    fn read_syncpoint(&mut self, body: &[u8]) -> Parse<()> {
        let Some(main) = self.main.as_ref() else {
            bail!(format: "NUT input has a syncpoint before its main header");
        };
        let mut r = Cursor::new(body, 0);
        let coded = r.v()?;
        let _back_ptr = r.v()?;
        // The field is `ticks * count + index` with ticks SIGNED: a writer
        // casts to unsigned before the multiply, so a stream that opens
        // before zero (a dts ahead of reordered pictures, under
        // `-avoid_negative_ts disabled`) puts a two's complement pattern on
        // the wire. Read back as signed, a floored divide and a modulus that
        // is never negative take it apart again for any number of time
        // bases, which is more than an unsigned divide can say.
        let coded = coded as i64;
        let count = main.time_bases.len() as i64;
        let from = main.time_bases[coded.rem_euclid(count) as usize];
        let ticks = i128::from(coded.div_euclid(count));
        for (index, stream) in self.streams.iter().enumerate() {
            let Some(stream) = stream else { continue };
            let to = stream.time_base;
            let scaled = ticks * i128::from(from.num) * i128::from(to.den)
                / (i128::from(from.den) * i128::from(to.num));
            let pts = i64::try_from(scaled).map_err(|_| {
                Error::format("NUT syncpoint timestamp overflows 64 bits".to_string())
            })?;
            self.clocks[index].last_pts = pts;
        }
        Ok(())
    }

    /// The frame rate an info packet states, onto the stream it named.
    fn apply_info(&mut self, info: Info) {
        let Some(rate) = info.frame_rate else { return };
        match info.stream {
            Some(index) => {
                if let Some(Some(stream)) = self.streams.get_mut(index) {
                    stream.frame_rate = Some(rate);
                }
            }
            None => {
                for stream in self.streams.iter_mut().flatten() {
                    stream.frame_rate = Some(rate);
                }
            }
        }
    }

    /// One frame, whose header byte is `first`: its header here, its payload
    /// as it arrives.
    fn frame(&mut self, first: u8) -> Parse<Option<Event>> {
        if !self.headers_complete() {
            bail!(format: "NUT input has a frame before its stream header");
        }
        let tables = self.main.as_ref().expect("the headers are complete");
        let code = tables.frame_codes[usize::from(first)];
        let elision_count = tables.elision.len();
        if code.flags & flags::INVALID != 0 {
            bail!(format: "NUT frame header byte {first} is not a framecode the main header defines");
        }

        let mut cursor = Cursor::new(&self.buf[self.at..], self.pos);
        let header_from = cursor.at();
        cursor.u8()?;

        let mut frame_flags = code.flags;
        if frame_flags & flags::CODED != 0 {
            frame_flags ^= cursor.v()?;
        }

        let mut stream_id = code.stream_id;
        if frame_flags & flags::STREAM_ID != 0 {
            stream_id = cursor.v()?;
        }
        if stream_id >= self.stream_count as u64 {
            bail!(
                format: "NUT frame belongs to stream {stream_id}, which the main header does not \
                 declare"
            );
        }
        let slot = stream_id as usize;

        let pts = if frame_flags & flags::CODED_PTS != 0 {
            let coded = cursor.v()?;
            self.decode_pts(slot, coded)?
        } else {
            self.clocks[slot]
                .last_pts
                .checked_add(code.pts_delta)
                .ok_or_else(|| Error::format("NUT frame PTS overflows 64 bits".to_string()))?
        };

        let mut size = code.size_lsb;
        if frame_flags & flags::SIZE_MSB != 0 {
            let msb = cursor.v()?;
            size = msb
                .checked_mul(code.size_mul)
                .and_then(|scaled| scaled.checked_add(code.size_lsb))
                .ok_or_else(|| Error::format("NUT frame size overflows 64 bits".to_string()))?;
        }
        if frame_flags & flags::MATCH_TIME != 0 {
            cursor.s()?;
        }
        let mut header_idx = code.header_idx;
        if frame_flags & flags::HEADER_IDX != 0 {
            header_idx = cursor.v()?;
        }
        if header_idx as usize >= elision_count {
            bail!(
                format: "NUT frame elides its first bytes into header {header_idx}, which the main \
                 header does not declare"
            );
        }
        let mut reserved_count = code.reserved_count;
        if frame_flags & flags::RESERVED != 0 {
            reserved_count = cursor.v()?;
        }
        if reserved_count > MAX_RESERVED_FIELDS {
            bail!(limit: "NUT frame header declares {reserved_count} reserved fields");
        }
        for _ in 0..reserved_count {
            cursor.v()?;
        }
        if frame_flags & flags::SM_DATA != 0 {
            bail!(unsupported: "NUT frame carries side data, which NUT version {VERSION} does not define");
        }

        let header = cursor.since(header_from);
        if frame_flags & flags::CHECKSUM != 0 {
            let got = crc32(header);
            let want = cursor.u32()?;
            if got != want {
                bail!(
                    format: "NUT frame header checksum is {got:#010x}, not the {want:#010x} it carries"
                );
            }
        }

        // `size` counts the whole frame, elided prefix included; the wire
        // carries only what follows the prefix.
        let elided = self.main.as_ref().expect("read above").elision[header_idx as usize].len();
        if elided as u64 > size {
            bail!(
                format: "NUT frame of {size} bytes elides a {elided} byte header, which does not \
                 fit in it"
            );
        }
        let on_wire = size - elided as u64;
        let ceiling = self.ceiling(slot);
        if size > ceiling {
            bail!(limit: "NUT frame claims {size} bytes, more than {ceiling}");
        }

        // Past here the frame is read, so the clock moves with the cursor.
        let consumed = cursor.at();
        self.advance(consumed);
        self.clocks[slot].last_pts = pts;

        if frame_flags & flags::EOR != 0 {
            self.state = State::Skip {
                remaining: on_wire,
                what: "an end-of-relevance frame",
            };
            return Ok(None);
        }

        let packet = Packet {
            pts,
            dts: self.decode_dts(slot, pts),
            keyframe: frame_flags & flags::KEY != 0,
        };
        self.payload.clear();
        self.payload.extend_from_slice(
            &self.main.as_ref().expect("read above").elision[header_idx as usize],
        );
        self.payload.resize(size as usize, 0);
        self.state = State::Payload {
            stream: slot,
            packet,
            want: on_wire as usize,
            filled: 0,
        };
        Ok(None)
    }

    /// The largest frame this stream may carry.
    fn ceiling(&self, stream: usize) -> u64 {
        self.ceilings
            .get(stream)
            .copied()
            .flatten()
            .unwrap_or(self.limits.max_frame)
    }

    /// A coded PTS is either the whole value, offset, or only its low bits,
    /// which are lifted back onto the last PTS seen on that stream.
    fn decode_pts(&self, slot: usize, coded: u64) -> Parse<i64> {
        let shift_bits = self.streams[slot]
            .as_ref()
            .map_or(14, |stream| stream.msb_pts_shift);
        let shift = 1u64 << shift_bits;
        if coded >= shift {
            Ok(i64::try_from(coded - shift)
                .map_err(|_| Error::format("NUT frame PTS overflows 64 bits".to_string()))?)
        } else {
            let mask = (shift - 1) as i64;
            let delta = self.clocks[slot].last_pts - mask / 2;
            Ok(((coded as i64 - delta) & mask) + delta)
        }
    }

    /// One frame's DTS out of the reorder buffer: the new PTS lands over the
    /// smallest entry - the previous DTS, already spent - order is restored,
    /// and the smallest entry left is the answer. With no reordering the
    /// buffer holds one entry and the DTS is the PTS itself.
    fn decode_dts(&mut self, slot: usize, pts: i64) -> Option<i64> {
        let buffer = &mut self.clocks[slot].pts_buffer;
        buffer[0] = Some(pts);
        let mut i = 0;
        while i + 1 < buffer.len() && buffer[i] > buffer[i + 1] {
            buffer.swap(i, i + 1);
            i += 1;
        }
        buffer[0]
    }

    /// Steps over up to `count` buffered bytes, answering how many there
    /// were.
    fn drop_bytes(&mut self, count: u64) -> u64 {
        let take = count.min(self.buffered() as u64);
        self.advance(take as usize);
        take
    }

    fn advance(&mut self, count: usize) {
        self.at += count;
        self.pos += count as u64;
    }

    /// Drops what has been read, when there is enough of it to be worth
    /// moving the rest.
    fn compact(&mut self) {
        if self.at == 0 {
            return;
        }
        if self.at == self.buf.len() {
            self.buf.clear();
            self.at = 0;
            return;
        }
        if self.at >= 64 * 1024 || self.at * 2 >= self.buf.len() {
            self.buf.drain(..self.at);
            self.at = 0;
        }
    }

    /// More bytes are needed. Where none are coming, `ended` says what the
    /// stream stopped in the middle of.
    fn more(&mut self, ended: impl FnOnce() -> Error) -> Result<Option<Event>> {
        if self.eof {
            return Err(ended());
        }
        if self.buffered() > self.limits.max_buffered {
            bail!(
                limit: "NUT parser holds {} bytes of one unfinished packet, more than {}",
                self.buffered(),
                self.limits.max_buffered
            );
        }
        Ok(None)
    }
}

/// A packet body is whole by the time it is parsed, so a field that runs off
/// its end is a malformed packet rather than a slow wire.
fn whole<T>(parsed: Parse<T>) -> Result<T> {
    match parsed {
        Ok(value) => Ok(value),
        Err(Halt::Bad(error)) => Err(error),
        Err(Halt::Need(need)) => Err(need.ended()),
    }
}

/// The fields a video stream header ends with.
fn read_video_geometry(r: &mut Cursor<'_>) -> Parse<Media> {
    let width = r.v()?;
    let height = r.v()?;
    if width == 0 || height == 0 {
        bail!(format: "NUT stream header gives frame size {width}x{height}");
    }
    Ok(Media::Video {
        width: u32::try_from(width)
            .map_err(|_| Error::format(format!("NUT frame width {width} is too large")))?,
        height: u32::try_from(height)
            .map_err(|_| Error::format(format!("NUT frame height {height} is too large")))?,
        sample_width: r.v()?,
        sample_height: r.v()?,
        colorspace_type: r.v()?,
    })
}

/// The fields an audio stream header ends with. The rate is a ratio in NUT;
/// this wire carries whole rates, so a denominator that does not divide it is
/// refused rather than rounded.
fn read_audio_geometry(r: &mut Cursor<'_>) -> Parse<Media> {
    let num = r.v()?;
    let den = r.v()?;
    let channels = r.v()?;
    if den == 0 || num == 0 || num % den != 0 {
        bail!(
            unsupported: "NUT audio stream declares sample rate {num}/{den}; this wire carries \
             whole rates"
        );
    }
    if channels == 0 {
        bail!(format: "NUT audio stream declares {channels} channels");
    }
    Ok(Media::Audio {
        sample_rate: u32::try_from(num / den)
            .map_err(|_| Error::format(format!("NUT sample rate {num}/{den} is too large")))?,
        channels: u32::try_from(channels)
            .map_err(|_| Error::format(format!("NUT channel count {channels} is too large")))?,
    })
}

/// Walks the 256-entry framecode table. Each group in the header describes a
/// run of entries; index `N` is always invalid and never consumes one.
fn read_frame_codes(r: &mut Cursor<'_>) -> Parse<Box<[FrameCode; 256]>> {
    let mut codes = Box::new([FrameCode::default(); 256]);
    let mut index = 0usize;
    let mut groups = 0usize;

    let mut pts_delta = 0i64;
    let mut size_mul = 1u64;
    let mut stream_id = 0u64;
    let mut header_idx = 0u64;

    while index < 256 {
        groups += 1;
        if groups > 256 {
            bail!(limit: "NUT main header describes its framecode table in more than 256 groups");
        }

        let code_flags = r.v()?;
        let fields = r.v()?;
        if fields > 64 {
            bail!(limit: "NUT framecode group declares {fields} fields");
        }
        if fields > 0 {
            pts_delta = r.s()?;
        }
        if fields > 1 {
            size_mul = r.v()?;
        }
        if fields > 2 {
            stream_id = r.v()?;
        }
        let size_lsb = if fields > 3 { r.v()? } else { 0 };
        let reserved_count = if fields > 4 { r.v()? } else { 0 };
        let count = if fields > 5 {
            r.v()?
        } else {
            size_mul.saturating_sub(size_lsb)
        };
        if fields > 6 {
            r.s()?;
        }
        if fields > 7 {
            header_idx = r.v()?;
        }
        for _ in 8..fields {
            r.v()?;
        }

        let mut taken = 0u64;
        while taken < count && index < 256 {
            // Every startcode opens with this byte, so it can never be a
            // framecode. It takes a slot in the table and none of the group's
            // count.
            if index == usize::from(b'N') {
                codes[index].flags = flags::INVALID;
                index += 1;
                continue;
            }
            codes[index] = FrameCode {
                flags: code_flags,
                stream_id,
                size_mul,
                size_lsb: size_lsb.saturating_add(taken),
                pts_delta,
                reserved_count,
                header_idx,
            };
            index += 1;
            taken += 1;
        }
    }
    Ok(codes)
}

/// What an info packet says. It holds named fields, each a name and a value
/// whose type the leading signed integer picks; -1 is the UTF-8 string
/// `r_frame_rate` is written as, and every other type is read past. A field
/// this cannot parse is not an error: an info packet is advisory, and a
/// writer may put anything in one.
fn read_info(body: &[u8]) -> Option<Info> {
    let mut r = Cursor::new(body, 0);
    let stream_id_plus1 = r.v().ok()?;
    let _chapter_id = r.s().ok()?;
    let _chapter_start = r.v().ok()?;
    let _chapter_len = r.v().ok()?;
    let count = r.v().ok()?;
    let mut frame_rate = None;
    for _ in 0..count.min(64) {
        let name = r.vb("an info field name").ok()?;
        let kind = r.s().ok()?;
        let value = match kind {
            // A UTF-8 string, which is how ffmpeg writes this field.
            -1 => r.vb("an info field value").ok()?,
            // A named type and its value, both strings.
            -2 => {
                r.vb("an info field type").ok()?;
                r.vb("an info field value").ok()?
            }
            -3 => {
                r.s().ok()?;
                continue;
            }
            -4 => {
                r.v().ok()?;
                continue;
            }
            _ => continue,
        };
        if name == b"r_frame_rate" {
            frame_rate = read_rate(&value);
        }
    }
    Some(Info {
        stream: stream_id_plus1
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok()),
        frame_rate,
    })
}

/// `num/den` as an info packet spells a frame rate. Both have to be positive
/// for the rate to mean anything.
fn read_rate(value: &[u8]) -> Option<(u64, u64)> {
    let text = std::str::from_utf8(value).ok()?;
    let (num, den) = text.split_once('/')?;
    let num: u64 = num.trim().parse().ok()?;
    let den: u64 = den.trim().parse().ok()?;
    (num > 0 && den > 0).then_some((num, den))
}

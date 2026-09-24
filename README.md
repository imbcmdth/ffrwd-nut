# ffrwd-nut

The NUT container ffrwd puts on the pipes between its processes, as one
demuxer with two front ends and the muxer that writes what ffmpeg reads.
Raw frames cannot carry a timestamp, NUT can, ffmpeg reads and writes it
natively on a pipe, and it costs a few dozen bytes a frame. Plain Rust
with no dependencies, so a wasm module compiles it in and its tests run
on the host.

```rust
use ffrwd_nut::{Demuxer, Muxer};

let mut input = Demuxer::open(pipe)?;
let mut output = Muxer::new(out, input.stream())?;
let mut frame = Vec::new();
while let Some(pts) = input.read_frame(&mut frame)? {
    output.write_frame(pts, &frame)?;
}
```

## The two front ends, over one parser

`PushDemuxer` is the parser. Bytes go in as they arrive, in pieces of
any size, and events come out as they complete: the main header, a
stream header for each stream, the info packets this crate understands,
`EndOfHeaders` once the header section is behind it, then a frame for
every frame with its stream, pts, dts and keyframe flag. It never
reads, never blocks and never seeks, which is what a wasm module with
one non-blocking socket and no threads needs.

```rust
use ffrwd_nut::{Event, Limits, PushDemuxer};

let mut demuxer = PushDemuxer::new(Limits::default());
demuxer.feed(&whatever_the_socket_had);
while let Some(event) = demuxer.next_event()? {
    if let Event::Frame { stream, packet } = event {
        consume(stream, packet.pts, demuxer.payload());
    }
}
```

`Demuxer` is that same parser with a loop around it that fills it from
a `Read`, for a process reading a pipe. It carries the one media stream
an ffmpeg-facing wire puts on a pipe, and the two things a reader that
can wait knows how to do: deriving an aac stream's config from the ADTS
header its first packet carries, and reading the annotation stream.
There is no second parser behind it. A frame's payload is read straight
into the frame rather than through a buffer in the middle, so the two
front ends cost the same per byte.

The muxer writes the headers from a `Stream`, then `write_frame` for a
raw stream or `write_coded` for an encoded one. Encoded packets go in
decode order, because NUT carries no dts field at all and the reader on
the far side rebuilds dts from the pts values in the order they arrive,
through a reorder buffer of `decode_delay + 1` entries. Hand them over
in presentation order and the dts that come out the other end are
wrong, which is the one mistake this interface cannot catch for you.

## Why the frame rate is written down

NUT has no per-frame duration field. A reader can subtract one packet's
pts from the next and call the difference a duration, but only on a
stream that does not reorder: where there are B-frames the next packet
READ is not the next picture SHOWN, and no pair of timestamps settles
the gap. So the muxer writes `r_frame_rate` as an info packet ahead of
the first frame and the demuxer reads it back into `Stream::frame_rate`,
which is the only thing on this wire a duration can be worked out of.
It goes out with the headers rather than later because a reader that
learns the rate after the fact has already handed out packets without
it.

## Metadata

ffmpeg writes a stream's metadata (`-metadata:s:v name=value`) and the
file's (`-metadata name=value`) as info packets between the stream
headers and the first syncpoint. `PushDemuxer::tags(Some(index))` and
`PushDemuxer::tags(None)` hand back the string fields those packets
stated, as `(name, value)` pairs in the order the names first arrived,
and all of them are there by `EndOfHeaders`. A header section restated
mid-stream states them again, and a name stated again replaces its value
rather than adding a second one. Fields of any other type are read past,
and so is a new name once a stream already holds 64.

## What it does not do

Version 4 is refused by number rather than half read: it adds per-frame
side data and broadcast timestamps, neither of which belongs on a pipe
between two of our own processes.

There is no index and no seeking. An index packet is stepped over by
its own length without being held, because the wire this reads is a
pipe and nothing on it can seek. Startcodes this crate does not know go
the same way.

There is no resynchronizing. An error means the stream is not what it
claims to be, and the caller closes the connection or drops the file.
Nothing here hunts for the next syncpoint and carries on, because a
stream that lies about one frame has said nothing trustworthy about the
next.

Payloads are opaque. A video packet is a frame, encoded bytes or raw
pixels, an audio packet is however many samples the producer chose, and
nothing here looks inside either. The single exception is the ADTS
header, which is in `adts` and is there because NUT is where its loss
shows up: `-c copy` off an ADTS source leaves the stream header's
extradata empty, since ADTS states the same handful of fields on every
frame instead of once in the container.

Nothing is allocated from a length the wire gave until that length has
been checked against `Limits`, no arithmetic on a wire value is allowed
to wrap, and no input makes this panic. `Limits` bounds the largest
frame, the largest header held whole to parse, and how much may sit
buffered waiting for one packet to finish; `set_stream_limit` narrows
the frame ceiling per stream, for a consumer that knows what a stream
carries.

## Building and testing

```
cargo test
cargo test --all-features
cargo build --target wasm32-wasip2 --release
```

A consumer depends on this the way it depends on any of these crates,
by tag:

```toml
ffrwd-nut = { git = "https://github.com/imbcmdth/ffrwd-nut", tag = "v0.1.0" }
```

The default build has no dependencies at all. The `annotations` feature
adds the private stream a sidecar puts its rows on beside the frames,
and takes serde with it: telling one JSON record from the lines of
another means parsing JSON, which the format itself never needs.

`tests/push.rs` reads two wires ffmpeg wrote, whole, a byte at a time
and in arbitrary pieces, and pins that all three give the same events;
it then truncates one of them at every byte, flips bits, and feeds
thousands of random wires, none of which may panic. `tests/ffmpeg.rs`
hands real ffmpeg what this muxer wrote and reads back what ffmpeg
muxed, and skips with a message where ffmpeg is not on PATH. The two
fixtures were generated once with the commands recorded at the top of
`tests/push.rs`.

## License

Apache-2.0.

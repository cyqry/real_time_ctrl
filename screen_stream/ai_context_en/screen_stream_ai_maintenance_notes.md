# screen_stream AI Maintenance Notes

This document is written for future AI maintainers. It summarizes the crate quickly and points to the code paths that usually matter during debugging. It intentionally lives outside `screen_stream/docs` and outside the Chinese developer guide folder.

## Purpose

`screen_stream` is a Rust library crate for low-latency Windows desktop streaming:

1. Capture the primary display with DXGI Desktop Duplication.
2. Encode frames as H.264.
3. Send compact binary packets over any Tokio `AsyncWrite`.
4. Receive packets from any Tokio `AsyncRead`.
5. Decode H.264 with OpenH264 into RGBA frames.
6. Optionally render decoded frames in a native Win32 window.

The crate avoids Windows Graphics Capture on purpose, because DXGI Desktop Duplication does not show a picker, consent prompt, or capture border in the user session.

## Important Files

```text
src/lib.rs
  Public API and re-exports.

src/config.rs
  CaptureConfig, PlayerConfig, encoder backend selection, quality presets.

src/capture/dxgi.rs
  Main sender pipeline: DXGI capture, FPS limiting, bounded packet queue,
  H.264 encode, and async packet writing coordination.

src/codec/h264.rs
  Encoder facade plus OpenH264 software encoder and decoder.

src/codec/mf_h264.rs
  Windows Media Foundation hardware H.264 encoder. This file contains the
  D3D11 zero-copy path and most of the tricky state-machine logic.

src/codec/colorspace.rs
  BGRA to I420/NV12 conversion helpers.

src/wire/mod.rs
  RTSS packet body protocol.

src/stream.rs
  Outer length-prefixed stream framing and receive/decode loop.

src/player/native_window.rs
  Native Win32 playback window. The async decode task stores only the newest
  frame; the blocking window thread paints it with StretchDIBits.

examples/send_tcp.rs
examples/recv_tcp.rs
examples/recv_window_tcp.rs
  Minimal TCP sender, decode-only receiver, and native-window receiver.
```

## Sender Path

```text
send_tcp.rs
  -> TcpStream::connect()
  -> send_primary_screen()
  -> mpsc::channel(queue_capacity)
  -> spawn_blocking(run_capture_loop)
  -> Monitor::primary()
  -> DxgiDuplicationApi::new()
  -> H264Encoder::new()
  -> send Hello
  -> loop:
       acquire_next_frame()
       apply max_fps limiter
       try_reserve() bounded writer queue capacity
       encode using D3D11 MF path or CPU fallback
       enqueue WirePacket::Video
  -> async writer loop:
       rx.recv()
       write_packet_counted()
```

Latency design: the capture loop reserves queue capacity before encoding. If the writer side is already behind, the raw frame is skipped before expensive encoding starts. This is deliberate: the library prefers dropping stale frames over buffering latency.

## Receiver Path

```text
recv_tcp.rs
  -> TcpListener::bind()
  -> accept()
  -> play_from()
  -> receive_decoded()
  -> read_packet_counted()
  -> decode_packet()
  -> OpenH264 decoder
  -> callback(DecodedVideoFrame)
```

The receiver requires a Hello packet before Video packets. The current decoder is OpenH264 and outputs RGBA.

## Native Window Path

```text
recv_window_tcp.rs
  -> play_from_native_window()
  -> async decode task:
       play_from()
       convert RGBA to BGRA once
       store latest RenderFrame only
       coalesce WM_STREAM_FRAME posts
  -> blocking Win32 task:
       CreateWindowExW
       GetMessageW loop
       WM_STREAM_FRAME / WM_SIZE -> InvalidateRect
       WM_PAINT -> StretchDIBits
```

The window player intentionally stores only the newest decoded frame. Slow painting or resize events must not create playback latency. Repaint messages are coalesced so the decode task does not flood the Win32 queue.

## Wire Protocol

Outer framing in `stream.rs`:

```text
u32 big-endian body length
body bytes
```

Body protocol in `wire/mod.rs`:

```text
magic:   "RTSS"
version: u16 = 1
kind:    Hello | Video
```

Each Video payload is exactly one complete H.264 access unit. Preserve that invariant; it keeps the stream reusable for native decoding, WebCodecs, MSE, WebRTC RTP packetization, or relays without server-side decode/re-encode.

## Encoder Backends

```text
Auto
  Try Media Foundation hardware H.264 first, fall back to OpenH264.

MediaFoundation
  Force Windows hardware encoder. Fail if unavailable.

OpenH264
  Force software encoder.
```

The preferred MF path is:

```text
DXGI D3D11 texture
  -> D3D11 VideoProcessor BGRA to NV12
  -> MFCreateDXGISurfaceBuffer
  -> hardware H.264 MFT
  -> EncodedVideoFrame
```

The fallback MF path copies CPU BGRA, converts to NV12, and feeds an `IMFMediaBuffer` sample into the same hardware MFT.

## Critical Invariants And Pitfalls

1. `capture/dxgi.rs` must keep the writer queue bounded. Do not add unbounded buffering in the sender.
2. `stream.rs` must preserve one body packet per length prefix.
3. `wire/mod.rs` must preserve one H.264 access unit per Video payload.
4. `player/native_window.rs` must keep only the latest decoded frame; do not queue decoded frames for the renderer unless the product goal changes.
5. `mf_h264.rs` must not block indefinitely while pumping MFT events.
6. D3D11 zero-copy hardware encode must enable `ID3D11Multithread::SetMultithreadProtected(true)` before sharing the device with Media Foundation. Without this, some drivers can freeze when the DXGI capture thread, D3D11 VideoProcessor, and async hardware MFT share the same device/context.
7. The current MF keyframe request is best-effort. Explicit CODECAPI force-IDR support is a future improvement.

## Known Freeze Pattern

Observed symptom before the fix:

```text
recv_window_tcp + send_tcp quality --mf
receiver: [screen_stream receive] stops
receiver: [screen_stream render] continues painting duplicates
sender: [screen_stream send] and [screen_stream capture] stop
```

Interpretation:

1. The Win32 window thread is alive and painting the last frame.
2. The receive/decode pipeline no longer receives new packets.
3. TCP backpressure eventually stalls the sender.
4. The root cause was the D3D11/MF hardware encode path sharing a D3D11 device without multithread protection.

Fixes now in place:

1. Enable D3D11 multithread protection in `mf_h264.rs`.
2. Coalesce native-window repaint messages in `native_window.rs`.

## Useful Commands

Native-window receiver:

```powershell
cargo run -p screen_stream --release --example recv_window_tcp -- 127.0.0.1:7007 --stats
```

MF hardware sender:

```powershell
cargo run -p screen_stream --release --example send_tcp -- 127.0.0.1:7007 quality --mf --stats
```

Native-resolution quality test:

```powershell
cargo run -p screen_stream --release --example recv_window_tcp -- 127.0.0.1:7007 --stats
cargo run -p screen_stream --release --example send_tcp -- 127.0.0.1:7007 quality --native --mf --stats
```

Decode-only receiver:

```powershell
cargo run -p screen_stream --release --example recv_tcp -- 127.0.0.1:7007 --stats
```

Software sender comparison:

```powershell
cargo run -p screen_stream --release --example send_tcp -- 127.0.0.1:7007 quality --software --stats
```

Validation:

```powershell
cargo fmt -p screen_stream
cargo check -p screen_stream
cargo check -p screen_stream --examples
cargo test -p screen_stream
```

## Stats Cheat Sheet

```text
[screen_stream capture]
acquired       DXGI acquired frame rate
encoded        encoded output frame rate
encoder_skip   encoder-internal skip rate
fps_limited    frames skipped by max_fps limiter
queue_drop     raw frames skipped because the writer queue was full
payload        H.264 payload bitrate only
encode_avg     average encode time
encode_max     max encode time

[screen_stream send]
wire           bytes written to stream, including framing, reported as Mbps
write_avg      average async write latency
write_max      max async write latency

[screen_stream receive]
wire           bytes read from stream, including framing, reported as Mbps
decoded        decoded frame rate
seq_gap        skipped sequence numbers; usually sender-side frame dropping
rgba           decoded RGBA memory throughput, not network bitrate
decode_avg     average decode time
decode_max     max decode time

[screen_stream render]
presented         unique frames painted
paints            total paint events
duplicate_paints  paints that reused the same frame
empty_paints      paints before first frame
```

## Recommended Future Improvements

1. Add command-line `--bitrate=...`, `--fps=...`, and `--gop=...` options.
2. Add cumulative byte counters, not only per-second rates.
3. Add a raw/lossless or image-diff test mode for visual quality validation.
4. Add explicit CODECAPI IDR forcing for the MF backend.
5. Add WebRTC/WebCodecs adapters that fan out the same encoded H.264 access units.

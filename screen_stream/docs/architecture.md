# screen_stream Architecture

`screen_stream` is a Windows screen streaming library crate inside the
`real_time_ctrl` workspace. It captures the primary display, encodes frames as
H.264, writes compact packets to an async stream, and decodes or renders those
packets on the receiver side.

The capture implementation intentionally uses DXGI Desktop Duplication instead
of Windows Graphics Capture. This avoids picker/consent UI and visible capture
borders in the user session.

## Module Map

```text
src/lib.rs
  Public crate surface and re-exports.

src/config.rs
  CaptureConfig, H264EncoderConfig, PlayerConfig, encoder backend and presets.

src/capture/dxgi.rs
  Primary monitor capture, FPS limiting, backpressure, H.264 encode, send loop.

src/codec/mod.rs
  RawBgraFrame, EncodedVideoFrame, DecodedVideoFrame data types.

src/codec/h264.rs
  H264Encoder facade, OpenH264 software encoder, OpenH264 decoder.

src/codec/mf_h264.rs
  Windows Media Foundation hardware H.264 encoder path.

src/wire/mod.rs
  RTSS binary packet protocol.

src/stream.rs
  AsyncRead/AsyncWrite framing, packet read/write, receive-and-decode loop.

src/player/native_window.rs
  Win32 native playback window.

src/stats.rs
  Debug performance counters for capture, transport, receive, and render.
```

## Public API

The most common sending API is:

```rust
send_primary_screen(writer, CaptureConfig::smooth()).await?;
```

`writer` can be any `tokio::io::AsyncWrite + Unpin`, so the library can write to
TCP, IPC, encrypted streams, or a caller-provided transport.

The most common receive APIs are:

```rust
play_from(reader, PlayerConfig::default(), |frame| {
    // frame is DecodedVideoFrame with RGBA bytes.
    Ok(())
}).await?;
```

and on Windows:

```rust
play_from_native_window(
    reader,
    PlayerConfig::default(),
    NativeWindowConfig::default(),
).await?;
```

## Sender Execution Path

```text
examples/send_tcp.rs
  -> TcpStream::connect(addr)
  -> stream.set_nodelay(true)
  -> send_primary_screen(stream, config)
  -> spawn_blocking(run_capture_loop)
  -> Monitor::primary()
  -> DxgiDuplicationApi::new(monitor)
  -> H264Encoder::new(config.encoder_config())
  -> send Hello packet
  -> loop:
       acquire_next_frame(timeout)
       apply max_fps limit
       reserve bounded mpsc queue capacity
       map DXGI frame to BGRA
       encode BGRA to H.264
       validate max packet size
       enqueue WirePacket::Video
  -> async writer task:
       write_packet_counted()
       update transport stats
```

The capture loop reserves queue capacity before encoding. If the transport side
is already behind, the raw frame is skipped before expensive encode work starts.
This keeps latency bounded instead of encoding packets that would be dropped
immediately.

When DXGI returns `AccessLost`, the duplication session is recreated, the
encoder is asked for a keyframe, and a fresh `Hello` packet is sent so the
receiver can recover after display mode changes, desktop switches, or fullscreen
transitions.

## Encoder Architecture

`H264Encoder` is a small backend facade:

```text
EncoderBackend::Auto
  -> Try Media Foundation hardware H.264.
  -> Fall back to OpenH264 software encode if hardware init fails.

EncoderBackend::MediaFoundation
  -> Force Windows Media Foundation hardware H.264.
  -> Return an error if no hardware encoder can be initialized.

EncoderBackend::OpenH264
  -> Force portable OpenH264 software encode.
```

The software path converts DXGI BGRA directly into I420 and feeds OpenH264. The
conversion uses a shared BGRA-to-YUV block converter with a same-size fast path
that avoids per-pixel scaling division when the encoded size matches the source
frame. It uses screen-content realtime settings and disables OpenH264 options
that are known to be auto-disabled for screen content, avoiding noisy runtime
warnings.

The Media Foundation path has two input modes. The preferred sender path keeps
the DXGI duplication frame on the GPU, converts/scales it to NV12 with D3D11
VideoProcessor, wraps the NV12 texture with `MFCreateDXGISurfaceBuffer`, and
feeds that D3D11-backed sample into the hardware H.264 MFT through an
`IMFDXGIDeviceManager`. If D3D11 sample input is rejected by a driver, the
encoder falls back to the CPU-input MF path, which fills pooled locked
`IMFMediaBuffer` samples directly from BGRA.

The active GPU path is:

```text
DXGI D3D11 texture
  -> D3D11 VideoProcessor BGRA-to-NV12 and optional scale
  -> D3D11-backed Media Foundation sample via MFCreateDXGISurfaceBuffer
  -> hardware H.264 MFT
  -> existing RTSS H.264 packets
```

## Wire Protocol

`stream.rs` wraps each `WirePacket` body with a simple outer frame:

```text
u32 big-endian body length
body bytes
```

The body is encoded by `wire/mod.rs`:

```text
magic:   "RTSS"
version: u16 = 1
kind:    Hello | Video
```

`Hello` packet:

```text
codec
width
height
fps
bitrate_bps
```

`Video` packet:

```text
seq
timestamp_us
width
height
keyframe flag
payload_len
payload bytes
```

Each video payload is one complete H.264 access unit. Keeping access units
intact makes the same encoded stream usable by native clients, browser
WebCodecs, MSE packaging, or WebRTC RTP packetization without server-side
decode/re-encode.

## Receiver Execution Path

```text
examples/recv_tcp.rs
  -> TcpListener::bind(addr)
  -> accept()
  -> stream.set_nodelay(true)
  -> play_from(stream, config, callback)
  -> receive_decoded()
  -> read_packet_counted()
  -> decode_packet()
  -> require Hello before Video
  -> SoftwareH264Decoder::decode_packet()
  -> callback(DecodedVideoFrame)
```

The current receiver decoder is OpenH264 and outputs RGBA frames. The callback
API lets callers plug in custom rendering, file recording, diagnostics, or a
higher-level application pipeline.

## Native Window Playback Path

```text
examples/recv_window_tcp.rs
  -> TcpListener::bind(addr)
  -> accept()
  -> play_from_native_window(stream, player_config, window_config)
  -> async decode task:
       play_from()
       store newest RenderFrame only
       post WM_STREAM_FRAME
  -> blocking Win32 window task:
       CreateWindowExW
       message loop
       WM_PAINT
       StretchDIBits
```

The native player keeps only the latest decoded frame in a shared slot. Slow
painting, resizing, or expose events therefore do not build playback latency.

Decoded RGBA is converted once to BGRA because Win32 32-bit `BI_RGB` DIB data is
BGRA on little-endian Windows. Paint then hands the same allocation directly to
`StretchDIBits`.

## Browser Playback Design

Browser playback is designed as an adapter over the same encoded H.264 packet
stream, not as a separate capture or server-side decode path.

Recommended production path:

```text
DXGI capture
  -> one H.264 encoder
  -> encoded access-unit fanout
  -> WebRTC H.264 RTP packetization
  -> browser hardware decoder
  -> <video>
```

Local or controlled Chromium/Edge MVP:

```text
encoded access units
  -> WebSocket binary chunks
  -> WebCodecs VideoDecoder
  -> canvas/WebGL render
```

Compatibility fallback:

```text
encoded access units
  -> fragmented MP4
  -> MSE SourceBuffer
  -> <video>
```

The key rule is to encode once and fan out encoded frames. Browser clients
should not force the server to decode and re-encode per viewer.

## Configuration Presets

`CaptureConfig` exposes presets for common tradeoffs:

```text
balanced
  30 fps, up to 1920x1080, 8 Mbps, Auto encoder.

smooth
  60 fps, up to 1280x720, 8 Mbps, low latency.

high_quality
  30 fps, up to 1920x1080, 16 Mbps, tighter QP.

bandwidth_saver
  30 fps, up to 1280x720, 3 Mbps.

native_resolution()
  Removes encoded-size bounds. Useful for quality checks, expensive on 4K.
```

## Stats

Debug builds enable stats by default. Release builds stay quiet unless callers
enable stats explicitly.

Capture stats:

```text
acquired       DXGI frames acquired.
encoded        frames that produced H.264 access units.
encoder_skip   frames skipped internally by the encoder.
fps_limited    frames skipped by configured max_fps.
queue_drop     raw frames skipped because the async writer queue was full.
payload        encoded H.264 bitrate before outer stream framing.
avg_packet     average encoded payload size.
encode_avg     BGRA conversion plus selected H.264 encoder latency.
encode_max     maximum encode latency in the reporting interval.
```

Transport stats:

```text
wire           bytes written to the stream, including protocol overhead.
packets        all packet rate.
video          video packet rate.
write_avg      async write latency.
write_max      maximum write latency.
```

Receive stats:

```text
wire           incoming bitrate.
video          incoming video packet rate.
decoded        decoded frame rate.
seq_gap        skipped sequence numbers.
out_of_order   unexpected sequence movement.
rgba           decoded RGBA throughput.
decode_avg     OpenH264 decode plus RGBA conversion latency.
decode_max     maximum decode latency.
```

Render stats:

```text
presented         unique frames painted into the native window.
paints            total paint events.
duplicate_paints  paint events that reused the same frame.
empty_paints      paint events before the first decoded frame.
```

## Example Commands

Run the native window receiver:

```powershell
cargo run -p screen_stream --release --example recv_window_tcp -- 127.0.0.1:7007 --stats
```

Run the decode-only receiver:

```powershell
cargo run -p screen_stream --release --example recv_tcp -- 127.0.0.1:7007 --stats
```

Run the sender:

```powershell
cargo run -p screen_stream --release --example send_tcp -- 127.0.0.1:7007 smooth --encoder=auto --stats
```

Sender presets:

```text
balanced
smooth | low-latency | latency
quality | high-quality | hq
bandwidth | low-bandwidth | bw
native | source | full
```

Sender flags:

```text
--stats              Enable stats in release builds.
--no-stats           Disable stats in debug builds.
--native             Encode at native desktop resolution.
--max=1600x900       Override encoded-size upper bound.
--encoder=auto       Prefer MF hardware, fall back to OpenH264.
--encoder=mf         Force Media Foundation hardware H.264.
--encoder=software   Force OpenH264 software encode.
```

Probe encoder initialization and throughput:

```powershell
cargo run -p screen_stream --release --example probe_encoder -- auto 120
cargo run -p screen_stream --release --example probe_encoder -- mf 120
cargo run -p screen_stream --release --example probe_encoder -- software 60
```

Compare software and hardware encoder performance:

```powershell
cargo run -p screen_stream --release --example bench_encoder -- --size=1280x720 --frames=240 --warmup=30
cargo run -p screen_stream --release --example bench_encoder -- --size=1920x1080 --frames=120 --warmup=20 --backends=software,mf
```

## Troubleshooting Guide

If `encode_avg` is high and `write_avg` is low, the sender is encoder-bound.
Use `--encoder=mf`, lower `--max`, or use the `smooth` or `bandwidth` preset.

If `write_avg` grows or `queue_drop` increases, the stream is backpressured by
the network, receiver, or renderer. Keep queue capacity low for low-latency
control scenarios and prefer dropping stale frames over buffering.

If the receiver reports `seq_gap`, frames were skipped before or during encode.
For TCP this is usually intentional sender-side latency control rather than
packet reordering.

If native playback repaints but does not advance, inspect `duplicate_paints` and
`decoded`. High duplicate paints with normal decoded rate usually means resize
or expose events are repainting the latest frame; low decoded rate means the
receive/decode path is the bottleneck.

If Media Foundation hardware encode is unavailable, `--encoder=auto` falls back
to OpenH264. Use `--encoder=mf` when a startup error is preferable to silently
using software encode.

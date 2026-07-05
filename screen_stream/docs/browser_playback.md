# Browser Playback Design

## Goals

Browser playback should reuse the same captured and encoded H.264 frames used by native clients. The server should not decode and re-encode per viewer. Each browser client should receive encoded access units and let the browser hardware decoder render them.

The capture side remains DXGI Desktop Duplication. Do not introduce Windows Graphics Capture because it adds picker/consent UI and visible capture borders.

## Recommended Architecture

```text
DXGI capture
  -> one H.264 encoder
  -> encoded access-unit fanout
  -> browser transport adapter
  -> browser hardware decoder
  -> video/canvas render
```

The existing `WirePacket::Video` already carries one complete H.264 access unit with sequence number, timestamp, dimensions, and keyframe flag. Browser playback should build adapters on top of this packet stream.

## Path A: WebRTC, Production Default

Use WebRTC for the production browser path.

Server side:

- Add a `browser-webrtc` feature in `screen_stream` or a sibling `screen_stream_web` crate.
- Use the Rust `webrtc` crate for PeerConnection, signaling hooks, SRTP, RTCP feedback, and congestion control.
- Convert each H.264 access unit to RTP H.264 packets with FU-A/STAP-A packetization.
- Feed receiver feedback into the encoder control path: request keyframe, lower bitrate, lower FPS, or drop P-frames.

Browser side:

- Use `RTCPeerConnection` and attach the remote track to a normal `<video>` element.
- Rendering, sync, and hardware decoding are handled by the browser.

Why this is the best default:

- Lowest practical latency for browsers.
- Good browser compatibility.
- Built-in jitter buffering, packet loss handling, NAT traversal, and adaptive congestion behavior.
- No custom JavaScript decoder pipeline.

Constraints:

- Requires signaling and ICE configuration.
- H.264 profile must stay browser-friendly, typically constrained baseline/main without B-frames.
- SPS/PPS must be sent at session start and before IDR frames.

## Path B: WebSocket + WebCodecs, LAN/Modern Chromium

Use this as the simplest browser MVP for local or controlled Chromium/Edge environments.

Server side:

- Add a `browser-webcodecs` feature.
- Serve a small static HTML/JS player and a WebSocket endpoint.
- Send a binary protocol:
  - `hello`: codec, width, height, fps, SPS/PPS, codec string such as `avc1....`.
  - `chunk`: sequence, timestamp, keyframe flag, encoded H.264 payload.
  - `control`: stats, request-keyframe, close.
- Add H.264 utilities:
  - Annex-B NAL parser.
  - SPS/PPS extraction.
  - IDR/keyframe detection.
  - Annex-B to AVCC conversion when WebCodecs needs length-prefixed NAL units.

Browser side:

- Use `WebSocket` for transport.
- Use `VideoDecoder` from WebCodecs.
- Render decoded `VideoFrame` to `canvas` or WebGL.
- Maintain a tiny frame queue and drop late chunks instead of growing latency.

Why this is useful:

- Very direct mapping from the current wire packets to browser playback.
- Easy to debug.
- No SDP, ICE, or RTP work for the first web demo.

Constraints:

- WebCodecs support is not universal.
- HTTPS/secure-context rules apply outside localhost.
- WebSocket has no real media congestion control, so it is best for localhost/LAN first.

## Path C: MSE Fragmented MP4, Compatibility Mode

Use MSE only when broad compatibility matters more than latency.

Server side:

- Convert H.264 access units into fragmented MP4:
  - init segment with AVC configuration.
  - repeated `moof`/`mdat` media fragments.
- Serve fragments over HTTP chunked response or WebSocket.

Browser side:

- Use `MediaSource` and append fMP4 segments to a `SourceBuffer`.
- Render through a normal `<video>` element.

Tradeoff:

- Good browser support.
- Higher latency and more buffering than WebRTC/WebCodecs.

## Implementation Plan

1. Add codec packet utilities:
   - `H264AccessUnit`
   - `NalUnit`
   - `extract_sps_pps`
   - `annex_b_to_avcc`
   - `is_idr_access_unit`

2. Add an encoded packet fanout:
   - One capture/encoder pipeline.
   - Per-client bounded queues.
   - Drop P-frames when a client falls behind.
   - Retain latest SPS/PPS and request/force keyframe for new clients.

3. Add `browser-webcodecs` MVP:
   - Local HTTP server for `player.html`.
   - WebSocket endpoint that forwards encoded chunks.
   - JavaScript `VideoDecoder` canvas renderer.

4. Add `browser-webrtc` production path:
   - Signaling abstraction supplied by application code.
   - RTP H.264 packetizer.
   - RTCP feedback handling.
   - Bitrate/FPS adaptation hooks.

5. Add MSE compatibility only if needed.

## API Shape

```rust
let publisher = BrowserPublisher::new(capture_config);
publisher.add_webcodecs_client(ws).await?;
publisher.add_webrtc_peer(peer).await?;
```

The browser publisher should consume encoded frames, not raw frames:

```rust
pub trait EncodedFrameSink {
    fn on_stream_info(&mut self, info: &StreamInfo) -> Result<()>;
    fn on_frame(&mut self, frame: &EncodedPacket) -> Result<()>;
}
```

This keeps native TCP clients, WebCodecs clients, MSE clients, and WebRTC peers on the same encoder output.

## Performance Rules

- Encode once, fan out encoded frames.
- Never decode on the server just to serve a browser.
- Keep per-client queues bounded.
- Drop stale P-frames before dropping keyframes.
- Request a fresh IDR frame when a browser joins or loses decoder sync.
- Prefer hardware decode in browsers.
- Keep timestamps monotonic and based on capture time.
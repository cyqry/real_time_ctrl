# Performance and Quality Stats

Debug builds enable periodic stats by default through `DebugStatsConfig::default()`.
Release builds keep stats disabled unless the caller explicitly enables them.

## Sender Metrics

`[screen_stream capture]`

- `acquired`: frames delivered by DXGI Desktop Duplication.
- `encoded`: frames that produced an H.264 access unit.
- `encoder_skip`: frames OpenH264 skipped internally.
- `fps_limited`: frames intentionally skipped to respect `max_fps`.
- `queue_drop`: raw frames skipped before encode because the bounded async writer queue was full.
- `payload`: encoded H.264 payload bitrate, before stream framing.
- `avg_packet`: average encoded frame payload size.
- `encode_avg` / `encode_max`: time spent in BGRA color conversion plus the selected H.264 encoder.

`[screen_stream send]`

- `wire`: bytes actually written to the stream, including the 4-byte length prefix and protocol header.
- `packets` / `video`: packet rate and video packet rate.
- `write_avg` / `write_max`: async write latency. If this grows, the receiver or network is backpressuring the sender.

## Receiver Metrics

`[screen_stream receive]`

- `wire`: incoming stream bitrate.
- `video` / `decoded`: encoded packet rate and decoded frame rate.
- `seq_gap`: sequence numbers skipped by the sender, usually from bounded queue drops or encoder skip.
- `out_of_order`: unexpected sequence movement. TCP should normally keep this at zero.
- `rgba`: decoded RGBA throughput before rendering.
- `decode_avg` / `decode_max`: OpenH264 decode plus RGBA conversion time.

## Native Window Metrics

`[screen_stream render]`

- `presented`: unique frames actually painted into the Win32 window.
- `paints`: total paint events.
- `duplicate_paints`: paint events that reused the same frame, often caused by resize/expose events.
- `empty_paints`: paint events before the first decoded frame.

## Presets

The TCP sender example accepts a preset:

```powershell
cargo run -p screen_stream --release --example send_tcp -- 127.0.0.1:7007 smooth --stats
cargo run -p screen_stream --release --example send_tcp -- 127.0.0.1:7007 quality --stats
cargo run -p screen_stream --release --example send_tcp -- 127.0.0.1:7007 bandwidth --stats
```

Use `smooth` when control latency matters most, `quality` when text clarity matters most, and `bandwidth` when the network is constrained.
For real performance measurements, run with `--release`; debug builds are intentionally instrumented and much slower at per-pixel conversion and software encoding.
Additional sender arguments:

- --stats: enable stats in release builds.
- --no-stats: disable stats in debug builds.
- --native: encode at native desktop resolution. This is useful for quality checks but expensive with software OpenH264.
- --max=1600x900: override the encoded-size upper bound while preserving aspect ratio.

Encoder selection:

- --encoder=auto: prefer Media Foundation hardware H.264 and fall back to OpenH264 if no hardware encoder can be initialized.
- --encoder=mf: force Media Foundation hardware H.264. Use this when you explicitly want GPU encode and prefer a startup error over software fallback.
- --encoder=software: force OpenH264 software encode for comparison.

## Encoder Benchmark

Use the release benchmark example to compare CPU and GPU encoding on the same generated BGRA frames:

```powershell
cargo run -p screen_stream --release --example bench_encoder -- --size=1280x720 --frames=240 --warmup=30
cargo run -p screen_stream --release --example bench_encoder -- --size=1920x1080 --frames=120 --warmup=20
```

The benchmark reports per-call `encode_bgra` latency, so it includes BGRA color conversion plus the selected H.264 encoder. `software` uses OpenH264 CPU encoding. `mf` uses Media Foundation hardware H.264. The current MF path fills pooled Media Foundation input buffers directly from BGRA, avoiding an intermediate NV12 allocation/copy and repeated input sample allocation while keeping the existing wire protocol unchanged.

## D3D11 Zero-Copy Validation

Validate the public D3D11 texture API and decode the produced H.264 packets:

```powershell
cargo run -p screen_stream --release --example probe_zero_copy -- 30
```

Compare real capture encode paths:

```powershell
cargo run -p screen_stream --release --example bench_capture_encoder -- --frames=120 --warmup=20 --backends=mf-cpu,mf-d3d11
```

`mf-cpu` maps the DXGI frame to CPU BGRA and uses the CPU-input Media Foundation path. `mf-d3d11` keeps the DXGI texture on the GPU, converts/scales with D3D11 VideoProcessor, and feeds a D3D11-backed sample to the hardware encoder.

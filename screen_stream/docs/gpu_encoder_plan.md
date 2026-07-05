# GPU Encoder Plan

The current implementation uses DXGI Desktop Duplication for capture and prefers a D3D11 zero-copy Media Foundation path. The primary sender path keeps the captured texture on the GPU, uses D3D11 VideoProcessor to produce an NV12 texture, and feeds that texture to the hardware H.264 MFT through a D3D11 device manager. If a driver rejects D3D11 sample input, it falls back to the CPU-input Media Foundation path, then to OpenH264 only when no hardware encoder is available. This keeps the API free of Windows Graphics Capture consent UI and capture borders.

The observed stats point at the encoder as the bottleneck when `encode_avg` is hundreds of milliseconds and `write_avg` is sub-millisecond. Use `--encoder=auto` or `--encoder=mf` to move H.264 encode to Media Foundation hardware.

## Immediate Software Mitigation

Limit the encoded frame size before OpenH264:

- `CaptureConfig::balanced()`: up to 1920x1080, 30 fps.
- `CaptureConfig::smooth()`: up to 1280x720, 60 fps.
- `CaptureConfig::high_quality()`: up to 1920x1080, higher bitrate and tighter QP.
- `CaptureConfig::bandwidth_saver()`: up to 1280x720, lower bitrate.

The software encoder samples the DXGI BGRA frame directly into the target I420 size before calling OpenH264. This is still CPU work, but it avoids both native-resolution encoding on high-DPI/4K monitors and the earlier BGRA -> RGB -> YUV double conversion. The shared BGRA-to-YUV converter has a same-size fast path so 720p/1080p frames that are not being scaled avoid per-pixel division.

## Full Zero-Copy Hardware Path

The implemented zero-copy GPU path keeps frames on the GPU until the encoded H.264 access unit is produced:

1. Capture with DXGI Output Duplication into a D3D11 texture.
2. Use D3D11 VideoProcessor to convert/copy BGRA into NV12, including downscale when configured.
3. Feed NV12 textures into the Media Foundation H.264 hardware encoder MFT through a D3D11 device manager.
4. Extract encoded H.264 access units and keep the existing wire protocol unchanged.
5. Reuse the same encoded packets for native playback, WebRTC, WebCodecs, and MSE adapters.

This path does not use Windows Graphics Capture.

## Why Media Foundation

Media Foundation hardware H.264 is the most practical Windows-native path:

- Uses vendor GPU encoders through Windows APIs.
- Avoids shipping vendor-specific NVENC/AMF/QSV bindings first.
- Keeps deployment simpler for Intel, AMD, and NVIDIA machines.
- Preserves the current DXGI capture policy with no picker or capture border.

## Expected Metrics

When the GPU path is working, the stats should change like this:

- `encode_avg`: from hundreds of milliseconds to a few milliseconds or low tens of milliseconds.
- `encoded`: approaches configured fps.
- `acquired`: remains high because capture is no longer blocked by software encode.
- GPU video encode engine: visible activity in Task Manager.
- CPU usage: substantially lower than software OpenH264 at the same resolution.
## Implemented Steps

The crate now has an `EncoderBackend` selector. `Auto` prefers the Media Foundation hardware H.264 encoder and falls back to OpenH264 if no hardware encoder can be initialized. `EncoderBackend::MediaFoundation` or `--encoder=mf` forces hardware encode.

The first MF implementation feeds CPU-produced NV12 samples into a hardware encoder MFT and drives the transform through the required asynchronous `IMFMediaEventGenerator` events.

The CPU-input MF fallback fills pooled, locked `IMFMediaBuffer` samples directly from BGRA, so it avoids the previous intermediate NV12 vector, the follow-up copy into the Media Foundation sample, and repeated input sample allocation. The same shared converter also handles the OpenH264 I420 path, keeping color behavior consistent between CPU and MF backends.

The D3D11 zero-copy MF path adds:

- `RawD3D11Frame`, a Windows-only public frame type for borrowed D3D11 texture input.
- `H264Encoder::encode_d3d11`, which accepts a DXGI duplication texture without CPU mapping.
- `send_primary_screen` now tries D3D11 input first for the Media Foundation backend and falls back to CPU input if unavailable.
- A D3D11 VideoProcessor BGRA-to-NV12/scaling stage.
- `MFCreateDXGISurfaceBuffer` samples submitted to the hardware encoder through `IMFDXGIDeviceManager`.

Measured on the current development machine with `bench_encoder`:

```text
1280x720 MF: 11.5ms avg before, 8.6ms avg after.
1920x1080 MF: 20.4ms avg before, 12.9ms avg after.
```

Measured on the current development machine with real DXGI capture at source 2560x1440 and encoded 1280x720:

```text
mf-cpu:   9.66ms avg, 10.66ms p95.
mf-d3d11: 4.87ms avg,  5.59ms p95.
speedup:  1.99x.
```

Correctness verification:

```powershell
cargo run -p screen_stream --release --example probe_zero_copy -- 10
```

The probe captures real DXGI textures, calls the public `encode_d3d11` API, decodes the produced H.264 packets, and validates sequence number, dimensions, decoded RGBA length, and decodable frame count.

The next hardware step is broader driver validation across Intel, AMD, and NVIDIA machines, plus optional shader-based conversion if VideoProcessor quality or driver behavior is not acceptable on a target GPU.

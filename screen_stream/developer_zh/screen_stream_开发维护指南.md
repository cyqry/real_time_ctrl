# screen_stream 开发维护指南

本文档面向有一定 Rust、Windows 客户端、音视频或网络开发基础的维护者，目标是让开发者能快速理解 `screen_stream` 的架构、核心执行路线、关键技术选择和复现测试方式。

本文档不放在原有 `screen_stream/docs` 目录下，避免和历史英文设计文档混在一起。

## 1. 模块定位

`screen_stream` 是 `real_time_ctrl` 工作区里的一个 lib crate，用于 Windows 桌面屏幕流传输。它完成以下工作：

1. 采集主屏幕画面。
2. 将画面编码为 H.264 access unit。
3. 通过简单二进制协议写入任意 `AsyncWrite`，当前 examples 使用 TCP。
4. 接收端从 `AsyncRead` 读取 packet，使用 OpenH264 解码成 RGBA。
5. 可选地把解码帧渲染到 Win32 原生窗口。

当前采集实现使用 DXGI Desktop Duplication，而不是 Windows Graphics Capture。这样可以避免用户会话中的选择器、授权弹窗和采集边框，更适合作为远控或后台屏幕流能力。

## 2. 目录结构

```text
screen_stream/
  Cargo.toml
  src/
    lib.rs                  对外 API 和模块导出
    config.rs               采集、编码、播放配置和预设
    error.rs                crate 统一错误类型
    stats.rs                采集、发送、接收、渲染统计
    stream.rs               AsyncRead/AsyncWrite 外层封包读写和解码循环
    wire/mod.rs             RTSS 二进制协议编码/解码
    capture/dxgi.rs         DXGI Desktop Duplication 采集与发送主循环
    codec/mod.rs            原始帧、编码帧、解码帧数据结构
    codec/h264.rs           H264Encoder facade、OpenH264 编码/解码
    codec/mf_h264.rs        Windows Media Foundation 硬件 H.264 编码
    codec/colorspace.rs     BGRA -> I420/NV12 色彩空间转换
    player/native_window.rs Win32 原生窗口播放
  examples/
    send_tcp.rs             TCP 发送端
    recv_tcp.rs             TCP 解码测试接收端，不开窗口
    recv_window_tcp.rs      TCP 原生窗口接收端
    probe_encoder.rs        编码器初始化/吞吐探测
    probe_zero_copy.rs      D3D11 zero-copy 探测
    bench_encoder.rs        编码器基准测试
    bench_capture_encoder.rs采集+编码基准测试
```

## 3. 对外 API

发送端最常用入口：

```rust
send_primary_screen(writer, CaptureConfig::smooth()).await?;
```

`writer` 只要求实现 `tokio::io::AsyncWrite + Unpin`，所以可以接 TCP、IPC、加密流或业务层自定义 transport。

接收端常用入口：

```rust
play_from(reader, PlayerConfig::default(), |frame| {
    // frame 是 DecodedVideoFrame，包含 RGBA bytes。
    Ok(())
}).await?;
```

Windows 原生窗口播放入口：

```rust
play_from_native_window(
    reader,
    PlayerConfig::default(),
    NativeWindowConfig::default(),
).await?;
```

## 4. 发送端核心路线

命令：

```powershell
cargo run -p screen_stream --release --example send_tcp -- 127.0.0.1:7007 quality --mf --stats
```

执行路径：

```text
examples/send_tcp.rs
  -> TcpStream::connect(addr)
  -> stream.set_nodelay(true)
  -> send_primary_screen(stream, config)
  -> tokio::sync::mpsc::channel(queue_capacity)
  -> spawn_blocking(run_capture_loop)
  -> Monitor::primary()
  -> DxgiDuplicationApi::new(monitor)
  -> H264Encoder::new(config.encoder_config())
  -> send Hello packet
  -> loop:
       acquire_next_frame(timeout)
       按 max_fps 限帧
       try_reserve writer 队列容量
       D3D11 texture zero-copy 编码，失败后回退 CPU 输入
       生成 WirePacket::Video
       permit.send(packet)
  -> async writer loop:
       rx.recv()
       write_packet_counted(writer, packet)
       更新 send 统计
```

设计重点：

1. 采集和编码运行在 `spawn_blocking` 线程，避免阻塞 Tokio runtime。
2. 编码前先 `try_reserve()` 有界队列容量。队列满说明网络或接收端落后，此时直接丢原始帧，避免编码出马上要丢弃的 stale packet。
3. `queue_capacity` 通常很小，默认 1 或 2，目标是低延迟，不是完整保帧。
4. DXGI `AccessLost` 时会重建 duplication、请求关键帧并重新发送 Hello，适配显示模式变化、桌面切换和部分全屏切换。

## 5. 编码器架构

`H264Encoder` 是 facade，隐藏具体后端：

```text
EncoderBackend::Auto
  -> 优先 Media Foundation 硬件 H.264
  -> 失败后回退 OpenH264 软件编码

EncoderBackend::MediaFoundation
  -> 强制 Media Foundation 硬件 H.264
  -> 初始化失败直接报错

EncoderBackend::OpenH264
  -> 强制 OpenH264 软件编码
```

### 5.1 OpenH264 软件路径

```text
DXGI BGRA
  -> colorspace::fill_i420_from_bgra()
  -> OpenH264 Encoder
  -> EncodedVideoFrame
```

软件路径优势是可移植、状态简单；缺点是高分辨率下 CPU 压力大，尤其是 1920x1080 或 4K 桌面。

### 5.2 Media Foundation 硬件路径

优先路径：

```text
DXGI D3D11 texture
  -> D3D11 VideoProcessor: BGRA -> NV12，可同时缩放
  -> MFCreateDXGISurfaceBuffer 包装为 D3D11-backed sample
  -> Media Foundation hardware H.264 MFT
  -> H.264 access unit
```

如果 D3D11 sample 输入被驱动或 MFT 拒绝，会回退到 CPU 输入：

```text
DXGI frame buffer
  -> BGRA -> NV12
  -> pooled IMFMediaBuffer sample
  -> Media Foundation hardware H.264 MFT
```

维护注意点：

1. MF 硬编 MFT 是事件驱动的异步 transform，会通过 `METransformNeedInput` 和 `METransformHaveOutput` 通知输入/输出状态。
2. `pump_events(max_wait)` 不能无限等待，否则会卡死采集线程。
3. D3D11 zero-copy 路径必须开启 `ID3D11Multithread::SetMultithreadProtected(true)`。DXGI 采集线程、D3D11 VideoProcessor 和 MF 硬编 MFT 会共享同一个 device/context，部分驱动在未开启多线程保护时会偶发卡在 GPU/MFT 同步点。
4. 关键帧请求目前是 best-effort。多数硬编器会在开流和自身 GOP 周期产生 IDR；如果业务需要更强控制，后续可接入 CODECAPI。

## 6. 线协议

`stream.rs` 的外层 framing：

```text
u32 big-endian body_len
body bytes
```

`wire/mod.rs` 的 body 协议：

```text
magic:   "RTSS"
version: u16 = 1
kind:    Hello | Video
```

Hello packet：

```text
codec
width
height
fps
bitrate_bps
```

Video packet：

```text
seq
timestamp_us
width
height
keyframe flag
payload_len
payload bytes
```

每个 video payload 是一个完整 H.264 access unit。这样做的好处是：同一份编码数据可以直接给原生解码器、WebCodecs、MSE 或 RTP packetizer 使用，不需要服务器端先解码再编码。

## 7. 接收端核心路线

无窗口解码测试命令：

```powershell
cargo run -p screen_stream --release --example recv_tcp -- 127.0.0.1:7007 --stats
```

执行路径：

```text
examples/recv_tcp.rs
  -> TcpListener::bind(addr)
  -> accept()
  -> stream.set_nodelay(true)
  -> play_from(stream, config, callback)
  -> receive_decoded()
  -> read_packet_counted()
  -> decode_packet()
  -> Hello 配置 stream codec
  -> Video 使用 OpenH264 decoder 解码
  -> 输出 DecodedVideoFrame(RGBA)
  -> callback(frame)
```

当前接收端解码器是 OpenH264，输出 RGBA。这个 API 适合接入自定义渲染、录制、诊断或上层业务 pipeline。

## 8. Win32 原生窗口播放路线

窗口接收命令：

```powershell
cargo run -p screen_stream --release --example recv_window_tcp -- 127.0.0.1:7007 --stats
```

执行路径：

```text
examples/recv_window_tcp.rs
  -> TcpListener::bind(addr)
  -> accept()
  -> play_from_native_window()
  -> async decode task:
       play_from()
       解码 RGBA
       转成 GDI 需要的 BGRA
       存入 shared.latest，只保留最新帧
       合并投递 WM_STREAM_FRAME
  -> blocking window task:
       RegisterClassW
       CreateWindowExW
       GetMessageW message loop
       WM_STREAM_FRAME / WM_SIZE -> InvalidateRect
       WM_PAINT -> StretchDIBits
```

窗口端只保留最新帧，不排队历史帧。这样当绘制、缩放或窗口暴露事件变慢时，不会累积播放延迟。

最新实现还会合并重绘消息：如果上一个 `WM_STREAM_FRAME` 还没被 `WM_PAINT` 消费，解码线程不会继续投递重复消息。这个逻辑和“只保留最新帧”是一致的。

## 9. 配置预设

发送端 preset：

```text
balanced
  30 fps, 最大 1920x1080, 8 Mbps, Auto encoder

smooth | low-latency | latency
  60 fps, 最大 1280x720, 8 Mbps, 更低队列容量

quality | high-quality | hq
  30 fps, 最大 1920x1080, 16 Mbps, 更高质量

bandwidth | low-bandwidth | bw
  30 fps, 最大 1280x720, 3 Mbps

native | source | full
  不限制编码分辨率，按原屏幕尺寸编码
```

发送端常用参数：

```text
--stats / --stat        release 下开启统计日志
--no-stats              关闭统计日志
--native                不缩放，按原屏幕尺寸编码
--max=1600x900          覆盖编码分辨率上限
--encoder=auto / --auto 自动选择编码器
--encoder=mf / --mf     强制 Media Foundation 硬编
--encoder=software      强制 OpenH264 软件编码
```

## 10. 统计指标

发送端 transport：

```text
[screen_stream send]
wire        写入 stream 的总码率，包含外层 framing
packets     所有 packet 速率
video       video packet 速率
hello       Hello packet 数
keyframes   关键帧数
avg_packet  平均 packet 大小
write_avg   平均写耗时
write_max   最大写耗时
```

采集/编码：

```text
[screen_stream capture]
acquired       DXGI 成功采集的帧速率
encoded        成功编码输出的帧速率
encoder_skip   编码器内部跳帧速率
fps_limited    因 max_fps 限制跳过的帧速率
queue_drop     writer 队列满时跳过的原始帧速率
payload        H.264 payload 码率，不含外层 framing
avg_packet     平均 H.264 payload 大小
keyframes      关键帧数
encode_avg     编码平均耗时
encode_max     编码最大耗时
```

接收/解码：

```text
[screen_stream receive]
wire          读取 stream 的总码率，包含外层 framing
packets       所有 packet 速率
video         video packet 速率
decoded       成功解码帧速率
hello         Hello packet 数
keyframes     关键帧数
seq_gap       序号跳变，通常表示发送端主动丢帧
out_of_order  乱序计数，TCP 下一般应为 0
rgba          解码后 RGBA 内存吞吐，不是网络码率
decode_avg    平均解码耗时
decode_max    最大解码耗时
```

渲染：

```text
[screen_stream render]
presented         实际展示的新帧速率
paints            WM_PAINT 绘制速率
duplicate_paints  重复绘制同一帧次数
empty_paints      首帧前空绘制次数
```

## 11. 核心测试脚本

### 11.1 原生窗口播放 + MF 硬编

先开接收端：

```powershell
cargo run -p screen_stream --release --example recv_window_tcp -- 127.0.0.1:7007 --stats
```

再开发送端：

```powershell
cargo run -p screen_stream --release --example send_tcp -- 127.0.0.1:7007 quality --mf --stats
```

这是最接近实际桌面流播放的核心路径，覆盖 DXGI 采集、D3D11 zero-copy、Media Foundation 硬编、TCP 传输、OpenH264 解码和 Win32 渲染。

### 11.2 无窗口接收，排除渲染因素

接收端：

```powershell
cargo run -p screen_stream --release --example recv_tcp -- 127.0.0.1:7007 --stats
```

发送端：

```powershell
cargo run -p screen_stream --release --example send_tcp -- 127.0.0.1:7007 quality --mf --stats
```

如果无窗口接收稳定，而窗口接收异常，优先查 `player/native_window.rs` 或窗口导致的桌面内容变化。

### 11.3 原屏幕清晰度测试

接收端：

```powershell
cargo run -p screen_stream --release --example recv_window_tcp -- 127.0.0.1:7007 --stats
```

发送端：

```powershell
cargo run -p screen_stream --release --example send_tcp -- 127.0.0.1:7007 quality --native --mf --stats
```

说明：

1. `--native` 表示不缩放，按原屏幕尺寸编码。
2. `quality` 当前使用 16 Mbps，高分辨率桌面或快速变化内容仍可能有 H.264 压缩痕迹。
3. 如果要更接近像素级原图，后续建议增加命令行 `--bitrate=...` 或 raw/lossless 测试模式。

### 11.4 软件编码对比

```powershell
cargo run -p screen_stream --release --example recv_window_tcp -- 127.0.0.1:7007 --stats
cargo run -p screen_stream --release --example send_tcp -- 127.0.0.1:7007 quality --software --stats
```

软件编码用于对比硬编问题，但 1080p 以上可能明显吃 CPU，帧率降低是正常现象。

### 11.5 编译和测试

```powershell
cargo fmt -p screen_stream
cargo check -p screen_stream
cargo check -p screen_stream --examples
cargo test -p screen_stream
```

## 12. 常见问题定位

### 12.1 发送和接收都不动，窗口仍在重复绘制最后一帧

典型表现：

```text
[screen_stream receive] 停止输出
[screen_stream render] presented=0.0/s，duplicate_paints 增加
[screen_stream send] 或 [screen_stream capture] 停止输出
```

优先判断：

1. 窗口线程还活着，只是在绘制旧帧。
2. 接收解码线程没有继续收到新包。
3. TCP 读端不消费后，发送端最终反压。
4. 如果只在 `--mf` 时出现，重点看 D3D11/MF 硬编状态机和多线程保护。

当前已修复过一次：D3D11 zero-copy 硬编路径需要开启 `ID3D11Multithread::SetMultithreadProtected(true)`，并合并窗口重绘消息。

### 12.2 `write_avg` 或 `write_max` 变大

说明写 socket 变慢，可能是接收端解码慢、窗口渲染慢、网络慢或系统调度抖动。

观察：

```text
[screen_stream send] write_avg / write_max
[screen_stream capture] queue_drop
[screen_stream receive] decoded / decode_avg
[screen_stream render] presented
```

### 12.3 `queue_drop` 上升

说明发送端的 writer 队列满了，采集线程主动跳过原始帧。这是低延迟设计的一部分，不一定是 bug。

### 12.4 `seq_gap` 上升

TCP 不会乱序，`seq_gap` 通常表示发送端主动丢帧，例如队列满或编码器跳帧。远控场景一般宁愿丢旧帧，也不希望积压延迟。

## 13. 后续改进建议

1. 增加 `--bitrate=...`、`--fps=...`、`--gop=...` 命令行参数，方便快速做质量/带宽实验。
2. 增加累计字节数统计，例如 `wire_total` 和 `payload_total`，便于长期压测。
3. 增加 raw/lossless 或 PNG diff 测试工具，用于验证“原屏幕清晰度”。
4. 为 MF 硬编接入 CODECAPI，支持显式强制 IDR。
5. 为浏览器播放实现 WebRTC H.264 RTP 或 WebCodecs adapter，保持服务器端只编码一次。

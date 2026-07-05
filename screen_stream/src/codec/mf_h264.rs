use std::{
    collections::VecDeque,
    mem::ManuallyDrop,
    ptr::null_mut,
    thread,
    time::{Duration, Instant},
};

use windows::{
    core::{Error as WinError, Interface, HRESULT},
    Win32::{
        Foundation::{RECT, RPC_E_CHANGED_MODE},
        Graphics::{
            Direct3D11::{
                ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource,
                ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
                ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
                ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
                D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VIDEO_ENCODER, D3D11_TEX2D_VPIV,
                D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
            },
            Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC},
        },
        Media::MediaFoundation::{
            IMFActivate, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaEventGenerator, IMFSample,
            IMFTransform, MEError, METransformHaveOutput, METransformNeedInput,
            MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
            MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSampleExtension_CleanPoint,
            MFShutdown, MFStartup, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
            MFVideoInterlace_Progressive, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
            MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
            MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
            MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_INFO, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
            MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT, MF_E_NOTACCEPTING,
            MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
            MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
            MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK,
            MF_VERSION,
        },
        System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED},
    },
};

use crate::{
    codec::{colorspace, EncodedVideoFrame, RawBgraFrame, RawD3D11Frame},
    config::H264EncoderConfig,
    error::{Result, ScreenStreamError},
};

pub struct MediaFoundationH264Encoder {
    config: H264EncoderConfig,
    cpu_state: Option<MfEncoderState>,
    d3d11_state: Option<D3D11MfEncoderState>,
    d3d11_disabled: bool,
    force_keyframe_requested: bool,
    // Media Foundation 运行时必须放在最后，确保 drop 时先释放上面的 COM/MF
    // 对象，再调用 MFShutdown 和 CoUninitialize。
    _runtime: MfRuntime,
}

impl MediaFoundationH264Encoder {
    pub fn new(config: H264EncoderConfig) -> Result<Self> {
        validate_config(&config)?;
        Ok(Self {
            config,
            cpu_state: None,
            d3d11_state: None,
            d3d11_disabled: false,
            force_keyframe_requested: false,
            _runtime: MfRuntime::new()?,
        })
    }

    pub fn request_keyframe(&mut self) {
        // 目前 MF 路径对关键帧请求采取 best-effort 策略。大多数硬件编码器会在
        // 开流和自身 GOP 周期内产生 IDR；后续可以再接入 CODECAPI 做显式强制。
        self.force_keyframe_requested = true;
    }

    pub fn encode_bgra(
        &mut self,
        frame: RawBgraFrame<'_>,
        seq: u64,
        force_keyframe: bool,
    ) -> Result<Option<EncodedVideoFrame>> {
        if force_keyframe {
            self.request_keyframe();
        }

        let (width, height) = self.config.encoded_dimensions(frame.width, frame.height)?;
        if self
            .cpu_state
            .as_ref()
            .is_none_or(|state| state.width != width || state.height != height)
        {
            self.cpu_state = Some(MfEncoderState::new(&self.config, width, height)?);
            self.force_keyframe_requested = true;
        }

        let state = self.cpu_state.as_mut().expect("state initialized");
        let output = state.encode_bgra(
            frame.data,
            frame.width as usize,
            frame.height as usize,
            seq,
            frame.timestamp_us,
        )?;
        self.force_keyframe_requested = false;
        Ok(output)
    }

    pub fn encode_d3d11(
        &mut self,
        frame: RawD3D11Frame<'_>,
        seq: u64,
        force_keyframe: bool,
    ) -> Result<Option<EncodedVideoFrame>> {
        if self.d3d11_disabled {
            return Err(ScreenStreamError::InvalidFrame(
                "D3D11 Media Foundation input was disabled after a previous failure".into(),
            ));
        }

        if force_keyframe {
            self.request_keyframe();
        }

        let (width, height) = self.config.encoded_dimensions(frame.width, frame.height)?;
        if self.d3d11_state.as_ref().is_none_or(|state| {
            state.width != width
                || state.height != height
                || state.source_width != frame.width
                || state.source_height != frame.height
        }) {
            match D3D11MfEncoderState::new(
                &self.config,
                frame.device,
                frame.context,
                frame.width,
                frame.height,
                width,
                height,
            ) {
                Ok(state) => {
                    self.d3d11_state = Some(state);
                    self.force_keyframe_requested = true;
                }
                Err(err) => {
                    self.d3d11_disabled = true;
                    return Err(err);
                }
            }
        }

        let state = self.d3d11_state.as_mut().expect("state initialized");
        let output = match state.encode_texture(frame.texture, seq, frame.timestamp_us) {
            Ok(output) => output,
            Err(err) => {
                self.d3d11_disabled = true;
                return Err(err);
            }
        };
        self.force_keyframe_requested = false;
        Ok(output)
    }
}

struct MfEncoderState {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    output_info: MFT_OUTPUT_STREAM_INFO,
    pending: VecDeque<PendingFrame>,
    ready: VecDeque<EncodedVideoFrame>,
    free_inputs: Vec<InputSample>,
    width: u32,
    height: u32,
    frame_duration_hns: i64,
    needs_input: bool,
}

struct PendingFrame {
    seq: u64,
    timestamp_us: u64,
    input: InputSample,
}

struct InputSample {
    sample: IMFSample,
    buffer: IMFMediaBuffer,
    capacity: usize,
}

struct D3D11MfEncoderState {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    output_info: MFT_OUTPUT_STREAM_INFO,
    pending: VecDeque<D3D11PendingFrame>,
    ready: VecDeque<EncodedVideoFrame>,
    free_inputs: Vec<D3D11InputSample>,
    converter: D3D11VideoConverter,
    _device_manager: IMFDXGIDeviceManager,
    source_width: u32,
    source_height: u32,
    width: u32,
    height: u32,
    frame_duration_hns: i64,
    needs_input: bool,
}

struct D3D11PendingFrame {
    seq: u64,
    timestamp_us: u64,
    input: D3D11InputSample,
}

struct D3D11InputSample {
    _texture: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    sample: IMFSample,
}

struct D3D11VideoConverter {
    device: ID3D11Device,
    _context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    _source_width: u32,
    _source_height: u32,
    width: u32,
    height: u32,
}

impl MfEncoderState {
    fn new(config: &H264EncoderConfig, width: u32, height: u32) -> Result<Self> {
        let transform = create_hardware_transform()?;
        configure_transform_attributes(&transform);
        configure_transform(&transform, config, width, height)?;
        let events = transform
            .cast::<IMFMediaEventGenerator>()
            .map_err(mf_error("cast IMFMediaEventGenerator"))?;
        let output_info = unsafe {
            transform
                .GetOutputStreamInfo(0)
                .map_err(mf_error("GetOutputStreamInfo"))?
        };

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(mf_error("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(mf_error("MFT_MESSAGE_NOTIFY_START_OF_STREAM"))?;
        }

        let mut state = Self {
            transform,
            events,
            output_info,
            pending: VecDeque::new(),
            ready: VecDeque::new(),
            free_inputs: Vec::new(),
            width,
            height,
            frame_duration_hns: 10_000_000_i64 / config.max_fps.max(1) as i64,
            needs_input: false,
        };

        // 硬件编码 MFT 通过事件通知输入可用。初始化时短暂等待初始
        // NeedInput 事件，尽量让第一帧采集后可以立即提交。
        state.pump_events(Duration::from_millis(50))?;
        Ok(state)
    }

    fn encode_bgra(
        &mut self,
        bgra: &[u8],
        source_width: usize,
        source_height: usize,
        seq: u64,
        timestamp_us: u64,
    ) -> Result<Option<EncodedVideoFrame>> {
        self.pump_events(Duration::from_millis(0))?;

        if !self.needs_input {
            self.pump_events(Duration::from_millis(2))?;
        }

        if self.needs_input {
            let mut input = self.take_input_sample()?;
            input.fill_from_bgra(
                bgra,
                source_width,
                source_height,
                self.width as usize,
                self.height as usize,
                timestamp_us,
                self.frame_duration_hns,
            )?;

            unsafe {
                match self.transform.ProcessInput(0, &input.sample, 0) {
                    Ok(()) => {
                        self.needs_input = false;
                        self.pending.push_back(PendingFrame {
                            seq,
                            timestamp_us,
                            input,
                        });
                    }
                    Err(err) if err.code() == MF_E_NOTACCEPTING => {
                        // 将 NOTACCEPTING 视为编码器反压。下一次调用会重新 pump
                        // 事件，并用更新的采集帧重试。
                        self.recycle_input_sample(input);
                    }
                    Err(err) => return Err(mf_error("ProcessInput")(err)),
                }
            }
        }

        // 短暂轮询匹配的输出，但不能无限阻塞采集线程。延迟输出的帧会进入
        // ready 队列，在后续调用返回，从而保持低延迟。
        self.pump_events(Duration::from_millis(4))?;
        Ok(self.ready.pop_front())
    }

    fn take_input_sample(&mut self) -> Result<InputSample> {
        let required = colorspace::nv12_len(self.width as usize, self.height as usize)?;
        match self.free_inputs.pop() {
            Some(input) if input.capacity >= required => Ok(input),
            _ => InputSample::new(required),
        }
    }

    fn recycle_input_sample(&mut self, input: InputSample) {
        if self.free_inputs.len() < 4 {
            self.free_inputs.push(input);
        }
    }

    fn pump_events(&mut self, max_wait: Duration) -> Result<()> {
        let started = Instant::now();
        let mut events_seen = 0_usize;

        loop {
            if events_seen > 64 {
                return Err(ScreenStreamError::InvalidFrame(
                    "Media Foundation encoder produced too many events in one pump".into(),
                ));
            }

            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if started.elapsed() >= max_wait {
                        return Ok(());
                    }
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(err) => return Err(mf_error("GetEvent")(err)),
            };

            events_seen += 1;
            let event_status = unsafe { event.GetStatus().map_err(mf_error("event GetStatus"))? };
            if event_status.is_err() {
                return Err(mf_hresult("Media Foundation encoder event", event_status));
            }

            let event_type = unsafe { event.GetType().map_err(mf_error("event GetType"))? };
            if event_type == METransformNeedInput.0 as u32 {
                self.needs_input = true;
            } else if event_type == METransformHaveOutput.0 as u32 {
                if let Some(frame) = self.process_output()? {
                    self.ready.push_back(frame);
                }
            } else if event_type == MEError.0 as u32 {
                return Err(ScreenStreamError::InvalidFrame(
                    "Media Foundation encoder reported MEError".into(),
                ));
            }
        }
    }

    fn process_output(&mut self) -> Result<Option<EncodedVideoFrame>> {
        let provides_samples =
            self.output_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let output_sample = if provides_samples {
            None
        } else {
            Some(create_output_sample(self.output_buffer_size())?)
        };

        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(output_sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0_u32;

        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };
        match result {
            Ok(()) => {}
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                drop_output_buffer(output);
                return Ok(None);
            }
            Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                drop_output_buffer(output);
                self.output_info = unsafe {
                    self.transform
                        .GetOutputStreamInfo(0)
                        .map_err(mf_error("GetOutputStreamInfo after stream change"))?
                };
                return Ok(None);
            }
            Err(err) => {
                drop_output_buffer(output);
                return Err(mf_error("ProcessOutput")(err));
            }
        }

        let sample = unsafe { ManuallyDrop::take(&mut output.pSample) }.ok_or_else(|| {
            ScreenStreamError::Codec(openh264::Error::msg("Media Foundation returned no sample"))
        })?;
        let _events = unsafe { ManuallyDrop::take(&mut output.pEvents) };
        let payload = read_sample_bytes(&sample)?;
        if payload.is_empty() {
            return Ok(None);
        }

        let is_keyframe =
            unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0 };
        let pending = self.pending.pop_front().ok_or_else(|| {
            ScreenStreamError::InvalidFrame(
                "Media Foundation returned output without a pending input frame".into(),
            )
        })?;
        let PendingFrame {
            seq,
            timestamp_us,
            input,
        } = pending;
        self.recycle_input_sample(input);

        Ok(Some(EncodedVideoFrame {
            seq,
            timestamp_us,
            width: self.width,
            height: self.height,
            is_keyframe,
            payload,
        }))
    }

    fn output_buffer_size(&self) -> u32 {
        let raw_size = self.width.saturating_mul(self.height).saturating_mul(3) / 2;
        self.output_info.cbSize.max(raw_size).max(1024 * 1024)
    }
}

impl D3D11MfEncoderState {
    fn new(
        config: &H264EncoderConfig,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source_width: u32,
        source_height: u32,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let transform = create_hardware_transform()?;
        configure_transform_attributes(&transform);
        enable_d3d11_multithread_protection(device)?;
        let device_manager = create_d3d11_device_manager(device)?;
        unsafe {
            transform
                .ProcessMessage(
                    MFT_MESSAGE_SET_D3D_MANAGER,
                    Interface::as_raw(&device_manager) as usize,
                )
                .map_err(mf_error("MFT_MESSAGE_SET_D3D_MANAGER"))?;
        }
        configure_transform(&transform, config, width, height)?;
        let events = transform
            .cast::<IMFMediaEventGenerator>()
            .map_err(mf_error("cast IMFMediaEventGenerator"))?;
        let output_info = unsafe {
            transform
                .GetOutputStreamInfo(0)
                .map_err(mf_error("GetOutputStreamInfo"))?
        };
        let converter =
            D3D11VideoConverter::new(device, context, source_width, source_height, width, height)?;

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(mf_error("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(mf_error("MFT_MESSAGE_NOTIFY_START_OF_STREAM"))?;
        }

        let mut state = Self {
            transform,
            events,
            output_info,
            pending: VecDeque::new(),
            ready: VecDeque::new(),
            free_inputs: Vec::new(),
            converter,
            _device_manager: device_manager,
            source_width,
            source_height,
            width,
            height,
            frame_duration_hns: 10_000_000_i64 / config.max_fps.max(1) as i64,
            needs_input: false,
        };

        state.pump_events(Duration::from_millis(50))?;
        Ok(state)
    }

    fn encode_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        seq: u64,
        timestamp_us: u64,
    ) -> Result<Option<EncodedVideoFrame>> {
        self.pump_events(Duration::from_millis(0))?;

        if !self.needs_input {
            self.pump_events(Duration::from_millis(2))?;
        }

        if self.needs_input {
            let mut input = self.take_input_sample()?;
            input.fill_from_texture(
                &self.converter,
                texture,
                timestamp_us,
                self.frame_duration_hns,
            )?;

            unsafe {
                match self.transform.ProcessInput(0, &input.sample, 0) {
                    Ok(()) => {
                        self.needs_input = false;
                        self.pending.push_back(D3D11PendingFrame {
                            seq,
                            timestamp_us,
                            input,
                        });
                    }
                    Err(err) if err.code() == MF_E_NOTACCEPTING => {
                        self.recycle_input_sample(input);
                    }
                    Err(err) => return Err(mf_error("D3D11 ProcessInput")(err)),
                }
            }
        }

        self.pump_events(Duration::from_millis(4))?;
        Ok(self.ready.pop_front())
    }

    fn take_input_sample(&mut self) -> Result<D3D11InputSample> {
        match self.free_inputs.pop() {
            Some(input) => Ok(input),
            None => D3D11InputSample::new(&self.converter),
        }
    }

    fn recycle_input_sample(&mut self, input: D3D11InputSample) {
        if self.free_inputs.len() < 4 {
            self.free_inputs.push(input);
        }
    }

    fn pump_events(&mut self, max_wait: Duration) -> Result<()> {
        let started = Instant::now();
        let mut events_seen = 0_usize;

        loop {
            if events_seen > 64 {
                return Err(ScreenStreamError::InvalidFrame(
                    "Media Foundation D3D11 encoder produced too many events in one pump".into(),
                ));
            }

            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if started.elapsed() >= max_wait {
                        return Ok(());
                    }
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(err) => return Err(mf_error("D3D11 GetEvent")(err)),
            };

            events_seen += 1;
            let event_status = unsafe { event.GetStatus().map_err(mf_error("event GetStatus"))? };
            if event_status.is_err() {
                return Err(mf_hresult(
                    "Media Foundation D3D11 encoder event",
                    event_status,
                ));
            }

            let event_type = unsafe { event.GetType().map_err(mf_error("event GetType"))? };
            if event_type == METransformNeedInput.0 as u32 {
                self.needs_input = true;
            } else if event_type == METransformHaveOutput.0 as u32 {
                if let Some(frame) = self.process_output()? {
                    self.ready.push_back(frame);
                }
            } else if event_type == MEError.0 as u32 {
                return Err(ScreenStreamError::InvalidFrame(
                    "Media Foundation D3D11 encoder reported MEError".into(),
                ));
            }
        }
    }

    fn process_output(&mut self) -> Result<Option<EncodedVideoFrame>> {
        let provides_samples =
            self.output_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let output_sample = if provides_samples {
            None
        } else {
            Some(create_output_sample(self.output_buffer_size())?)
        };

        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(output_sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0_u32;

        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };
        match result {
            Ok(()) => {}
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                drop_output_buffer(output);
                return Ok(None);
            }
            Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                drop_output_buffer(output);
                self.output_info = unsafe {
                    self.transform
                        .GetOutputStreamInfo(0)
                        .map_err(mf_error("GetOutputStreamInfo after D3D11 stream change"))?
                };
                return Ok(None);
            }
            Err(err) => {
                drop_output_buffer(output);
                return Err(mf_error("D3D11 ProcessOutput")(err));
            }
        }

        let sample = unsafe { ManuallyDrop::take(&mut output.pSample) }.ok_or_else(|| {
            ScreenStreamError::Codec(openh264::Error::msg(
                "Media Foundation D3D11 encoder returned no sample",
            ))
        })?;
        let _events = unsafe { ManuallyDrop::take(&mut output.pEvents) };
        let payload = read_sample_bytes(&sample)?;
        if payload.is_empty() {
            return Ok(None);
        }

        let is_keyframe =
            unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0 };
        let pending = self.pending.pop_front().ok_or_else(|| {
            ScreenStreamError::InvalidFrame(
                "Media Foundation D3D11 returned output without a pending input frame".into(),
            )
        })?;
        let D3D11PendingFrame {
            seq,
            timestamp_us,
            input,
        } = pending;
        self.recycle_input_sample(input);

        Ok(Some(EncodedVideoFrame {
            seq,
            timestamp_us,
            width: self.width,
            height: self.height,
            is_keyframe,
            payload,
        }))
    }

    fn output_buffer_size(&self) -> u32 {
        let raw_size = self.width.saturating_mul(self.height).saturating_mul(3) / 2;
        self.output_info.cbSize.max(raw_size).max(1024 * 1024)
    }
}

impl InputSample {
    fn new(capacity: usize) -> Result<Self> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(capacity as u32)
                .map_err(mf_error("MFCreateMemoryBuffer input"))?;
            let sample = MFCreateSample().map_err(mf_error("MFCreateSample input"))?;
            sample
                .AddBuffer(&buffer)
                .map_err(mf_error("input AddBuffer"))?;
            Ok(Self {
                sample,
                buffer,
                capacity,
            })
        }
    }

    fn fill_from_bgra(
        &mut self,
        bgra: &[u8],
        source_width: usize,
        source_height: usize,
        width: usize,
        height: usize,
        timestamp_us: u64,
        duration_hns: i64,
    ) -> Result<()> {
        let nv12_len = colorspace::nv12_len(width, height)?;
        if nv12_len > self.capacity {
            return Err(ScreenStreamError::InvalidFrame(format!(
                "input sample capacity {} is smaller than required {nv12_len}",
                self.capacity
            )));
        }

        unsafe {
            let mut data = null_mut();
            self.buffer
                .Lock(&mut data, None, None)
                .map_err(mf_error("input buffer Lock"))?;
            let fill_result = {
                let nv12 = std::slice::from_raw_parts_mut(data.cast::<u8>(), nv12_len);
                colorspace::fill_nv12_from_bgra(
                    nv12,
                    bgra,
                    source_width,
                    source_height,
                    width,
                    height,
                )
            };
            let unlock_result = self
                .buffer
                .Unlock()
                .map_err(mf_error("input buffer Unlock"));
            fill_result?;
            unlock_result?;
            self.buffer
                .SetCurrentLength(nv12_len as u32)
                .map_err(mf_error("input buffer SetCurrentLength"))?;
            self.sample
                .SetSampleTime((timestamp_us as i64).saturating_mul(10))
                .map_err(mf_error("SetSampleTime"))?;
            self.sample
                .SetSampleDuration(duration_hns)
                .map_err(mf_error("SetSampleDuration"))?;
        }

        Ok(())
    }
}

impl D3D11InputSample {
    fn new(converter: &D3D11VideoConverter) -> Result<Self> {
        let texture = create_nv12_texture(&converter.device, converter.width, converter.height)?;
        let output_view = converter.create_output_view(&texture)?;
        let buffer = unsafe {
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &texture, 0, false)
                .map_err(mf_error("MFCreateDXGISurfaceBuffer input"))?
        };
        let sample = unsafe { MFCreateSample().map_err(mf_error("MFCreateSample D3D11 input"))? };
        unsafe {
            sample
                .AddBuffer(&buffer)
                .map_err(mf_error("D3D11 input AddBuffer"))?;
        }
        Ok(Self {
            _texture: texture,
            output_view,
            sample,
        })
    }

    fn fill_from_texture(
        &mut self,
        converter: &D3D11VideoConverter,
        texture: &ID3D11Texture2D,
        timestamp_us: u64,
        duration_hns: i64,
    ) -> Result<()> {
        converter.convert(texture, &self.output_view)?;
        unsafe {
            self.sample
                .SetSampleTime((timestamp_us as i64).saturating_mul(10))
                .map_err(mf_error("D3D11 SetSampleTime"))?;
            self.sample
                .SetSampleDuration(duration_hns)
                .map_err(mf_error("D3D11 SetSampleDuration"))?;
        }
        Ok(())
    }
}

impl D3D11VideoConverter {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source_width: u32,
        source_height: u32,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        if source_width == 0 || source_height == 0 || width == 0 || height == 0 {
            return Err(ScreenStreamError::InvalidFrame(format!(
                "invalid D3D11 converter dimensions source={source_width}x{source_height} encoded={width}x{height}"
            )));
        }

        let video_device = device
            .cast::<ID3D11VideoDevice>()
            .map_err(mf_error("cast ID3D11VideoDevice"))?;
        let video_context = context
            .cast::<ID3D11VideoContext>()
            .map_err(mf_error("cast ID3D11VideoContext"))?;
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: source_width,
            InputHeight: source_height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe {
            video_device
                .CreateVideoProcessorEnumerator(&content_desc)
                .map_err(mf_error("CreateVideoProcessorEnumerator"))?
        };
        let processor = unsafe {
            video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(mf_error("CreateVideoProcessor"))?
        };

        unsafe {
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let source_rect = RECT {
                left: 0,
                top: 0,
                right: source_width as i32,
                bottom: source_height as i32,
            };
            let dest_rect = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            video_context.VideoProcessorSetStreamSourceRect(
                &processor,
                0,
                true,
                Some(&source_rect),
            );
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dest_rect));
        }

        Ok(Self {
            device: device.clone(),
            _context: context.clone(),
            video_device,
            video_context,
            enumerator,
            processor,
            _source_width: source_width,
            _source_height: source_height,
            width,
            height,
        })
    }

    fn convert(
        &self,
        source_texture: &ID3D11Texture2D,
        output_view: &ID3D11VideoProcessorOutputView,
    ) -> Result<()> {
        let input_view = self.create_input_view(source_texture)?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: null_mut(),
            pInputSurface: ManuallyDrop::new(Some(input_view)),
            ppFutureSurfaces: null_mut(),
            ppPastSurfacesRight: null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: null_mut(),
        };

        let result = unsafe {
            self.video_context
                .VideoProcessorBlt(
                    &self.processor,
                    output_view,
                    0,
                    std::slice::from_ref(&stream),
                )
                .map_err(mf_error("VideoProcessorBlt BGRA to NV12"))
        };

        unsafe {
            let _ = ManuallyDrop::take(&mut stream.pInputSurface);
            let _ = ManuallyDrop::take(&mut stream.pInputSurfaceRight);
        }
        result
    }

    fn create_input_view(
        &self,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorInputView> {
        let resource = texture
            .cast::<ID3D11Resource>()
            .map_err(mf_error("cast input texture ID3D11Resource"))?;
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut view = None;
        unsafe {
            self.video_device
                .CreateVideoProcessorInputView(&resource, &self.enumerator, &desc, Some(&mut view))
                .map_err(mf_error("CreateVideoProcessorInputView"))?;
        }
        view.ok_or_else(|| {
            ScreenStreamError::InvalidFrame("D3D11 video processor input view is null".into())
        })
    }

    fn create_output_view(
        &self,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorOutputView> {
        let resource = texture
            .cast::<ID3D11Resource>()
            .map_err(mf_error("cast output texture ID3D11Resource"))?;
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut view = None;
        unsafe {
            self.video_device
                .CreateVideoProcessorOutputView(&resource, &self.enumerator, &desc, Some(&mut view))
                .map_err(mf_error("CreateVideoProcessorOutputView"))?;
        }
        view.ok_or_else(|| {
            ScreenStreamError::InvalidFrame("D3D11 video processor output view is null".into())
        })
    }
}

fn enable_d3d11_multithread_protection(device: &ID3D11Device) -> Result<()> {
    // Media Foundation 硬编 MFT 可能在内部异步线程中访问同一个 D3D11 device。
    // DXGI 采集线程、VideoProcessor 转换和硬编 MFT 共享 device/context 时，
    // 必须打开 D3D11 多线程保护，否则部分驱动会偶发卡在 GPU/MFT 同步点。
    let multithread = device
        .cast::<ID3D11Multithread>()
        .map_err(mf_error("cast ID3D11Multithread"))?;
    unsafe {
        let _previous = multithread.SetMultithreadProtected(true);
    }
    Ok(())
}

fn create_nv12_texture(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width.max(2),
        Height: height.max(2),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET
            | D3D11_BIND_SHADER_RESOURCE
            | D3D11_BIND_VIDEO_ENCODER)
            .0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(mf_error("CreateTexture2D NV12 encoder input"))?;
    }
    texture.ok_or_else(|| ScreenStreamError::InvalidFrame("created NV12 texture is null".into()))
}

fn create_d3d11_device_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
    unsafe {
        let mut reset_token = 0_u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)
            .map_err(mf_error("MFCreateDXGIDeviceManager"))?;
        let manager = manager.ok_or_else(|| {
            ScreenStreamError::InvalidFrame("MFCreateDXGIDeviceManager returned null".into())
        })?;
        manager
            .ResetDevice(device, reset_token)
            .map_err(mf_error("IMFDXGIDeviceManager ResetDevice"))?;
        Ok(manager)
    }
}

fn create_hardware_transform() -> Result<IMFTransform> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
        let mut activates: *mut Option<IMFActivate> = null_mut();
        let mut count = 0_u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
        .map_err(mf_error("MFTEnumEx H.264 hardware encoder"))?;

        if activates.is_null() || count == 0 {
            if !activates.is_null() {
                CoTaskMemFree(Some(activates.cast()));
            }
            return Err(ScreenStreamError::InvalidFrame(
                "no Media Foundation hardware H.264 encoder found".into(),
            ));
        }

        let slice = std::slice::from_raw_parts_mut(activates, count as usize);
        let mut selected = None;
        let mut last_error = None;
        for activate_slot in slice.iter_mut() {
            let Some(activate) = activate_slot.take() else {
                continue;
            };
            if selected.is_some() {
                continue;
            }
            match activate.ActivateObject::<IMFTransform>() {
                Ok(transform) => selected = Some(transform),
                Err(err) => last_error = Some(err),
            }
        }
        CoTaskMemFree(Some(activates.cast()));

        selected.ok_or_else(|| {
            last_error
                .map(mf_error("ActivateObject H.264 hardware encoder"))
                .unwrap_or_else(|| {
                    ScreenStreamError::InvalidFrame(
                        "no activatable Media Foundation hardware H.264 encoder found".into(),
                    )
                })
        })
    }
}

fn configure_transform(
    transform: &IMFTransform,
    config: &H264EncoderConfig,
    width: u32,
    height: u32,
) -> Result<()> {
    unsafe {
        let output_type = MFCreateMediaType().map_err(mf_error("MFCreateMediaType output"))?;
        output_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(mf_error("output major type"))?;
        output_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(mf_error("output subtype"))?;
        output_type
            .SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps)
            .map_err(mf_error("output bitrate"))?;
        set_attr_size(&output_type, &MF_MT_FRAME_SIZE, width, height)?;
        set_attr_ratio(&output_type, &MF_MT_FRAME_RATE, config.max_fps.max(1), 1)?;
        output_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(mf_error("output interlace"))?;

        transform
            .SetOutputType(0, &output_type, 0)
            .map_err(mf_error("SetOutputType H.264"))?;

        let input_type = MFCreateMediaType().map_err(mf_error("MFCreateMediaType input"))?;
        input_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(mf_error("input major type"))?;
        input_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(mf_error("input subtype"))?;
        set_attr_size(&input_type, &MF_MT_FRAME_SIZE, width, height)?;
        set_attr_ratio(&input_type, &MF_MT_FRAME_RATE, config.max_fps.max(1), 1)?;
        input_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(mf_error("input interlace"))?;

        transform
            .SetInputType(0, &input_type, 0)
            .map_err(mf_error("SetInputType NV12"))?;
    }

    Ok(())
}

fn configure_transform_attributes(transform: &IMFTransform) {
    unsafe {
        if let Ok(attributes) = transform.GetAttributes() {
            let _ = attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
        }
    }
}

fn create_output_sample(size: u32) -> Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(size).map_err(mf_error("MFCreateMemoryBuffer output"))?;
        let sample = MFCreateSample().map_err(mf_error("MFCreateSample output"))?;
        sample
            .AddBuffer(&buffer)
            .map_err(mf_error("output AddBuffer"))?;
        Ok(sample)
    }
}

fn read_sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(mf_error("ConvertToContiguousBuffer"))?;
        let current_len = buffer
            .GetCurrentLength()
            .map_err(mf_error("output GetCurrentLength"))?;
        if current_len == 0 {
            return Ok(Vec::new());
        }

        let mut data = null_mut();
        buffer
            .Lock(&mut data, None, None)
            .map_err(mf_error("output buffer Lock"))?;
        let bytes = std::slice::from_raw_parts(data, current_len as usize).to_vec();
        buffer.Unlock().map_err(mf_error("output buffer Unlock"))?;
        Ok(bytes)
    }
}

fn set_attr_size(
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    key: &windows::core::GUID,
    width: u32,
    height: u32,
) -> Result<()> {
    unsafe {
        media_type
            .SetUINT64(key, ((width as u64) << 32) | height as u64)
            .map_err(mf_error("SetUINT64 size"))
    }
}

fn set_attr_ratio(
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    key: &windows::core::GUID,
    numerator: u32,
    denominator: u32,
) -> Result<()> {
    unsafe {
        media_type
            .SetUINT64(key, ((numerator as u64) << 32) | denominator as u64)
            .map_err(mf_error("SetUINT64 ratio"))
    }
}

fn drop_output_buffer(mut output: MFT_OUTPUT_DATA_BUFFER) {
    unsafe {
        let _ = ManuallyDrop::take(&mut output.pSample);
        let _ = ManuallyDrop::take(&mut output.pEvents);
    }
}

fn validate_config(config: &H264EncoderConfig) -> Result<()> {
    if config.max_fps == 0 || config.bitrate_bps == 0 {
        return Err(ScreenStreamError::InvalidFrame(
            "Media Foundation encoder requires positive fps and bitrate".into(),
        ));
    }
    Ok(())
}

struct MfRuntime {
    com_initialized: bool,
}

impl MfRuntime {
    fn new() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let com_initialized = if hr.is_ok() {
            true
        } else if hr == RPC_E_CHANGED_MODE {
            false
        } else {
            return Err(mf_hresult("CoInitializeEx", hr));
        };

        if let Err(err) = unsafe { MFStartup(MF_VERSION, 0) } {
            if com_initialized {
                unsafe { CoUninitialize() };
            }
            return Err(mf_error("MFStartup")(err));
        }
        Ok(Self { com_initialized })
    }
}

impl Drop for MfRuntime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            if self.com_initialized {
                CoUninitialize();
            }
        }
    }
}

fn mf_error(context: &'static str) -> impl FnOnce(WinError) -> ScreenStreamError + 'static {
    move |error| ScreenStreamError::Window(format!("{context}: {error}"))
}

fn mf_hresult(context: &'static str, hr: HRESULT) -> ScreenStreamError {
    ScreenStreamError::Window(format!("{context}: HRESULT 0x{:08X}", hr.0 as u32))
}

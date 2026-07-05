use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, QpRange,
    RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::formats::YUVSource;
use openh264::OpenH264API;

#[cfg(windows)]
#[path = "mf_h264.rs"]
mod mf_h264;
#[cfg(windows)]
pub use mf_h264::MediaFoundationH264Encoder;

#[cfg(windows)]
use crate::codec::RawD3D11Frame;
use crate::codec::{colorspace, DecodedVideoFrame, EncodedVideoFrame, RawBgraFrame};
use crate::config::{EncoderBackend, EncoderComplexity, H264EncoderConfig};
use crate::error::{Result, ScreenStreamError};
use crate::wire::EncodedPacket;

pub struct H264Encoder {
    inner: H264EncoderInner,
}

enum H264EncoderInner {
    OpenH264(SoftwareH264Encoder),
    #[cfg(windows)]
    MediaFoundation(MediaFoundationH264Encoder),
}

impl H264Encoder {
    pub fn new(config: H264EncoderConfig) -> Result<Self> {
        match config.backend {
            EncoderBackend::OpenH264 => {
                eprintln!("[screen_stream encoder] using OpenH264 software encoder");
                Ok(Self {
                    inner: H264EncoderInner::OpenH264(SoftwareH264Encoder::new(config)?),
                })
            }
            EncoderBackend::MediaFoundation => Self::new_media_foundation(config),
            EncoderBackend::Auto => Self::new_auto(config),
        }
    }

    pub fn request_keyframe(&mut self) {
        match &mut self.inner {
            H264EncoderInner::OpenH264(encoder) => encoder.request_keyframe(),
            #[cfg(windows)]
            H264EncoderInner::MediaFoundation(encoder) => encoder.request_keyframe(),
        }
    }

    #[cfg(windows)]
    pub fn can_encode_d3d11(&self) -> bool {
        matches!(self.inner, H264EncoderInner::MediaFoundation(_))
    }

    pub fn encode_bgra(
        &mut self,
        frame: RawBgraFrame<'_>,
        seq: u64,
        force_keyframe: bool,
    ) -> Result<Option<EncodedVideoFrame>> {
        match &mut self.inner {
            H264EncoderInner::OpenH264(encoder) => encoder.encode_bgra(frame, seq, force_keyframe),
            #[cfg(windows)]
            H264EncoderInner::MediaFoundation(encoder) => {
                encoder.encode_bgra(frame, seq, force_keyframe)
            }
        }
    }

    #[cfg(windows)]
    pub fn encode_d3d11(
        &mut self,
        frame: RawD3D11Frame<'_>,
        seq: u64,
        force_keyframe: bool,
    ) -> Result<Option<EncodedVideoFrame>> {
        match &mut self.inner {
            H264EncoderInner::OpenH264(_) => Err(ScreenStreamError::InvalidFrame(
                "OpenH264 backend does not accept D3D11 texture input".into(),
            )),
            H264EncoderInner::MediaFoundation(encoder) => {
                encoder.encode_d3d11(frame, seq, force_keyframe)
            }
        }
    }

    #[cfg(windows)]
    fn new_media_foundation(config: H264EncoderConfig) -> Result<Self> {
        eprintln!("[screen_stream encoder] using Media Foundation H.264");
        Ok(Self {
            inner: H264EncoderInner::MediaFoundation(MediaFoundationH264Encoder::new(config)?),
        })
    }

    #[cfg(not(windows))]
    fn new_media_foundation(_config: H264EncoderConfig) -> Result<Self> {
        Err(ScreenStreamError::InvalidFrame(
            "Media Foundation encoder is only available on Windows".into(),
        ))
    }

    fn new_auto(config: H264EncoderConfig) -> Result<Self> {
        #[cfg(windows)]
        {
            match MediaFoundationH264Encoder::new(config.clone()) {
                Ok(encoder) => {
                    eprintln!("[screen_stream encoder] using Media Foundation H.264 (auto)");
                    return Ok(Self {
                        inner: H264EncoderInner::MediaFoundation(encoder),
                    });
                }
                Err(err) => {
                    eprintln!(
                        "[screen_stream encoder] Media Foundation unavailable, falling back to OpenH264: {err}"
                    );
                }
            }
        }

        eprintln!("[screen_stream encoder] using OpenH264 software encoder");
        Ok(Self {
            inner: H264EncoderInner::OpenH264(SoftwareH264Encoder::new(config)?),
        })
    }
}

pub struct SoftwareH264Encoder {
    encoder: Encoder,
    config: H264EncoderConfig,
    i420: I420Frame,
}

impl SoftwareH264Encoder {
    pub fn new(config: H264EncoderConfig) -> Result<Self> {
        validate_encoder_config(&config)?;

        let api = OpenH264API::from_source();
        let encoder_config = EncoderConfig::new()
            .debug(config.openh264_debug)
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(config.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(config.max_fps.max(1) as f32))
            .intra_frame_period(IntraFramePeriod::from_num_frames(config.gop_frames))
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .skip_frames(config.allow_skip_frames)
            .num_threads(config.threads)
            .complexity(to_openh264_complexity(config.complexity))
            .qp(QpRange::new(config.qp_min, config.qp_max))
            // OpenH264 对屏幕内容会自动关闭这两个开关。这里显式设置，
            // 是为了避免运行时输出无意义的 warning。
            .adaptive_quantization(false)
            .background_detection(false);

        Ok(Self {
            encoder: Encoder::with_api_config(api, encoder_config)?,
            config,
            i420: I420Frame::default(),
        })
    }

    pub fn request_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    pub fn encode_bgra(
        &mut self,
        frame: RawBgraFrame<'_>,
        seq: u64,
        force_keyframe: bool,
    ) -> Result<Option<EncodedVideoFrame>> {
        let (width, height) = self.config.encoded_dimensions(frame.width, frame.height)?;
        let width = width as usize;
        let height = height as usize;
        let source_width = frame.width as usize;
        let source_height = frame.height as usize;

        // DXGI 输出 BGRA。软件编码路径直接转成 I420 喂给 OpenH264，
        // 避免之前 BGRA -> RGB -> I420 的多余转换链路。
        self.i420
            .fill_from_bgra(frame.data, source_width, source_height, width, height)?;

        if force_keyframe {
            self.request_keyframe();
        }

        let bitstream = self.encoder.encode(&self.i420)?;
        let frame_type = bitstream.frame_type();
        let payload = bitstream.to_vec();

        if payload.is_empty() || frame_type == FrameType::Skip {
            return Ok(None);
        }

        Ok(Some(EncodedVideoFrame {
            seq,
            timestamp_us: frame.timestamp_us,
            width: width as u32,
            height: height as u32,
            is_keyframe: matches!(frame_type, FrameType::IDR | FrameType::I),
            payload,
        }))
    }
}

#[derive(Default)]
struct I420Frame {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420Frame {
    fn fill_from_bgra(
        &mut self,
        bgra: &[u8],
        source_width: usize,
        source_height: usize,
        width: usize,
        height: usize,
    ) -> Result<()> {
        self.resize(width, height)?;
        colorspace::fill_i420_from_bgra(
            &mut self.y,
            &mut self.u,
            &mut self.v,
            bgra,
            source_width,
            source_height,
            width,
            height,
        )
    }

    fn resize(&mut self, width: usize, height: usize) -> Result<()> {
        let (y_len, uv_len) = colorspace::i420_plane_lengths(width, height)?;

        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
        }

        if self.y.len() != y_len {
            self.y.resize(y_len, 0);
        }
        if self.u.len() != uv_len {
            self.u.resize(uv_len, 128);
        }
        if self.v.len() != uv_len {
            self.v.resize(uv_len, 128);
        }

        Ok(())
    }
}

impl YUVSource for I420Frame {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

pub struct SoftwareH264Decoder {
    decoder: openh264::decoder::Decoder,
}

impl SoftwareH264Decoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            decoder: openh264::decoder::Decoder::new()?,
        })
    }

    pub fn decode_packet(&mut self, packet: &EncodedPacket) -> Result<Option<DecodedVideoFrame>> {
        let Some(yuv) = self.decoder.decode(&packet.payload)? else {
            return Ok(None);
        };

        let (width, height) = yuv.dimensions();
        let mut rgba = vec![0_u8; yuv.rgba8_len()];
        yuv.write_rgba8(&mut rgba);

        Ok(Some(DecodedVideoFrame {
            seq: packet.seq,
            timestamp_us: packet.timestamp_us,
            width: width as u32,
            height: height as u32,
            is_keyframe: packet.is_keyframe,
            rgba,
        }))
    }
}

impl From<EncodedVideoFrame> for EncodedPacket {
    fn from(value: EncodedVideoFrame) -> Self {
        Self {
            seq: value.seq,
            timestamp_us: value.timestamp_us,
            width: value.width,
            height: value.height,
            is_keyframe: value.is_keyframe,
            payload: value.payload,
        }
    }
}

fn validate_encoder_config(config: &H264EncoderConfig) -> Result<()> {
    if config.qp_max > 51 || config.qp_min > config.qp_max {
        return Err(ScreenStreamError::InvalidFrame(format!(
            "invalid qp range {}..={}; expected 0 <= min <= max <= 51",
            config.qp_min, config.qp_max
        )));
    }
    Ok(())
}

fn to_openh264_complexity(value: EncoderComplexity) -> Complexity {
    match value {
        EncoderComplexity::Low => Complexity::Low,
        EncoderComplexity::Medium => Complexity::Medium,
        EncoderComplexity::High => Complexity::High,
    }
}

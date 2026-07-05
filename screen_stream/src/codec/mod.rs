pub(crate) mod colorspace;
pub mod h264;

#[derive(Debug, Clone, Copy)]
pub struct RawBgraFrame<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub timestamp_us: u64,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
pub struct RawD3D11Frame<'a> {
    pub texture: &'a windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    pub device: &'a windows::Win32::Graphics::Direct3D11::ID3D11Device,
    pub context: &'a windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    pub width: u32,
    pub height: u32,
    pub timestamp_us: u64,
}

#[derive(Debug, Clone)]
pub struct EncodedVideoFrame {
    pub seq: u64,
    pub timestamp_us: u64,
    pub width: u32,
    pub height: u32,
    pub is_keyframe: bool,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    pub seq: u64,
    pub timestamp_us: u64,
    pub width: u32,
    pub height: u32,
    pub is_keyframe: bool,
    pub rgba: Vec<u8>,
}

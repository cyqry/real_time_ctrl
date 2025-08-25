use anyhow::anyhow;
use bytes::Buf;
use common::file_util;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ImageBuffer, ImageEncoder, ImageOutputFormat, Rgb, RgbImage, Rgba};
use winapi::ctypes::c_int;
use winapi::shared::windef::{HBITMAP, HDC};
use winapi::um::wingdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits,
    GetObjectW, SelectObject, BITMAP, BITMAPINFO, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
};
use winapi::um::winuser::{
    GetDC, GetDesktopWindow, GetSystemMetrics, GetWindowDC, ReleaseDC, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

pub async fn cut_screen() -> anyhow::Result<Vec<u8>> {
    unsafe {
        // 获取桌面窗口的设备上下文
        let desktop = GetDesktopWindow();
        let hdc = GetWindowDC(desktop);

        if hdc.is_null() {
            return Err(anyhow!("Failed to get desktop HDC."));
        }

        // winapi 获取屏幕的宽度和高度,但不知为什么总不能获取全
        // let screen_width = GetSystemMetrics(winapi::um::winuser::SM_CXSCREEN);
        // let screen_height = GetSystemMetrics(winapi::um::winuser::SM_CYSCREEN);

        //使用库获取，保证全
        let (screen_width, screen_height) = get_xy()? ;
        let screen_width=screen_width as c_int;
        let screen_height=screen_height as c_int;

        // 创建一个与桌面设备上下文兼容的内存设备上下文和位图
        let mem_dc = CreateCompatibleDC(hdc);
        let bitmap = CreateCompatibleBitmap(hdc, screen_width, screen_height);
        let old_bitmap = SelectObject(mem_dc, bitmap as *mut _);

        // 从桌面设备上下文到内存设备上下文中复制像素
        BitBlt(
            mem_dc,
            0,
            0,
            screen_width,
            screen_height,
            hdc,
            0,
            0,
            SRCCOPY,
        );

        let pixels = get_pixels_from_hbitmap(hdc, bitmap, screen_width, screen_height)?;

        let png = pixels_2_png(pixels, screen_width, screen_height)?;
        // 清理
        SelectObject(mem_dc, old_bitmap);
        DeleteObject(bitmap as *mut _);
        DeleteDC(mem_dc);
        winapi::um::winuser::ReleaseDC(desktop, hdc);
        Ok(png)
    }
}

fn pixels_2_png(
    pixels: Vec<u8>,
    screen_width: c_int,
    screen_height: c_int,
) -> anyhow::Result<Vec<u8>> {
    println!("{}", pixels.len());
    // 转换像素数据为图像缓冲区
    let img: RgbImage = ImageBuffer::from_fn(screen_width as u32, screen_height as u32, |x, y| {
        let base = ((screen_height as u32 - 1 - y) * screen_width as u32 + x) * 3;
        let r = pixels[(base + 2) as usize];
        let g = pixels[(base + 1) as usize];
        let b = pixels[base as usize];
        Rgb([r, g, b])
    });

    // 将图像缓冲区保存为PNG格式的Vec<u8>
    let mut png_data = Vec::new();
    // 方式一，简单
    // {
    //     //通过标准库Cursor构造一个writer,从缓冲区已png格式写入
    //     let mut writer = Cursor::new(&mut png_data);
    //     img.write_to(&mut writer, ImageOutputFormat::Png).unwrap();
    // }

    //方式二，可控制png格式的一些参数
    {
        let encoder = PngEncoder::new_with_quality(
            &mut png_data,
            CompressionType::Best,
            FilterType::Adaptive,
        );
        encoder.write_image(&img, img.width(), img.height(), image::ColorType::Rgb8)?;
    }
    Ok(png_data)
}

fn get_xy() -> anyhow::Result<(usize, usize)> {
    use scrap::{Capturer, Display};
    let display = Display::primary()?;
    let mut capturer = Capturer::new(display)?;
    Ok((capturer.width(), capturer.height()))
}

fn get_pixels_from_hbitmap(
    hdc: HDC,
    bitmap: HBITMAP,
    screen_width: c_int,
    screen_height: c_int,
) -> anyhow::Result<Vec<u8>> {
    unsafe {
        let mut bmp_info = BITMAPINFO {
            bmiHeader: {
                let mut header = std::mem::zeroed::<winapi::um::wingdi::BITMAPINFOHEADER>();
                header.biSize = std::mem::size_of::<winapi::um::wingdi::BITMAPINFOHEADER>() as u32;
                header.biWidth = screen_width;
                header.biHeight = screen_height;
                header.biPlanes = 1;
                header.biBitCount = 24; // Assuming 24 bits per pixel (RGB)
                header.biCompression = BI_RGB;
                header
            },
            bmiColors: [std::mem::zeroed(); 1],
        };

        let pixel_data_size = (screen_width * screen_height * 3) as usize; // 3 bytes per pixel for RGB
        let mut pixels: Vec<u8> = vec![0; pixel_data_size];

        GetDIBits(
            hdc,
            bitmap,
            0,
            screen_height as u32,
            pixels.as_mut_ptr() as *mut _,
            &mut bmp_info as *mut _,
            DIB_RGB_COLORS,
        );
        if pixels.is_empty() {
            return Err(anyhow!("读取位图失败"));
        }
        Ok(pixels)
    }
}

#[tokio::test]
pub async fn test() {

    // println!(
    //     "{:?}",
    //     file_util::save_file(
    //         r"C:\Users\lenovo\Desktop\Imag\1.png",
    //         &cut_screen().await.unwrap()
    //     )
    //     .await
    //     .unwrap()
    // );
    // let display = Display::primary().expect("无法获取主屏幕");
    // let mut capturer = Capturer::new(display).expect("无法抓取屏幕");
    // let (w, h) = (capturer.width(), capturer.height());
    // println!("{},{}", w,h);
    let x = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_CYSCREEN) };


    println!("{}", x);
}

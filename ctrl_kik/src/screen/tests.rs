use super::{cut_screen, cut_screen_dxgi, PngProfile, PrimaryScreenCapturer};
use anyhow::anyhow;

#[tokio::test]
#[ignore = "依赖当前 Windows 交互桌面，手工运行用于比较两种截屏实现"]
async fn compare_capture_backends() -> anyhow::Result<()> {
    use std::time::Instant;

    let output_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| anyhow!("无法定位工作区根目录"))?
        .join("target")
        .join("screen");
    std::fs::create_dir_all(&output_dir)?;

    let legacy_started = Instant::now();
    let legacy_png = cut_screen().await?;
    let legacy_elapsed = legacy_started.elapsed();
    std::fs::write(output_dir.join("legacy_gdi.png"), &legacy_png)?;
    println!(
        "legacy GDI -> {} bytes, {:?}",
        legacy_png.len(),
        legacy_elapsed
    );

    let mut capturer = PrimaryScreenCapturer::new()?;
    let mut first_png = Vec::new();
    let dxgi_started = Instant::now();
    capturer.capture_png_into(PngProfile::Balanced, &mut first_png)?;
    let dxgi_elapsed = dxgi_started.elapsed();
    if capturer.last_frame_is_uniform_black() {
        return Err(anyhow!("DXGI 首帧重取后仍为全黑，未通过现场验收"));
    }

    let mut profile_results = Vec::new();
    for (name, profile) in [
        ("fast", PngProfile::Fast),
        ("balanced", PngProfile::Balanced),
        ("high", PngProfile::High),
    ] {
        let mut png = Vec::new();
        let started = Instant::now();
        capturer.encode_cached_png_into(profile, &mut png)?;
        let elapsed = started.elapsed();
        std::fs::write(output_dir.join(format!("dxgi_{name}.png")), &png)?;
        profile_results.push((name, png, elapsed));
    }

    let reference_pixels = image::load_from_memory(&first_png)?.to_rgb8();
    let mut profile_report = serde_json::Map::new();
    for (name, png, elapsed) in &profile_results {
        let decoded = image::load_from_memory(png)?.to_rgb8();
        if decoded.as_raw() != reference_pixels.as_raw() {
            return Err(anyhow!("PNG {name} 档位解码后像素与原始帧不一致"));
        }
        profile_report.insert(
            (*name).to_string(),
            serde_json::json!({
                "png_bytes": png.len(),
                "encode_ms": elapsed.as_secs_f64() * 1000.0
            }),
        );
    }

    let dxgi_png = profile_results
        .iter()
        .find(|(name, _, _)| *name == "balanced")
        .map(|(_, png, _)| png)
        .ok_or_else(|| anyhow!("缺少 Balanced 压缩结果"))?;

    let legacy_image = image::load_from_memory(&legacy_png)?.to_rgb8();
    let dxgi_image = image::load_from_memory(dxgi_png)?.to_rgb8();
    let legacy_non_black = legacy_image
        .pixels()
        .filter(|pixel| pixel.0.iter().any(|channel| *channel > 2))
        .count();
    let dxgi_non_black = dxgi_image
        .pixels()
        .filter(|pixel| pixel.0.iter().any(|channel| *channel > 2))
        .count();

    drop(capturer);
    let worker_first_started = Instant::now();
    let worker_first = cut_screen_dxgi(PngProfile::Fast).await?;
    let worker_first_elapsed = worker_first_started.elapsed();
    let worker_steady_started = Instant::now();
    let worker_steady = cut_screen_dxgi(PngProfile::Fast).await?;
    let worker_steady_elapsed = worker_steady_started.elapsed();
    let worker_image = image::load_from_memory(&worker_steady)?.to_rgb8();
    if worker_image
        .pixels()
        .all(|pixel| pixel.0.iter().all(|channel| *channel <= 2))
    {
        return Err(anyhow!("持久 DXGI 工作线程返回了全黑帧"));
    }
    std::fs::write(output_dir.join("dxgi_worker_fast.png"), &worker_steady)?;

    let report = serde_json::json!({
        "legacy_gdi": {
            "width": legacy_image.width(),
            "height": legacy_image.height(),
            "png_bytes": legacy_png.len(),
            "elapsed_ms": legacy_elapsed.as_secs_f64() * 1000.0,
            "non_black_pixels": legacy_non_black
        },
        "dxgi_balanced": {
            "width": dxgi_image.width(),
            "height": dxgi_image.height(),
            "png_bytes": dxgi_png.len(),
            "first_capture_and_encode_ms": dxgi_elapsed.as_secs_f64() * 1000.0,
            "non_black_pixels": dxgi_non_black
        },
        "dxgi_worker_fast": {
            "first_request_ms": worker_first_elapsed.as_secs_f64() * 1000.0,
            "steady_request_ms": worker_steady_elapsed.as_secs_f64() * 1000.0,
            "first_png_bytes": worker_first.len(),
            "steady_png_bytes": worker_steady.len()
        },
        "png_profiles_same_pixels": profile_report
    });
    std::fs::write(
        output_dir.join("comparison.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);

    Ok(())
}

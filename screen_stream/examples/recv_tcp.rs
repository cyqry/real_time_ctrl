use screen_stream::{play_from, PlayerConfig, Result};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:7007".to_string());
    let mut config = PlayerConfig::default();
    for arg in args {
        match arg.as_str() {
            "--stats" | "stats" | "--stat" | "stat" => config.debug_stats.enabled = true,
            "--no-stats" | "no-stats" => config.debug_stats.enabled = false,
            _ => {}
        }
    }

    let listener = TcpListener::bind(addr).await?;
    let (stream, peer) = listener.accept().await?;
    stream.set_nodelay(true)?;
    println!(
        "screen stream connected from {peer}; stats={}",
        config.debug_stats.enabled
    );

    let mut frames = 0_u64;
    play_from(stream, config, |frame| {
        frames += 1;
        if frames == 1 || frames % 30 == 0 {
            println!(
                "decoded frame #{frames}: seq={}, key={}, {}x{}, {} rgba bytes, ts={}us",
                frame.seq,
                frame.is_keyframe,
                frame.width,
                frame.height,
                frame.rgba.len(),
                frame.timestamp_us
            );
        }
        Ok(())
    })
    .await
}

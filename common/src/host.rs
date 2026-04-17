
use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use crate::generated::encrypted_strings::HOST;

// 全局缓存结构
struct DnsCache {
    store: Mutex<HashMap<String, (Vec<IpAddr>, std::time::Instant)>>,
    ttl_seconds: u64,
}

impl DnsCache {
    fn new(ttl_seconds: u64) -> Self {
        Self {
            store: Mutex::new(HashMap::new()),
            ttl_seconds,
        }
    }

    fn get(&self, domain: &str) -> Option<Vec<IpAddr>> {
        let guard = self.store.lock().ok()?;
        guard.get(domain).and_then(|(ips, cached_at)| {
            if cached_at.elapsed().as_secs() < self.ttl_seconds {
                Some(ips.clone())
            } else {
                None // 缓存过期
            }
        })
    }

    fn set(&self, domain: String, ips: Vec<IpAddr>) {
        if let Ok(mut guard) = self.store.lock() {
            guard.insert(domain, (ips, std::time::Instant::now()));
        }
    }
}

// 全局DNS缓存实例 (使用标准库 OnceLock)
static DNS_CACHE: OnceLock<DnsCache> = OnceLock::new();


pub fn get_host() -> String {
    let host = HOST();
    if host == "localhost" || host == "127.0.0.1" {
        return host;
    }
    resolve_domain(host.as_str())
        .unwrap_or(vec![])
        .get(0)
        .map(|ip| ip.to_string())
        .unwrap_or(host)
}


// 带缓存的域名解析主函数
pub fn resolve_domain(domain: &str) -> anyhow::Result<Vec<IpAddr>> {
    //检查缓存
    let cache = get_cache();
    if let Some(cached_ips) = cache.get(domain) {
        // println!("[缓存命中] {} (TTL内)", domain);
        return Ok(cached_ips);
    }

    // 提取IP地址
    let ips: Vec<IpAddr> = (domain, 0).to_socket_addrs()?.map(|a| a.ip()).collect();

    // 更新缓存
    cache.set(domain.to_string(), ips.clone());
    Ok(ips)
}


// 获取全局缓存实例
fn get_cache() -> &'static DnsCache {
    DNS_CACHE.get_or_init(|| DnsCache::new(300)) // 默认5分钟TTL
}



#[tokio::test]
async fn test() -> Result<(), Box<dyn std::error::Error>> {
    // 测试解析几个域名
    let test_domains = ["google.com", "github.com", "rust-lang.org", "example.com"];

    for domain in test_domains {
        match resolve_domain(domain) {
            Ok(ips) => {
                println!("{} 解析结果: {}个IP", domain, ips.len());
                for (i, ip) in ips.iter().enumerate() {
                    println!("  {}. {}", i + 1, ip);
                }
            }
            Err(e) => println!("解析 {} 失败: {}", domain, e),
        }
        println!("---");
    }

    // 测试缓存效果
    println!("\n=== 测试缓存 ===");
    for _ in 0..3 {
        let start = std::time::Instant::now();
        let result = resolve_domain("google.com");
        let duration = start.elapsed();

        match result {
            Ok(ips) => println!(
                "google.com -> {:?} (耗时: {:?})",
                ips.get(0).unwrap_or(&"127.0.0.1".parse()?),
                duration
            ),
            Err(e) => println!("错误: {} (耗时: {:?})", e, duration),
        }
    }

    Ok(())
}

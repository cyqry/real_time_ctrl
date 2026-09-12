use crate::generated::encrypted_strings::HOST;
use std::collections::HashMap;
use std::env;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};

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
    resolve_configured_host(HOST())
}

pub fn get_host_from_env_or(env_name: &str, default: &str) -> String {
    // 仅 real_ctrl 这类控制端允许通过环境变量切换服务端地址，ctrl_kik 必须继续使用编译期配置。
    let host = env::var(env_name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string());
    resolve_configured_host(host)
}

fn resolve_configured_host(host: String) -> String {
    resolve_domain(host.as_str())
        .unwrap_or_default()
        .first()
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
    let first = resolve_domain("localhost")?;
    let second = resolve_domain("localhost")?;
    assert!(!first.is_empty());
    assert_eq!(first, second);
    Ok(())
}

use crate::config::{Config, SecurityConfig};
use crate::hidden;
use anyhow::{anyhow, Context};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// 服务端监听层只依赖该公开别名，不需要重复声明 tokio-rustls 依赖版本。
pub type ServerTlsAcceptor = TlsAcceptor;
/// pinned TLS 客户端先发送的非秘密协议前导，随后立即进入标准 TLS 1.3 握手。
///
/// 部分公网中间设备会复位非标准端口上“首包即 TLS”的连接，因此生产独立 TLS
/// 端口也保留此前导。它只用于穿透与协议识别，不参与认证、授权或降级协商；
/// 服务端身份仍由证书链、DNS 名称和 SPKI pin 共同验证。
pub const CTRL_TLS_PREFIX: [u8; 5] = [0x52, 0x54, 0x43, 0x54, 0x03];
use x509_parser::prelude::{FromDer, X509Certificate};

pub type BoxedAsyncRead = Pin<Box<dyn AsyncRead + Send>>;
pub type BoxedAsyncWrite = Pin<Box<dyn AsyncWrite + Send>>;

pub struct TransportParts {
    pub reader: BoxedAsyncRead,
    pub writer: BoxedAsyncWrite,
    pub local_addr: io::Result<SocketAddr>,
    pub peer_addr: io::Result<SocketAddr>,
}

pub async fn connect_real_ctrl(config: &Config) -> anyhow::Result<TransportParts> {
    connect_pinned_tls(config).await
}

pub fn build_server_tls_acceptor(security: &SecurityConfig) -> anyhow::Result<TlsAcceptor> {
    let certs = match (
        security.server_cert_path.as_deref(),
        security.server_cert_pem.as_deref(),
    ) {
        (Some(path), _) => load_certs(path)?,
        (None, Some(pem)) => load_certs_from_pem(pem.as_bytes())?,
        (None, None) => return Err(anyhow!(hidden!("必须配置 CTRL_SERVER_TLS_CERT"))),
    };
    let key = match (
        security.server_key_path.as_deref(),
        security.server_key_pem.as_deref(),
    ) {
        (Some(path), _) => load_private_key(path)?,
        (None, Some(pem)) => load_private_key_from_pem(pem.as_bytes())?,
        (None, None) => return Err(anyhow!(hidden!("必须配置 CTRL_SERVER_TLS_KEY"))),
    };
    let server_config = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context(hidden!("加载服务端 TLS 证书失败"))?;

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

pub async fn accept_tls(
    acceptor: TlsAcceptor,
    stream: TcpStream,
    handshake_timeout: std::time::Duration,
) -> anyhow::Result<TransportParts> {
    let local_addr = stream.local_addr();
    let peer_addr = stream.peer_addr();
    stream.set_nodelay(true)?;
    let tls_stream = timeout(handshake_timeout, acceptor.accept(stream))
        .await
        .context(hidden!("TLS 握手超时"))??;
    let (reader, writer) = tokio::io::split(tls_stream);

    Ok(TransportParts {
        reader: Box::pin(reader),
        writer: Box::pin(writer),
        local_addr,
        peer_addr,
    })
}

pub async fn accept_prefixed_tls(
    acceptor: TlsAcceptor,
    mut stream: TcpStream,
    handshake_timeout: std::time::Duration,
) -> anyhow::Result<TransportParts> {
    let mut prefix = [0_u8; CTRL_TLS_PREFIX.len()];
    timeout(handshake_timeout, stream.read_exact(&mut prefix))
        .await
        .context(hidden!("TLS 协议前导读取超时"))??;
    if prefix != CTRL_TLS_PREFIX {
        return Err(anyhow!(hidden!("TLS 协议前导无效")));
    }
    accept_tls(acceptor, stream, handshake_timeout).await
}

pub fn normalize_sha256_pin(pin: &str) -> String {
    pin.trim()
        .trim_start_matches(&hidden!("sha256:"))
        .trim_start_matches(&hidden!("SHA256:"))
        .replace(':', "")
        .to_ascii_lowercase()
}

pub fn certificate_spki_sha256_hex(cert_der: &[u8]) -> anyhow::Result<String> {
    let (_, cert) = X509Certificate::from_der(cert_der)
        .map_err(|e| anyhow!(hidden!("解析服务端证书失败: ", e)))?;
    let spki_der = cert.tbs_certificate.subject_pki.raw;
    let digest = Sha256::digest(spki_der);
    Ok(hex::encode(digest))
}

pub fn spki_sha256_hex_from_pem_file(path: &str) -> anyhow::Result<String> {
    let cert = load_certs(path)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!(hidden!("证书文件中没有 PEM 证书: ", path)))?;
    certificate_spki_sha256_hex(cert.as_ref())
}

async fn connect_pinned_tls(config: &Config) -> anyhow::Result<TransportParts> {
    let expected_pin = config
        .security
        .pinned_spki_sha256
        .as_deref()
        .ok_or_else(|| {
            anyhow!(hidden!(
                "REAL_CTRL_TLS_SERVER_SPKI_SHA256 未配置，拒绝启动强安全连接"
            ))
        })?;
    validate_sha256_pin(expected_pin)?;
    let endpoint = hidden!(&config.server_host, ":", &config.security.tls_port);
    let mut stream = timeout(config.read_timeout, TcpStream::connect(&endpoint))
        .await
        .with_context(|| hidden!("连接 TLS 管理端口超时: ", &endpoint))??;
    stream.set_nodelay(true)?;
    // 部分公网链路设备会复位非标准端口上“首包即 TLS”的连接。固定非秘密前导只用于
    // 穿透与协议识别；随后仍执行完整 TLS 1.3、证书链、服务名和 SPKI pin 校验。
    // 服务端同时接受标准 ClientHello，便于运维工具直接探测独立 TLS 端口。
    stream.write_all(&CTRL_TLS_PREFIX).await?;
    stream.flush().await?;
    let local_addr = stream.local_addr();
    let peer_addr = stream.peer_addr();

    let mut roots = RootCertStore::empty();
    let ca_certs = match (
        config.security.ca_cert_path.as_deref(),
        config.security.ca_cert_pem.as_deref(),
    ) {
        (Some(path), _) => load_certs(path)?,
        (None, Some(pem)) => load_certs_from_pem(pem.as_bytes())?,
        (None, None) => {
            return Err(anyhow!(hidden!(
                "REAL_CTRL_TLS_CA_CERT 未配置，拒绝启动强安全连接"
            )))
        }
    };
    for cert in ca_certs {
        roots
            .add(cert)
            .map_err(|e| anyhow!(hidden!("加载 TLS CA/服务端证书失败: ", e)))?;
    }

    let client_config = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));
    let server_name = ServerName::try_from(config.security.tls_server_name.clone())
        .map_err(|_| anyhow!(hidden!("REAL_CTRL_TLS_SERVER_NAME 不是合法 DNS 名称")))?;
    let tls_stream = timeout(config.read_timeout, connector.connect(server_name, stream))
        .await
        .context(hidden!("TLS 握手超时"))??;

    let (_, session) = tls_stream.get_ref();
    let peer_certs = session
        .peer_certificates()
        .ok_or_else(|| anyhow!(hidden!("TLS 握手完成但服务端未提供证书")))?;
    let end_entity = peer_certs
        .first()
        .ok_or_else(|| anyhow!(hidden!("TLS 握手完成但服务端证书链为空")))?;
    verify_spki_pin(end_entity, expected_pin)?;

    let (reader, writer) = tokio::io::split(tls_stream);
    Ok(TransportParts {
        reader: Box::pin(reader),
        writer: Box::pin(writer),
        local_addr,
        peer_addr,
    })
}

fn verify_spki_pin(cert: &CertificateDer<'_>, expected_pin: &str) -> anyhow::Result<()> {
    let actual = certificate_spki_sha256_hex(cert.as_ref())?;
    let expected = normalize_sha256_pin(expected_pin);
    if actual != expected {
        return Err(anyhow!(hidden!(
            "服务端 SPKI pin 不匹配，expected=",
            expected,
            ", actual=",
            actual
        )));
    }

    Ok(())
}

fn validate_sha256_pin(pin: &str) -> anyhow::Result<()> {
    let normalized = normalize_sha256_pin(pin);
    if normalized.len() != 64 || !normalized.bytes().all(|value| value.is_ascii_hexdigit()) {
        return Err(anyhow!(hidden!(
            "REAL_CTRL_TLS_SERVER_SPKI_SHA256 必须是 64 位十六进制 SHA-256"
        )));
    }
    Ok(())
}

fn load_certs(path: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path).with_context(|| hidden!("打开证书文件失败: ", path))?;
    // rustls-pki-types 是 rustls 官方承接的 PEM 解析入口，避免继续依赖已停止维护的
    // rustls-pemfile 包；迭代解析也允许证书链文件包含多张证书。
    let certs = CertificateDer::pem_reader_iter(file)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| hidden!("读取 PEM 证书失败: ", path))?;
    if certs.is_empty() {
        return Err(anyhow!(hidden!("证书文件中没有 PEM 证书: ", path)));
    }
    Ok(certs)
}

fn load_certs_from_pem(pem: &[u8]) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .context(hidden!("读取内置 PEM 证书失败"))?;
    if certs.is_empty() {
        return Err(anyhow!(hidden!("内置 PEM 中没有证书")));
    }
    Ok(certs)
}

fn load_private_key(path: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    let file = File::open(path).with_context(|| hidden!("打开私钥文件失败: ", path))?;
    PrivateKeyDer::from_pem_reader(file)
        .with_context(|| hidden!("私钥文件中没有可用 PEM 私钥: ", path))
}

fn load_private_key_from_pem(pem: &[u8]) -> anyhow::Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(pem).context(hidden!("内置 PEM 中没有可用私钥"))
}

#[cfg(test)]
mod tests {
    use super::normalize_sha256_pin;

    #[test]
    fn normalize_pin_accepts_common_formats() {
        assert_eq!(
            normalize_sha256_pin("SHA256:AA:bb:00"),
            "aabb00".to_string()
        );
        assert_eq!(
            normalize_sha256_pin(" sha256:aabb00 "),
            "aabb00".to_string()
        );
    }
}

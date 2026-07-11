use crate::config::{ClientTransportMode, Config, SecurityConfig};
use anyhow::{anyhow, Context};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, BufReader};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, TlsConnector};
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
    match config.security.client_mode {
        ClientTransportMode::Plain => {
            let endpoint = format!("{}:{}", config.server_host, config.server_port);
            let stream = timeout(config.read_timeout, TcpStream::connect(&endpoint))
                .await
                .with_context(|| format!("连接明文兼容端口超时: {endpoint}"))??;
            stream.set_nodelay(true)?;
            Ok(split_stream(stream))
        }
        ClientTransportMode::PinnedTls => connect_pinned_tls(config).await,
    }
}

pub fn build_server_tls_acceptor(security: &SecurityConfig) -> anyhow::Result<Option<TlsAcceptor>> {
    if !security.server_tls_enabled() {
        return Ok(None);
    }

    let cert_path = security
        .server_cert_path
        .as_deref()
        .ok_or_else(|| anyhow!("启用服务端 TLS 时必须配置 CTRL_SERVER_TLS_CERT"))?;
    let key_path = security
        .server_key_path
        .as_deref()
        .ok_or_else(|| anyhow!("启用服务端 TLS 时必须配置 CTRL_SERVER_TLS_KEY"))?;

    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    let server_config = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("加载服务端 TLS 证书失败")?;

    Ok(Some(TlsAcceptor::from(Arc::new(server_config))))
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
        .context("TLS 握手超时")??;
    let (reader, writer) = tokio::io::split(tls_stream);

    Ok(TransportParts {
        reader: Box::pin(reader),
        writer: Box::pin(writer),
        local_addr,
        peer_addr,
    })
}

pub fn split_stream(stream: TcpStream) -> TransportParts {
    let local_addr = stream.local_addr();
    let peer_addr = stream.peer_addr();
    let (reader, writer) = tokio::io::split(stream);

    TransportParts {
        reader: Box::pin(reader),
        writer: Box::pin(writer),
        local_addr,
        peer_addr,
    }
}

pub fn normalize_sha256_pin(pin: &str) -> String {
    pin.trim()
        .trim_start_matches("sha256:")
        .trim_start_matches("SHA256:")
        .replace(':', "")
        .to_ascii_lowercase()
}

pub fn certificate_spki_sha256_hex(cert_der: &[u8]) -> anyhow::Result<String> {
    let (_, cert) =
        X509Certificate::from_der(cert_der).map_err(|e| anyhow!("解析服务端证书失败: {e}"))?;
    let spki_der = cert.tbs_certificate.subject_pki.raw;
    let digest = Sha256::digest(spki_der);
    Ok(hex::encode(digest))
}

pub fn spki_sha256_hex_from_pem_file(path: &str) -> anyhow::Result<String> {
    let cert = load_certs(path)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("证书文件中没有 PEM 证书: {path}"))?;
    certificate_spki_sha256_hex(cert.as_ref())
}

async fn connect_pinned_tls(config: &Config) -> anyhow::Result<TransportParts> {
    let expected_pin = config
        .security
        .pinned_spki_sha256
        .as_deref()
        .ok_or_else(|| anyhow!("REAL_CTRL_TLS_SERVER_SPKI_SHA256 未配置，拒绝启动强安全连接"))?;
    validate_sha256_pin(expected_pin)?;
    let ca_cert_path = config
        .security
        .ca_cert_path
        .as_deref()
        .ok_or_else(|| anyhow!("REAL_CTRL_TLS_CA_CERT 未配置，拒绝启动强安全连接"))?;

    let endpoint = format!("{}:{}", config.server_host, config.security.tls_port);
    let stream = timeout(config.read_timeout, TcpStream::connect(&endpoint))
        .await
        .with_context(|| format!("连接 TLS 管理端口超时: {endpoint}"))??;
    stream.set_nodelay(true)?;
    let local_addr = stream.local_addr();
    let peer_addr = stream.peer_addr();

    let mut roots = RootCertStore::empty();
    for cert in load_certs(ca_cert_path)? {
        roots
            .add(cert)
            .map_err(|e| anyhow!("加载 TLS CA/服务端证书失败: {e:?}"))?;
    }

    let client_config = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));
    let server_name = ServerName::try_from(config.security.tls_server_name.clone())
        .map_err(|_| anyhow!("REAL_CTRL_TLS_SERVER_NAME 不是合法 DNS 名称"))?;
    let tls_stream = timeout(config.read_timeout, connector.connect(server_name, stream))
        .await
        .context("TLS 握手超时")??;

    let (_, session) = tls_stream.get_ref();
    let peer_certs = session
        .peer_certificates()
        .ok_or_else(|| anyhow!("TLS 握手完成但服务端未提供证书"))?;
    let end_entity = peer_certs
        .first()
        .ok_or_else(|| anyhow!("TLS 握手完成但服务端证书链为空"))?;
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
        return Err(anyhow!(
            "服务端 SPKI pin 不匹配，expected={}, actual={}",
            expected,
            actual
        ));
    }

    Ok(())
}

fn validate_sha256_pin(pin: &str) -> anyhow::Result<()> {
    let normalized = normalize_sha256_pin(pin);
    if normalized.len() != 64 || !normalized.bytes().all(|value| value.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "REAL_CTRL_TLS_SERVER_SPKI_SHA256 必须是 64 位十六进制 SHA-256"
        ));
    }
    Ok(())
}

fn load_certs(path: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path).with_context(|| format!("打开证书文件失败: {path}"))?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("读取 PEM 证书失败: {path}"))?;
    if certs.is_empty() {
        return Err(anyhow!("证书文件中没有 PEM 证书: {path}"));
    }
    Ok(certs)
}

fn load_private_key(path: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    let file = File::open(path).with_context(|| format!("打开私钥文件失败: {path}"))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("读取 PEM 私钥失败: {path}"))?
        .ok_or_else(|| anyhow!("私钥文件中没有可用私钥: {path}"))
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

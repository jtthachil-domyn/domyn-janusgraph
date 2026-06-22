use axum::serve::Listener;
use std::fs;
use std::fs::File;
use std::io::{self, BufReader};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{
    RootCertStore, ServerConfig as RustlsServerConfig, pki_types::PrivateKeyDer,
    server::WebPkiClientVerifier,
};
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TlsConfig {
    pub cert_path: String,
    pub key_path: String,
    #[serde(default)]
    pub client_ca_path: Option<String>,
    #[serde(default)]
    pub require_client_auth: bool,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            cert_path: String::new(),
            key_path: String::new(),
            client_ca_path: None,
            require_client_auth: false,
        }
    }
}

impl TlsConfig {
    pub fn load_server_config(&self) -> io::Result<RustlsServerConfig> {
        if self.require_client_auth && self.client_ca_path.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TLS client authentication requires client_ca_path",
            ));
        }
        load_rustls_server_config(
            &self.cert_path,
            &self.key_path,
            self.client_ca_path.as_deref(),
            self.require_client_auth,
        )
    }

    fn source_stamp(&self) -> io::Result<TlsSourceStamp> {
        Ok(TlsSourceStamp {
            cert: FileStamp::for_path(&self.cert_path)?,
            key: FileStamp::for_path(&self.key_path)?,
            client_ca: self
                .client_ca_path
                .as_deref()
                .map(FileStamp::for_path)
                .transpose()?,
            client_ca_path: self.client_ca_path.clone(),
            require_client_auth: self.require_client_auth,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStamp {
    fn for_path(path: impl AsRef<Path>) -> io::Result<Self> {
        let metadata = fs::metadata(path.as_ref())?;
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TlsSourceStamp {
    cert: FileStamp,
    key: FileStamp,
    client_ca: Option<FileStamp>,
    client_ca_path: Option<String>,
    require_client_auth: bool,
}

#[derive(Clone)]
pub struct ReloadingTlsAcceptor {
    tls: TlsConfig,
    state: Arc<Mutex<ReloadingTlsState>>,
}

struct ReloadingTlsState {
    stamp: TlsSourceStamp,
    acceptor: TlsAcceptor,
}

impl ReloadingTlsAcceptor {
    pub fn new(tls: TlsConfig) -> io::Result<Self> {
        let stamp = tls.source_stamp()?;
        let config = tls.load_server_config()?;
        Ok(Self {
            tls,
            state: Arc::new(Mutex::new(ReloadingTlsState {
                stamp,
                acceptor: TlsAcceptor::from(Arc::new(config)),
            })),
        })
    }

    pub fn current_acceptor(&self) -> io::Result<TlsAcceptor> {
        let next_stamp = match self.tls.source_stamp() {
            Ok(stamp) => stamp,
            Err(err) => {
                warn!(error = %err, "TLS certificate metadata check failed; keeping current TLS config");
                return Ok(self.state.lock().unwrap().acceptor.clone());
            }
        };

        let mut state = self.state.lock().unwrap();
        if state.stamp != next_stamp {
            match self.tls.load_server_config() {
                Ok(config) => {
                    state.stamp = next_stamp;
                    state.acceptor = TlsAcceptor::from(Arc::new(config));
                    warn!("TLS certificate configuration reloaded for new connections");
                }
                Err(err) => {
                    warn!(error = %err, "TLS certificate reload failed; keeping current TLS config");
                }
            }
        }

        Ok(state.acceptor.clone())
    }

    pub async fn accept<IO>(&self, io: IO) -> io::Result<tokio_rustls::server::TlsStream<IO>>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        let acceptor = self.current_acceptor()?;
        acceptor.accept(io).await
    }
}

pub fn load_rustls_server_config(
    cert_path: impl AsRef<Path>,
    key_path: impl AsRef<Path>,
    client_ca_path: Option<impl AsRef<Path>>,
    require_client_auth: bool,
) -> io::Result<RustlsServerConfig> {
    let cert_file = File::open(cert_path.as_ref())?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TLS certificate file contains no certificates",
        ));
    }

    let key_file = File::open(key_path.as_ref())?;
    let mut key_reader = BufReader::new(key_file);
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "TLS key file contains no private key",
            )
        })?;

    let builder = RustlsServerConfig::builder();
    let builder = match client_ca_path {
        Some(path) => {
            let roots = load_client_root_store(path)?;
            let verifier = if require_client_auth {
                WebPkiClientVerifier::builder(Arc::new(roots))
            } else {
                WebPkiClientVerifier::builder(Arc::new(roots)).allow_unauthenticated()
            }
            .build()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };

    builder
        .with_single_cert(certs, key)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn load_client_root_store(path: impl AsRef<Path>) -> io::Result<RootCertStore> {
    let ca_file = File::open(path.as_ref())?;
    let mut ca_reader = BufReader::new(ca_file);
    let certs = rustls_pemfile::certs(&mut ca_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TLS client CA file contains no certificates",
        ));
    }

    let mut roots = RootCertStore::empty();
    let (accepted, _) = roots.add_parsable_certificates(certs);
    if accepted == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TLS client CA file contains no parseable certificates",
        ));
    }
    Ok(roots)
}

pub struct TlsListener {
    listener: TcpListener,
    acceptor: ReloadingTlsAcceptor,
}

impl TlsListener {
    pub async fn bind(bind_addr: &str, tls: &TlsConfig) -> io::Result<Self> {
        let listener = TcpListener::bind(bind_addr).await?;
        Ok(Self {
            listener,
            acceptor: ReloadingTlsAcceptor::new(tls.clone())?,
        })
    }
}

impl Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, addr)) => match self.acceptor.accept(stream).await {
                    Ok(tls_stream) => return (tls_stream, addr),
                    Err(err) => warn!(%addr, error = %err, "TLS handshake failed"),
                },
                Err(err) => warn!(error = %err, "TLS listener accept failed"),
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_config_rejects_empty_pem_files() {
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, "").unwrap();
        std::fs::write(&key, "").unwrap();

        let err =
            load_rustls_server_config(&cert, &key, None::<&std::path::Path>, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn tls_config_requires_client_ca_when_mtls_is_required() {
        let tls = TlsConfig {
            cert_path: "cert.pem".into(),
            key_path: "key.pem".into(),
            client_ca_path: None,
            require_client_auth: true,
        };

        let err = tls.load_server_config().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn tls_source_stamp_changes_when_certificate_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, "cert-v1").unwrap();
        std::fs::write(&key, "key-v1").unwrap();

        let tls = TlsConfig {
            cert_path: cert.to_string_lossy().into_owned(),
            key_path: key.to_string_lossy().into_owned(),
            client_ca_path: None,
            require_client_auth: false,
        };
        let before = tls.source_stamp().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(2));
        std::fs::write(&cert, "cert-v2-longer").unwrap();
        let after = tls.source_stamp().unwrap();

        assert_ne!(before, after);
    }
}

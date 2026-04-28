//! TCP + TLS connection to a GrumpyDB server.

use grumpydb_protocol::Response;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::ClientError;

enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

/// A connection to a GrumpyDB server.
pub(crate) struct Connection {
    reader: BufReader<tokio::io::ReadHalf<Stream>>,
    writer: tokio::io::WriteHalf<Stream>,
}

impl tokio::io::AsyncRead for Stream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Stream::Tls(tls_stream) => std::pin::Pin::new(tls_stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for Stream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

impl Connection {
    /// Connect to a GrumpyDB server.
    pub async fn connect(host: &str, port: u16, tls: bool) -> Result<Self, ClientError> {
        let tcp = TcpStream::connect((host, port)).await?;

        let stream = if tls {
            // Accept any cert for development (dangerous_configuration)
            let tls_config = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
                .with_no_client_auth();

            let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
            let domain =
                rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| {
                    ClientError::Connection(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        e.to_string(),
                    ))
                })?;
            let tls_stream = connector.connect(domain, tcp).await?;
            Stream::Tls(Box::new(tls_stream))
        } else {
            Stream::Plain(tcp)
        };

        let (reader, writer) = tokio::io::split(stream);
        let mut conn = Self {
            reader: BufReader::new(reader),
            writer,
        };

        // Read server banner
        let mut banner = String::new();
        conn.reader.read_line(&mut banner).await?;
        // Banner is "+GRUMPYDB x.y.z\r\n" — just consume it

        Ok(conn)
    }

    /// Send a command and read the response.
    pub async fn execute(&mut self, cmd: &str) -> Result<Response, ClientError> {
        let line = if cmd.ends_with("\r\n") {
            cmd.to_string()
        } else {
            format!("{cmd}\r\n")
        };

        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;

        self.read_response().await
    }

    fn read_response(&mut self) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response, ClientError>> + Send + '_>> {
        Box::pin(async move {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                return Err(ClientError::Connection(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "server closed connection",
                )));
            }

            // Handle multi-line responses (bulk strings, arrays)
            let first = line.as_bytes()[0];
            match first {
                b'$' => self.read_bulk_response(&line).await,
                b'*' => self.read_array_response(&line).await,
                _ => {
                    // Single-line response
                    let (resp, _) = Response::parse(&line)
                        .map_err(|e| ClientError::Protocol(e.to_string()))?;
                    Ok(resp)
                }
            }
        })
    }

    async fn read_bulk_response(&mut self, header: &str) -> Result<Response, ClientError> {
        let len_str = header[1..].trim();
        let len: i64 = len_str
            .parse()
            .map_err(|_| ClientError::Protocol(format!("invalid bulk length: {len_str}")))?;

        if len < 0 {
            return Ok(Response::Bulk(None));
        }

        let len = len as usize;
        let mut data = vec![0u8; len + 2]; // +2 for trailing \r\n
        tokio::io::AsyncReadExt::read_exact(&mut self.reader, &mut data).await?;
        let content = String::from_utf8_lossy(&data[..len]).to_string();
        Ok(Response::Bulk(Some(content)))
    }

    async fn read_array_response(&mut self, header: &str) -> Result<Response, ClientError> {
        let count_str = header[1..].trim();
        let count: usize = count_str
            .parse()
            .map_err(|_| ClientError::Protocol(format!("invalid array count: {count_str}")))?;

        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            let resp = self.read_response().await?;
            items.push(resp);
        }
        Ok(Response::Array(items))
    }
}

/// Accept any TLS certificate (for development with self-signed certs).
#[derive(Debug)]
struct NoCertVerifier;

impl rustls::client::danger::ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
        ]
    }
}

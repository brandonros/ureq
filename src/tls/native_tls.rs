use std::convert::TryFrom;
use std::fmt;
use std::io::{Read, Write};
use std::sync::Arc;

use crate::time::Duration;
use crate::{transport::*, Error};
use der::pem::LineEnding;
use der::Document;
use http::uri::Scheme;
use native_tls::{Certificate, HandshakeError, Identity, TlsConnector, TlsStream};
use once_cell::sync::OnceCell;

use super::TlsConfig;

#[derive(Default)]
pub struct NativeTlsConnector {
    connector: OnceCell<Arc<TlsConnector>>,
}

impl Connector for NativeTlsConnector {
    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<Box<dyn Transport>>,
    ) -> Result<Option<Box<dyn Transport>>, Error> {
        let Some(transport) = chained else {
            panic!("NativeTlsConnector requires a chained transport");
        };

        // Only add TLS if we are connecting via HTTPS and the transport isn't TLS
        // already, otherwise use chained transport as is.
        if details.uri.scheme() != Some(&Scheme::HTTPS) || transport.is_tls() {
            trace!("Skip");
            return Ok(Some(transport));
        }

        trace!("Try wrap TLS");

        let tls_config = &details.config.tls_config;

        // Initialize the connector on first run.
        let connector_ref = match self.connector.get() {
            Some(v) => v,
            None => {
                // This is unlikely to be racy, but if it is, doesn't matter much.
                let c = build_connector(tls_config)?;
                // Maybe someone else set it first. Weird, but ok.
                let _ = self.connector.set(c);
                self.connector.get().unwrap()
            }
        };
        let connector = connector_ref.clone(); // cheap clone due to Arc

        let domain = details
            .uri
            .authority()
            .expect("uri authority for tls")
            .host()
            .to_string();

        let adapter = TransportAdapter::new(transport);
        let stream = LazyStream::Unstarted(Some((connector, domain, adapter)));

        let buffers = LazyBuffers::new(
            details.config.input_buffer_size,
            details.config.output_buffer_size,
        );

        let transport = Box::new(NativeTlsTransport { buffers, stream });

        debug!("Wrapped TLS");

        Ok(Some(transport))
    }
}

fn build_connector(tls_config: &TlsConfig) -> Result<Arc<TlsConnector>, Error> {
    let mut builder = TlsConnector::builder();

    if tls_config.disable_verification {
        debug!("Certificate verification disabled");
        builder.danger_accept_invalid_certs(true);
        builder.danger_accept_invalid_hostnames(true);
    } else {
        let mut added = 0;
        let mut ignored = 0;
        for cert in &tls_config.root_certs {
            let c = match Certificate::from_der(cert.der()) {
                Ok(v) => v,
                Err(e) => {
                    // Invalid/expired/broken root certs are expected
                    // in a native root store.
                    trace!("Ignore invalid root cert: {}", e);
                    ignored += 1;
                    continue;
                }
            };
            builder.add_root_certificate(c);
            added += 1;
        }
        debug!("Added {} and ignored {} root certs", added, ignored);
    }

    if let Some((certs, key)) = &tls_config.client_cert {
        let certs_pem = certs
            .iter()
            .map(|c| pemify(c.der(), "CERTIFICATE"))
            .collect::<Result<String, Error>>()?;

        let key_pem = pemify(key.der(), "PRIVATE KEY")?;

        debug!("Use client certficiate with key kind {:?}", key.kind());

        let identity = Identity::from_pkcs8(certs_pem.as_bytes(), key_pem.as_bytes())?;
        builder.identity(identity);
    }

    builder.use_sni(tls_config.use_sni);

    if !tls_config.use_sni {
        debug!("Disable SNI");
    }

    let conn = builder.build()?;

    Ok(Arc::new(conn))
}

fn pemify(der: &[u8], label: &'static str) -> Result<String, Error> {
    let doc = Document::try_from(der)?;
    let pem = doc.to_pem(label, LineEnding::LF)?;
    Ok(pem)
}

struct NativeTlsTransport {
    buffers: LazyBuffers,
    stream: LazyStream,
}

impl Transport for NativeTlsTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: Duration) -> Result<(), Error> {
        let stream = self.stream.handshaken()?;
        stream.get_mut().timeout = timeout;

        let output = &self.buffers.output()[..amount];
        stream.write_all(output)?;

        Ok(())
    }

    fn await_input(&mut self, timeout: Duration) -> Result<(), Error> {
        if self.buffers.can_use_input() {
            return Ok(());
        }

        let stream = self.stream.handshaken()?;
        stream.get_mut().timeout = timeout;

        let input = self.buffers.input_mut();
        let amount = stream.read(input)?;
        self.buffers.add_filled(amount);

        Ok(())
    }

    fn consume_input(&mut self, amount: usize) {
        self.buffers.consume(amount);
    }

    fn is_tls(&self) -> bool {
        true
    }
}

/// Helper to delay the handshake until we are starting IO.
/// This normalizes native-tls to behave like rustls.
enum LazyStream {
    Unstarted(Option<(Arc<TlsConnector>, String, TransportAdapter)>),
    Started(TlsStream<TransportAdapter>),
}

impl LazyStream {
    fn handshaken(&mut self) -> Result<&mut TlsStream<TransportAdapter>, Error> {
        match self {
            LazyStream::Unstarted(v) => {
                let (conn, domain, adapter) = v.take().unwrap();
                let stream = conn.connect(&domain, adapter).map_err(|e| match e {
                    HandshakeError::Failure(e) => e,
                    HandshakeError::WouldBlock(_) => unreachable!(),
                })?;
                *self = LazyStream::Started(stream);
                // Next time we hit the other match arm
                return self.handshaken();
            }
            LazyStream::Started(v) => Ok(v),
        }
    }
}
impl fmt::Debug for NativeTlsConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeTlsConnector").finish()
    }
}

impl fmt::Debug for NativeTlsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeTlsTransport").finish()
    }
}
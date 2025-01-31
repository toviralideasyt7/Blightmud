use crate::net::RwStream;
use anyhow::Result;
use rustls::{ClientConfig, ClientConnection, OwnedTrustAnchor, RootCertStore, StreamOwned};
use std::fmt::{Display, Formatter};
use std::net::TcpStream;
use std::sync::Arc;

/// Indicates a user's preference for certificate validation.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CertificateValidation {
    /// Certificate validation is performed and the server must present a valid certificate
    /// chain to a known root certificate.
    Enabled,
    /// DANGER: Certificate validation is **not** performed and connections will be vulnerable
    /// to person-in the-middle attacks and tampering.
    DangerousDisabled,
}

impl Display for CertificateValidation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                CertificateValidation::Enabled => true,
                CertificateValidation::DangerousDisabled => false,
            }
        )
    }
}

impl From<bool> for CertificateValidation {
    fn from(value: bool) -> Self {
        match value {
            true => CertificateValidation::Enabled,
            false => CertificateValidation::DangerousDisabled,
        }
    }
}

/// TlsStream is an alias for a read/write stream over an owned TLS client connection stream
/// using a TCP transport.
pub(super) type TlsStream = RwStream<StreamOwned<ClientConnection, TcpStream>>;

impl TlsStream {
    /// new constructs a [TlsStream] by attempting to establish a TLS session over the given
    /// [TcpStream] for the provided hostname. Certificate chains will be validated using
    /// a built-in set of CA certificates populated from the Mozilla root certificate program
    /// used by Firefox.
    ///
    /// ## DANGER
    /// If the `verify_cert` bool is set to false no certificate verification is performed and
    /// the connection is vulnerable to person-in-the-middle attacks and tampering.
    pub(super) fn tls_init(
        stream: TcpStream,
        host: &str,
        validation: CertificateValidation,
    ) -> Result<TlsStream> {
        Self::tls_init_with_roots(stream, host, validation, Self::default_root_certs())
    }

    // tls_init, but also accepts a RootCertStore. Presently this is only used by tests to
    // allow verifying certificate validation with a non-standard test CA.
    fn tls_init_with_roots(
        stream: TcpStream,
        host: &str,
        validation: CertificateValidation,
        roots: RootCertStore,
    ) -> Result<TlsStream> {
        let mut config = ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(roots)
            .with_no_client_auth();

        if let CertificateValidation::DangerousDisabled = validation {
            config
                .dangerous()
                .set_certificate_verifier(Arc::new(danger::NoCertificateVerification {}));
        };

        let server_name = host.try_into()?;
        let conn = ClientConnection::new(Arc::new(config), server_name)?;
        Ok(RwStream::new(StreamOwned::new(conn, stream)))
    }

    fn default_root_certs() -> RootCertStore {
        let mut root_store = RootCertStore::empty();
        root_store.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
            OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        }));
        root_store
    }
}

/// here be dragons.
mod danger {
    use rustls::{client, Certificate, ServerName};

    /// NoCertificateVerification is a **DANGEROUS** [client::ServerCertVerifier] that
    /// performs **no** certificate validation.
    pub struct NoCertificateVerification {}

    impl client::ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &Certificate,
            _intermediates: &[Certificate],
            _server_name: &ServerName,
            _scts: &mut dyn Iterator<Item = &[u8]>,
            _ocsp: &[u8],
            _now: std::time::SystemTime,
        ) -> Result<client::ServerCertVerified, rustls::Error> {
            Ok(client::ServerCertVerified::assertion())
        }
    }
}

#[cfg(test)]
mod test_tls {
    use crate::net::tls::TlsStream;
    use crate::net::CertificateValidation;
    use log::debug;
    use rustls::{
        Certificate, CertificateError, Error::InvalidCertificate, PrivateKey, RootCertStore,
        ServerConfig, ServerConnection, StreamOwned,
    };
    use std::io::{BufReader, Read};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::{fs, thread, time};

    // See tests/certs/README.md for information on how to (re)generate these files.
    const TEST_SERVER_CERTS: &str = "tests/certs/localhost/cert.pem";
    const TEST_SERVER_KEY: &str = "tests/certs/localhost/key.pem";
    const TEST_CA_CERTS: &str = "tests/certs/minica.pem";

    fn load_certs(filename: &str) -> Vec<Certificate> {
        let certfile = fs::File::open(filename).expect("cannot open certificate file");
        let mut reader = BufReader::new(certfile);
        rustls_pemfile::certs(&mut reader)
            .unwrap()
            .iter()
            .map(|v| Certificate(v.clone()))
            .collect()
    }

    fn load_private_key(filename: &str) -> PrivateKey {
        let keyfile = fs::File::open(filename).expect("cannot open private key file");
        let mut reader = BufReader::new(keyfile);

        loop {
            match rustls_pemfile::read_one(&mut reader).expect("cannot parse private key .pem file")
            {
                Some(rustls_pemfile::Item::RSAKey(key)) => return PrivateKey(key),
                None => break,
                _ => {}
            }
        }

        panic!("no keys found in {:?}", filename);
    }

    fn test_ca_roots() -> RootCertStore {
        let mut root_store = RootCertStore::empty();
        load_certs(TEST_CA_CERTS).iter().for_each(|c| {
            root_store.add(c).unwrap();
        });
        root_store
    }

    fn test_server(addr: SocketAddr) -> (SocketAddr, JoinHandle<()>) {
        let cert_chain = load_certs(TEST_SERVER_CERTS);
        let priv_key = load_private_key(TEST_SERVER_KEY);
        let config = Arc::new(
            ServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(cert_chain, priv_key)
                .unwrap(),
        );
        let listener = TcpListener::bind(addr).expect("cannot listen on port");
        let bound_addr = listener.local_addr().unwrap();

        (
            bound_addr,
            thread::spawn(move || handle_connection(listener.accept().unwrap(), config)),
        )
    }

    fn handle_connection(accept: (TcpStream, SocketAddr), config: Arc<ServerConfig>) {
        let tls_conn = ServerConnection::new(Arc::clone(&config)).unwrap();
        let mut stream = StreamOwned::new(tls_conn, accept.0);

        loop {
            let mut data = String::new();
            match stream.read_to_string(&mut data) {
                Ok(res) => match res {
                    0 => {
                        debug!("read EOF. Closing server");
                        return;
                    }
                    size => {
                        debug!("read {} bytes: {}", size, data);
                    }
                },
                Err(e) => {
                    debug!("read err: {:?}", e);
                    return;
                }
            }
        }
    }

    fn connect_to_server(addr: SocketAddr) -> TcpStream {
        for i in 0..10 {
            if let Ok(s) = TcpStream::connect(addr) {
                return s;
            }
            thread::sleep(time::Duration::from_millis(i * 100));
        }
        panic!("failed to connect to {:?} after 10 tries", addr);
    }

    #[test]
    /// Test that connecting to a TLS server w/ certificate validation works as expected when the
    /// server has a valid certificate.
    fn test_tls_init_verify() {
        let _ = env_logger::try_init();

        // Start up a test server on a random port.
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (bound_addr, server_handle) = test_server(addr);
        debug!("test server listening at {}", bound_addr);

        // Setup a TLS stream to the server.
        // We enable certificate validation - for this to work we must also provide the test CA root
        // certificates since our test server isn't using a "real" certificate from a CA in the
        // root CA collection provided by default_root_certs().
        debug!("connecting to {}", bound_addr);
        let tls_stream = TlsStream::tls_init_with_roots(
            connect_to_server(bound_addr),
            "localhost",
            CertificateValidation::Enabled,
            test_ca_roots(),
        )
        .unwrap();

        // Write a message so that we perform the full TLS handshake.
        {
            debug!("writing message");
            let mut out_writer = tls_stream.output_stream.lock().unwrap();
            out_writer.write("Hey!!!!".as_ref()).unwrap();
        }

        // Shut down the client and drop the connection so the server reads EOF and stops.
        tls_stream.inner().sock.shutdown(Shutdown::Both).unwrap();
        drop(tls_stream);

        debug!("waiting for server thread to join");
        server_handle.join().unwrap();
        debug!("all done!");
    }

    #[test]
    /// Test that connecting to a TLS server w/ certificate validation errors as expected when
    /// the server uses a certificate issued by an unknown CA.
    fn test_tls_init_verify_err() {
        let _ = env_logger::try_init();

        // Start up a test server on a random port.
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (bound_addr, _) = test_server(addr);
        debug!("test server listening at {}", bound_addr);

        // Setup a TLS stream to the server.  We enable certificate validation but *don't* provide
        // the test CA roots. This should cause the connection attempt to fail when we try to write
        // as the server's certificate isn't issued by a CA we know about.
        debug!("connecting to {}", bound_addr);
        let tls_stream = TlsStream::tls_init(
            connect_to_server(bound_addr),
            "localhost",
            CertificateValidation::Enabled,
        )
        .unwrap();

        // Write a message so that we perform the full TLS handshake.
        debug!("writing message");
        let mut out_writer = tls_stream.output_stream.lock().unwrap();
        let res = out_writer.write("Hey!!!!".as_ref());

        // The write should have errored because of an unknown issuer certificate error.
        match res {
            Ok(_) => panic!("did not error from invalid certificate"),
            Err(e) => {
                let tls_err = e.into_inner().unwrap().downcast::<rustls::Error>().unwrap();

                debug!("tls_err: {:?}", tls_err);
                assert_eq!(
                    *tls_err,
                    InvalidCertificate(CertificateError::UnknownIssuer)
                );
            }
        }
    }

    #[test]
    /// Test that connecting to a TLS server w/o certificate validation works as expected, even
    /// when the test server uses a certificate issued by an unknown CA.
    fn test_tls_init_no_verify() {
        let _ = env_logger::try_init();

        // Start up a test server on a random port.
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (bound_addr, server_handle) = test_server(addr);
        debug!("test server listening at {}", bound_addr);

        // Setup a TLS stream to the server.
        // We disable certificate validation, and also don't provide any custom roots.
        // Even though our test server uses a cert issued by an unknown root certificate this
        // configuration should not error because of the dangerous certificate validation state.
        debug!("connecting to {}", bound_addr);
        let tls_stream = TlsStream::tls_init(
            connect_to_server(bound_addr),
            "localhost",
            CertificateValidation::DangerousDisabled,
        )
        .unwrap();

        // Write a message so that we perform the full TLS handshake.
        {
            debug!("writing message");
            let mut out_writer = tls_stream.output_stream.lock().unwrap();
            out_writer.write("Hey!!!!".as_ref()).unwrap();
        }

        // Shut down the client and drop the connection so the server reads EOF and stops.
        tls_stream.inner().sock.shutdown(Shutdown::Both).unwrap();
        drop(tls_stream);

        debug!("waiting for server thread to join");
        server_handle.join().unwrap();
        debug!("all done!");
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum KeyError {
    // .map_err(|_| err("invalid key file"))
    BadFile(std::io::Error),
    // ("failed to find key header; supported formats are: RSA, PKCS8, SEC1")
    MissingHeader,
    NoKeysFound,
    // Err(err("no valid keys found; is the file malformed?")),
    // Err(err(format!("expected 1 key, found {}", n))),
    BadKeyCount(usize),
    // .map_err(|_| err("key parsed but is unusable"))
    Unsupported,
    Io(std::io::Error),
    Unusable(rustls::Error),
    BadItem(rustls_pemfile::Item),
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Tls(rustls::Error),
    Mtls(rustls::server::VerifierBuilderError),
    CertChain(std::io::Error),
    MissingKeyHeader,
    PrivKey(KeyError),
    CertAuth(rustls::Error),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
       Error::Io(e)
    }
}

impl From<rustls::Error> for Error {
    fn from(e: rustls::Error) -> Self {
        Error::Tls(e)
    }
}

impl From<rustls::server::VerifierBuilderError> for Error {
    fn from(value: rustls::server::VerifierBuilderError) -> Self {
        Error::Mtls(value)
    }
}

impl From<KeyError> for Error {
    fn from(value: KeyError) -> Self {
        Error::PrivKey(value)
    }
}

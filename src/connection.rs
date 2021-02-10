#[cfg(feature = "openssl")]
use crate::native_tls::{TlsConnector, TlsStream};
use crate::{Error, Method, Request, ResponseLazy};
#[cfg(feature = "native-tls")]
use native_tls::{TlsConnector, TlsStream};
#[cfg(feature = "rustls")]
use rustls::{self, ClientConfig, ClientSession, StreamOwned};
use std::env;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(feature = "rustls")]
use std::sync::Arc;
use std::time::{Duration, Instant};
#[cfg(feature = "webpki")]
use webpki::DNSNameRef;
#[cfg(feature = "webpki")]
use webpki_roots::TLS_SERVER_ROOTS;

#[cfg(feature = "rustls")]
lazy_static::lazy_static! {
    static ref CONFIG: Arc<ClientConfig> = {
        let mut config = ClientConfig::new();
        config
            .root_store
            .add_server_trust_anchors(&TLS_SERVER_ROOTS);
        Arc::new(config)
    };
}

type UnsecuredStream = BufReader<TcpStream>;
#[cfg(feature = "rustls")]
type SecuredStream = StreamOwned<ClientSession, TcpStream>;
#[cfg(any(feature = "openssl", feature = "native-tls"))]
type SecuredStream = TlsStream<TcpStream>;

pub(crate) enum HttpStream {
    Unsecured(UnsecuredStream, Option<Instant>),
    #[cfg(any(feature = "rustls", feature = "openssl", feature = "native-tls"))]
    Secured(Box<SecuredStream>, Option<Instant>),
}

impl HttpStream {
    fn create_unsecured(reader: UnsecuredStream, timeout_at: Option<Instant>) -> HttpStream {
        HttpStream::Unsecured(reader, timeout_at)
    }

    #[cfg(any(feature = "rustls", feature = "openssl", feature = "native-tls"))]
    fn create_secured(reader: SecuredStream, timeout_at: Option<Instant>) -> HttpStream {
        HttpStream::Secured(Box::new(reader), timeout_at)
    }
}

impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout = |tcp: &TcpStream, timeout_at: Option<Instant>| {
            if let Some(timeout_at) = timeout_at {
                let now = Instant::now();
                if timeout_at <= now {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "The request's timeout was reached.",
                    ));
                } else {
                    tcp.set_read_timeout(Some(timeout_at - now)).ok();
                }
            }
            Ok(())
        };

        match self {
            HttpStream::Unsecured(inner, timeout_at) => {
                timeout(inner.get_ref(), *timeout_at)?;
                inner.read(buf)
            }
            #[cfg(any(feature = "rustls", feature = "openssl", feature = "native-tls"))]
            HttpStream::Secured(inner, timeout_at) => {
                timeout(inner.get_ref(), *timeout_at)?;
                inner.read(buf)
            }
        }
    }
}

/// A connection to the server for sending
/// [`Request`](struct.Request.html)s.
pub struct Connection {
    request: Request,
    timeout: Option<u64>,
}

impl Connection {
    /// Creates a new `Connection`. See
    /// [`Request`](struct.Request.html) for specifics about *what* is
    /// being sent.
    pub(crate) fn new(request: Request) -> Connection {
        let timeout = request
            .timeout
            .or_else(|| match env::var("MINREQ_TIMEOUT") {
                Ok(t) => t.parse::<u64>().ok(),
                Err(_) => None,
            });
        Connection { request, timeout }
    }

    /// Sends the [`Request`](struct.Request.html), consumes this
    /// connection, and returns a [`Response`](struct.Response.html).
    #[cfg(feature = "rustls")]
    pub(crate) fn send_https(mut self) -> Result<ResponseLazy, Error> {
        self.request.host = ensure_ascii_host(self.request.host)?;
        let bytes = self.request.as_bytes();
        let mut timeout_duration = self.timeout.map(|d| Duration::from_secs(d));
        let timeout_at = timeout_duration.map(|d| Instant::now() + d);

        // Rustls setup
        let dns_name = &self.request.host;
        // parse_url in response.rs ensures that there is always a
        // ":port" in the host, which is why this unwrap is safe.
        let dns_name = dns_name.split(':').next().unwrap();
        let dns_name = match DNSNameRef::try_from_ascii_str(dns_name) {
            Ok(result) => result,
            Err(err) => return Err(Error::IoError(io::Error::new(io::ErrorKind::Other, err))),
        };
        let sess = ClientSession::new(&CONFIG, dns_name);

        let tcp = self.connect(timeout_duration)?;

        // Connect phase may have taken spend some time. so, calibrating the timeout.
        calibrate_timeout(&mut timeout_duration, timeout_at)?;

        // Send request
        let mut tls = StreamOwned::new(sess, tcp);
        // The connection could drop mid-write, so set a timeout
        tls.get_ref().set_write_timeout(timeout_duration).ok();
        tls.write(&bytes)?;

        // Receive request
        let response = ResponseLazy::from_stream(HttpStream::create_secured(tls, timeout_at))?;
        handle_redirects(self, response)
    }

    /// Sends the [`Request`](struct.Request.html), consumes this
    /// connection, and returns a [`Response`](struct.Response.html).
    #[cfg(any(feature = "openssl", feature = "native-tls"))]
    pub(crate) fn send_https(mut self) -> Result<ResponseLazy, Error> {
        self.request.host = ensure_ascii_host(self.request.host)?;
        let bytes = self.request.as_bytes();
        let mut timeout_duration = self.timeout.map(|d| Duration::from_secs(d));
        let timeout_at = timeout_duration.map(|d| Instant::now() + d);

        let dns_name = &self.request.host;
        // parse_url in response.rs ensures that there is always a
        // ":port" in the host, which is why this unwrap is safe.
        let dns_name = dns_name.split(':').next().unwrap();
        /*
        let mut builder = TlsConnector::builder();
        ...
        let sess = match builder.build() {
        */
        let sess = match TlsConnector::new() {
            Ok(sess) => sess,
            Err(err) => return Err(Error::IoError(io::Error::new(io::ErrorKind::Other, err))),
        };

        let tcp = self.connect(timeout_duration)?;

        // Connect phase may have taken spend some time. so, calibrating the timeout.
        calibrate_timeout(&mut timeout_duration, timeout_at)?;

        // Send request
        let mut tls = match sess.connect(dns_name, tcp) {
            Ok(tls) => tls,
            Err(err) => return Err(Error::IoError(io::Error::new(io::ErrorKind::Other, err))),
        };
        // The connection could drop mid-write, so set a timeout
        tls.get_ref().set_write_timeout(timeout_duration).ok();
        tls.write(&bytes)?;

        // Receive request
        let response = ResponseLazy::from_stream(HttpStream::create_secured(tls, timeout_at))?;
        handle_redirects(self, response)
    }

    /// Sends the [`Request`](struct.Request.html), consumes this
    /// connection, and returns a [`Response`](struct.Response.html).
    pub(crate) fn send(mut self) -> Result<ResponseLazy, Error> {
        self.request.host = ensure_ascii_host(self.request.host)?;
        let bytes = self.request.as_bytes();
        let mut timeout_duration = self.timeout.map(Duration::from_secs);
        let timeout_at = timeout_duration.map(|d| Instant::now() + d);

        let tcp = self.connect(timeout_duration)?;

        // Connect phase may have taken spend some time. so, calibrating the timeout.
        calibrate_timeout(&mut timeout_duration, timeout_at)?;

        // Send request
        let mut stream = BufWriter::new(tcp);
        stream.get_ref().set_write_timeout(timeout_duration).ok();
        stream.write_all(&bytes)?;

        // Receive response
        let tcp = match stream.into_inner() {
            Ok(tcp) => tcp,
            Err(_) => {
                return Err(Error::Other(
                    "IntoInnerError after writing the request into the TcpStream.",
                ));
            }
        };
        let stream = HttpStream::create_unsecured(BufReader::new(tcp), timeout_at);
        let response = ResponseLazy::from_stream(stream)?;
        handle_redirects(self, response)
    }

    fn connect(&self, timeout: Option<Duration>) -> Result<TcpStream, Error> {
        let tcp_connect = |host: &str| -> Result<TcpStream, Error> {
            if let Some(timeout) = timeout {
                let sock_address = host
                    .to_socket_addrs()
                    .map_err(Error::IoError)?
                    .next()
                    .ok_or(Error::Other("failed to lookup address information"))?;
                TcpStream::connect_timeout(&sock_address, timeout)
            } else {
                TcpStream::connect(host)
            }
            .map_err(Error::from)
        };

        #[cfg(feature = "proxy")]
        match self.request.proxy {
            Some(ref proxy) => {
                // do proxy things
                let proxy_host = format!("{}:{}", proxy.server, proxy.port);
                let mut tcp = tcp_connect(&proxy_host)?;

                write!(tcp, "{}", proxy.connect(self.request.host.as_str())).unwrap();
                tcp.flush()?;

                let mut proxy_response = Vec::new();

                loop {
                    let mut buf = vec![0; 256];
                    let total = tcp.read(&mut buf)?;
                    proxy_response.append(&mut buf);
                    if total < 256 {
                        break;
                    }
                }

                crate::Proxy::verify_response(&proxy_response)?;

                Ok(tcp)
            }
            None => tcp_connect(&self.request.host),
        }

        #[cfg(not(feature = "proxy"))]
        tcp_connect(&self.request.host)
    }
}

fn handle_redirects(connection: Connection, response: ResponseLazy) -> Result<ResponseLazy, Error> {
    let status_code = response.status_code;
    let url = response.headers.get("location");
    if let Some(request) = get_redirect(connection, status_code, url) {
        request?.send_lazy()
    } else {
        Ok(response)
    }
}

fn get_redirect(
    connection: Connection,
    status_code: i32,
    url: Option<&String>,
) -> Option<Result<Request, Error>> {
    match status_code {
        301 | 302 | 303 | 307 => {
            let url = match url {
                Some(url) => url,
                None => return Some(Err(Error::RedirectLocationMissing)),
            };

            match connection.request.redirect_to(url.clone()) {
                Ok(mut request) => {
                    if status_code == 303 {
                        match request.method {
                            Method::Post | Method::Put | Method::Delete => {
                                request.method = Method::Get;
                            }
                            _ => {}
                        }
                    }

                    Some(Ok(request))
                }
                Err(err) => Some(Err(err)),
            }
        }

        _ => None,
    }
}

fn ensure_ascii_host(host: String) -> Result<String, Error> {
    if host.is_ascii() {
        Ok(host)
    } else {
        #[cfg(not(feature = "punycode"))]
        {
            Err(Error::PunycodeFeatureNotEnabled)
        }

        #[cfg(feature = "punycode")]
        {
            let mut result = String::with_capacity(host.len() * 2);
            for s in host.split('.') {
                if s.is_ascii() {
                    result += s;
                } else {
                    match punycode::encode(s) {
                        Ok(s) => result = result + "xn--" + &s,
                        Err(_) => return Err(Error::PunycodeConversionFailed),
                    }
                }
                result += ".";
            }
            result.truncate(result.len() - 1); // Remove the trailing dot
            Ok(result)
        }
    }
}

fn calibrate_timeout(
    timeout: &mut Option<Duration>,
    timeout_at: Option<Instant>,
) -> Result<(), Error> {
    if let (Some(timeout), Some(timeout_at)) = (timeout, timeout_at) {
        if let Some(balance_time) = timeout_at.checked_duration_since(Instant::now()) {
            *timeout = balance_time;
        } else {
            return Err(Error::IoError(io::Error::new(
                io::ErrorKind::TimedOut,
                "the request's timeout was reached during the initial connection",
            )));
        }
    }

    Ok(())
}

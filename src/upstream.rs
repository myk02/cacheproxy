//! How the proxy talks to the origin server.
//!
//! The proxy only needs the [`Upstream`] trait, so tests can plug in a mock
//! and the real thing can be swapped (for example for a TLS client).

use std::io::{self, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::http::{is_hop_by_hop, read_response, Request, Response};

/// Something that can answer a request, normally the origin server.
pub trait Upstream: Send + Sync {
    fn fetch(&self, req: &Request) -> io::Result<Response>;
}

/// Where the origin lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub host: String,
    pub port: u16,
}

impl Origin {
    /// The value for the `Host` header: the port is left out when it is 80.
    pub fn host_header(&self) -> String {
        if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Parse `http://host[:port]`.
pub fn parse_origin(s: &str) -> Result<Origin, String> {
    let rest = if let Some(r) = s.strip_prefix("http://") {
        r
    } else if s.starts_with("https://") {
        return Err(
            "https:// origins are not supported yet (this build has no TLS \
                    client). Use an http:// origin, for example a local TLS terminator."
                .to_string(),
        );
    } else {
        return Err("origin must start with http://".to_string());
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() || rest.contains('/') || rest.contains('?') {
        return Err("origin must look like http://host[:port] (no path)".to_string());
    }
    match rest.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port: u16 = port
                .parse()
                .map_err(|_| format!("invalid port in origin: {port}"))?;
            Ok(Origin {
                host: host.to_string(),
                port,
            })
        }
        _ => Ok(Origin {
            host: rest.to_string(),
            port: 80,
        }),
    }
}

/// Plain HTTP/1.1 client: one TCP connection per request (`Connection: close`).
pub struct HttpUpstream {
    origin: Origin,
    timeout: Duration,
    max_body: usize,
}

impl HttpUpstream {
    pub fn new(origin: Origin, timeout: Duration, max_body: usize) -> Self {
        HttpUpstream {
            origin,
            timeout,
            max_body,
        }
    }

    fn connect(&self) -> io::Result<TcpStream> {
        let addrs = (self.origin.host.as_str(), self.origin.port).to_socket_addrs()?;
        let mut last = io::Error::new(io::ErrorKind::NotFound, "origin host did not resolve");
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, self.timeout) {
                Ok(s) => return Ok(s),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

impl Upstream for HttpUpstream {
    fn fetch(&self, req: &Request) -> io::Result<Response> {
        let mut stream = self.connect()?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        stream.set_nodelay(true)?;

        let mut head = Vec::with_capacity(512);
        write!(head, "{} {} HTTP/1.1\r\n", req.method, req.target)?;
        write!(head, "Host: {}\r\n", self.origin.host_header())?;
        for (k, v) in &req.headers {
            if is_hop_by_hop(k)
                || k.eq_ignore_ascii_case("host")
                || k.eq_ignore_ascii_case("content-length")
                || k.contains(['\r', '\n'])
                || v.contains(['\r', '\n'])
            {
                continue;
            }
            write!(head, "{k}: {v}\r\n")?;
        }
        if !req.body.is_empty() {
            write!(head, "Content-Length: {}\r\n", req.body.len())?;
        }
        write!(head, "Connection: close\r\n\r\n")?;
        head.extend_from_slice(&req.body);
        stream.write_all(&head)?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        read_response(&mut reader, req.method == "HEAD", self.max_body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_origins() {
        assert_eq!(
            parse_origin("http://localhost:3000").unwrap(),
            Origin {
                host: "localhost".into(),
                port: 3000
            }
        );
        assert_eq!(
            parse_origin("http://example.com/").unwrap(),
            Origin {
                host: "example.com".into(),
                port: 80
            }
        );
    }

    #[test]
    fn rejects_bad_origins() {
        assert!(parse_origin("https://example.com")
            .unwrap_err()
            .contains("not supported"));
        assert!(parse_origin("example.com").is_err());
        assert!(parse_origin("http://").is_err());
        assert!(parse_origin("http://example.com/api").is_err());
        assert!(parse_origin("http://example.com:notaport").is_err());
        assert!(parse_origin("http://example.com:99999").is_err());
    }

    #[test]
    fn host_header_hides_port_80() {
        let a = Origin {
            host: "a.test".into(),
            port: 80,
        };
        let b = Origin {
            host: "a.test".into(),
            port: 8080,
        };
        assert_eq!(a.host_header(), "a.test");
        assert_eq!(b.host_header(), "a.test:8080");
    }
}

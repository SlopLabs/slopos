//! curl: one URL over HTTP/1.1, in the clear or over TLS 1.3, with the body
//! streamed to stdout or a file as it arrives. Exit codes follow curl's own.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::Duration;

use slopos_http_core::{self as http, Chunked, Head, HeadError, Scheme, Url, UrlError};

use crate::net::ResolveError;
use crate::syscall::process;
use crate::tls::{self, CipherSuite, ClientConfig, ConnectError, TlsStream, TrustStore};

const MAX_REDIRECTS: usize = 10;
const MAX_HEAD_BYTES: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);

const USAGE: &str =
    "usage: curl [-vLfsS] [-o file] [-X method] [-H header] [-d data] [--cacert file] <url>";

struct Failure {
    code: i32,
    msg: String,
}

fn fail<T>(code: i32, msg: impl Into<String>) -> Result<T, Failure> {
    Err(Failure {
        code,
        msg: msg.into(),
    })
}

mod exit {
    pub const UNSUPPORTED_PROTOCOL: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const MALFORMED_URL: i32 = 3;
    pub const RESOLVE: i32 = 6;
    pub const CONNECT: i32 = 7;
    pub const WEIRD_SERVER_REPLY: i32 = 8;
    pub const PARTIAL_FILE: i32 = 18;
    pub const HTTP_ERROR: i32 = 22;
    pub const WRITE: i32 = 23;
    pub const TIMEOUT: i32 = 28;
    pub const TLS_CONNECT: i32 = 35;
    pub const TOO_MANY_REDIRECTS: i32 = 47;
    pub const EMPTY_REPLY: i32 = 52;
    pub const SEND: i32 = 55;
    pub const RECV: i32 = 56;
    pub const PEER_UNVERIFIED: i32 = 60;
    pub const CA_FILE: i32 = 77;
}

struct Config {
    verbose: bool,
    silent: bool,
    show_error: bool,
    follow: bool,
    fail_on_http_error: bool,
    method: Option<String>,
    output: Option<String>,
    headers: Vec<String>,
    data: Option<Vec<u8>>,
    cacert: Option<String>,
    url: String,
}

fn parse_args(args: &[String]) -> Result<Config, Failure> {
    let mut cfg = Config {
        verbose: false,
        silent: false,
        show_error: false,
        follow: false,
        fail_on_http_error: false,
        method: None,
        output: None,
        headers: Vec::new(),
        data: None,
        cacert: None,
        url: String::new(),
    };
    let mut url = None;
    let mut it = args.iter().skip(1);
    while let Some(arg) = it.next() {
        let (flags, glued): (Vec<char>, Option<String>) = match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--output" => (vec!['o'], None),
            "--request" => (vec!['X'], None),
            "--header" => (vec!['H'], None),
            "--data" => (vec!['d'], None),
            "--verbose" => (vec!['v'], None),
            "--location" => (vec!['L'], None),
            "--fail" => (vec!['f'], None),
            "--silent" => (vec!['s'], None),
            "--show-error" => (vec!['S'], None),
            "--cacert" => {
                let path = it.next().ok_or_else(|| Failure {
                    code: exit::USAGE,
                    msg: "option --cacert needs a value".into(),
                })?;
                cfg.cacert = Some(path.clone());
                continue;
            }
            cluster
                if cluster.len() > 1 && cluster.starts_with('-') && !cluster.starts_with("--") =>
            {
                let body = &cluster[1..];
                match body.find(['o', 'X', 'H', 'd']) {
                    Some(at) => (
                        body[..=at].chars().collect(),
                        Some(body[at + 1..].to_string()).filter(|v| !v.is_empty()),
                    ),
                    None => (body.chars().collect(), None),
                }
            }
            other if other.starts_with('-') && other != "-" => {
                return fail(exit::USAGE, format!("unknown option {other}"));
            }
            other => {
                if url.replace(other.to_string()).is_some() {
                    return fail(exit::USAGE, "only one URL may be given");
                }
                continue;
            }
        };
        let mut glued = glued;
        for f in flags {
            let mut value = || {
                glued
                    .take()
                    .or_else(|| it.next().cloned())
                    .ok_or_else(|| Failure {
                        code: exit::USAGE,
                        msg: format!("option -{f} needs a value"),
                    })
            };
            match f {
                'v' => cfg.verbose = true,
                's' => cfg.silent = true,
                'S' => cfg.show_error = true,
                'L' => cfg.follow = true,
                'f' => cfg.fail_on_http_error = true,
                'o' => cfg.output = Some(value()?).filter(|o| o != "-"),
                'X' => cfg.method = Some(value()?),
                'H' => cfg.headers.push(value()?),
                'd' => {
                    let more = value()?.into_bytes();
                    cfg.data = Some(match cfg.data.take() {
                        Some(mut data) => {
                            data.push(b'&');
                            data.extend(more);
                            data
                        }
                        None => more,
                    });
                }
                _ => return fail(exit::USAGE, format!("unknown option -{f}")),
            }
        }
    }
    if cfg.method.as_deref() == Some("") {
        return fail(exit::USAGE, "the request method is empty");
    }
    cfg.url = url.ok_or(Failure {
        code: exit::USAGE,
        msg: "no URL given".into(),
    })?;
    Ok(cfg)
}

fn url_failure(e: UrlError, url: &str) -> Failure {
    let (code, why) = match e {
        UrlError::UnsupportedScheme(s) => (
            exit::UNSUPPORTED_PROTOCOL,
            format!("protocol \"{s}\" not supported"),
        ),
        UrlError::NoScheme => (exit::MALFORMED_URL, "no scheme".into()),
        UrlError::Credentials => (
            exit::MALFORMED_URL,
            "credentials in a URL are not supported".into(),
        ),
        UrlError::Ipv6 => (
            exit::MALFORMED_URL,
            "IPv6 addresses are not supported".into(),
        ),
        UrlError::BadPort => (exit::MALFORMED_URL, "bad port".into()),
        UrlError::BadHost => (exit::MALFORMED_URL, "bad host".into()),
        UrlError::BadTarget => (
            exit::MALFORMED_URL,
            "whitespace or a control character in the path".into(),
        ),
    };
    Failure {
        code,
        msg: format!("{why}: {url}"),
    }
}

enum Conn<'a> {
    Plain(TcpStream),
    Tls(Box<TlsStream<'a, TcpStream>>),
}

impl Read for Conn<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

fn io_failure(e: io::Error, code: i32, what: &str) -> Failure {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Failure {
            code: exit::TIMEOUT,
            msg: format!("{what} timed out after {} seconds", IO_TIMEOUT.as_secs()),
        },
        _ => Failure {
            code,
            msg: format!("{what} failed: {e}"),
        },
    }
}

fn connect<'a>(
    url: &'a Url,
    trust: Option<&'a TrustStore>,
    verbose: bool,
) -> Result<Conn<'a>, Failure> {
    let ip = crate::net::resolve_host(&url.host).map_err(|e| Failure {
        code: exit::RESOLVE,
        msg: match e {
            ResolveError::NameNotFound => format!("could not resolve host: {}", url.host),
            other => format!("could not resolve host {}: {other}", url.host),
        },
    })?;
    let addr = SocketAddrV4::new(Ipv4Addr::from(ip.octets()), url.port);
    let tcp = TcpStream::connect(addr).map_err(|e| Failure {
        code: exit::CONNECT,
        msg: format!("failed to connect to {} port {}: {e}", url.host, url.port),
    })?;
    let _ = tcp.set_read_timeout(Some(IO_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(IO_TIMEOUT));
    if verbose {
        eprintln!("* Connected to {} ({addr})", url.host);
    }
    if url.scheme == Scheme::Http {
        return Ok(Conn::Plain(tcp));
    }
    let cfg = ClientConfig {
        server_name: &url.host,
        trust: trust.expect("trust store for https"),
        now: tls::unix_now(),
        alpn: &[b"http/1.1"],
        suites: &CipherSuite::ALL,
    };
    let stream = TlsStream::connect(tcp, cfg).map_err(|e| match e {
        ConnectError::Tls(tls::Error::Certificate(_)) => Failure {
            code: exit::PEER_UNVERIFIED,
            msg: format!("TLS: {e}"),
        },
        ConnectError::Io(io) => io_failure(io, exit::TLS_CONNECT, "TLS handshake"),
        ConnectError::Tls(_) => Failure {
            code: exit::TLS_CONNECT,
            msg: format!("TLS: {e}"),
        },
    })?;
    if verbose {
        let suite = stream.suite().map_or("?", CipherSuite::name);
        eprintln!("* TLS 1.3 connection using {suite}");
    }
    Ok(Conn::Tls(Box::new(stream)))
}

/// curl's default headers, each replaced by a user header of that name;
/// `Name:` removes one, `Name;` sends it empty, and a header with neither is dropped.
fn build_request(
    cfg: &Config,
    url: &Url,
    method: &str,
    body: Option<&[u8]>,
    headers: &[String],
) -> Vec<u8> {
    let mut defaults = vec![
        ("Host", url.host_header()),
        ("User-Agent", "SlopOS-curl/2.0".to_string()),
        ("Accept", "*/*".to_string()),
        ("Connection", "close".to_string()),
    ];
    if let Some(body) = body {
        defaults.push(("Content-Length", body.len().to_string()));
        defaults.push((
            "Content-Type",
            "application/x-www-form-urlencoded".to_string(),
        ));
    }
    let mut head = format!("{method} {} HTTP/1.1\r\n", url.target);
    for (name, value) in defaults {
        if !headers.iter().any(|h| header_named(h, name)) {
            head += &format!("{name}: {value}\r\n");
        }
    }
    for h in headers {
        match (h.split_once(':'), h.strip_suffix(';')) {
            (Some((_, value)), _) if value.trim().is_empty() => {}
            (Some(_), _) => head += &format!("{h}\r\n"),
            (None, Some(name)) => head += &format!("{name}:\r\n"),
            (None, None) => {}
        }
    }
    head += "\r\n";
    if cfg.verbose {
        for line in head.lines().filter(|l| !l.is_empty()) {
            eprintln!("> {line}");
        }
    }
    let mut req = head.into_bytes();
    req.extend_from_slice(body.unwrap_or(&[]));
    req
}

fn header_named(line: &str, name: &str) -> bool {
    line.split_once(':')
        .map(|(n, _)| n)
        .or_else(|| line.strip_suffix(';'))
        .is_some_and(|n| n.trim().eq_ignore_ascii_case(name))
}

/// `raw` is what has already arrived; returns the head and the bytes after it.
fn read_head(
    conn: &mut Conn<'_>,
    mut raw: Vec<u8>,
    verbose: bool,
) -> Result<(Head, Vec<u8>), Failure> {
    let mut buf = [0u8; 4096];
    let end = loop {
        if let Some(end) = http::head_end(&raw) {
            break end;
        }
        if raw.len() > MAX_HEAD_BYTES {
            return fail(exit::RECV, "the response head is too large");
        }
        let n = conn
            .read(&mut buf)
            .map_err(|e| io_failure(e, exit::RECV, "receiving"))?;
        if n == 0 {
            return if raw.is_empty() {
                fail(exit::EMPTY_REPLY, "empty reply from server")
            } else {
                fail(exit::RECV, "the connection closed inside the response head")
            };
        }
        raw.extend_from_slice(&buf[..n]);
    };
    let rest = raw.split_off(end);
    if verbose {
        for line in String::from_utf8_lossy(&raw)
            .split("\r\n")
            .filter(|l| !l.is_empty())
        {
            eprintln!("< {line}");
        }
    }
    let head = Head::parse(&raw).map_err(|e| Failure {
        code: match e {
            HeadError::NotHttp => exit::UNSUPPORTED_PROTOCOL,
            HeadError::BadContentLength => exit::WEIRD_SERVER_REPLY,
        },
        msg: match e {
            HeadError::NotHttp => "the reply is not HTTP".into(),
            HeadError::BadContentLength => "the reply's Content-Length is malformed".into(),
        },
    })?;
    Ok((head, rest))
}

fn stream_body(
    conn: &mut Conn<'_>,
    head: &Head,
    early: Vec<u8>,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), Failure>,
) -> Result<(), Failure> {
    let mut buf = vec![0u8; 64 * 1024];
    let recv = |conn: &mut Conn<'_>, buf: &mut [u8]| {
        conn.read(buf)
            .map_err(|e| io_failure(e, exit::RECV, "receiving"))
    };
    if head.chunked {
        let malformed = || Failure {
            code: exit::RECV,
            msg: "malformed chunked encoding".into(),
        };
        let mut chunked = Chunked::new();
        let mut input = &early[..];
        while let Some(data) = chunked.next(&mut input).map_err(|_| malformed())? {
            sink(data)?;
        }
        while !chunked.done() {
            let n = recv(conn, &mut buf)?;
            if n == 0 {
                return fail(
                    exit::PARTIAL_FILE,
                    "the connection closed inside a chunked body",
                );
            }
            let mut input = &buf[..n];
            while let Some(data) = chunked.next(&mut input).map_err(|_| malformed())? {
                sink(data)?;
            }
        }
        return Ok(());
    }
    if let Some(len) = head.content_length {
        let mut left = len;
        let first = (early.len() as u64).min(left) as usize;
        sink(&early[..first])?;
        left -= first as u64;
        while left > 0 {
            let n = recv(conn, &mut buf)?;
            if n == 0 {
                return fail(
                    exit::PARTIAL_FILE,
                    format!("the connection closed with {left} bytes of the body unsent"),
                );
            }
            let take = (n as u64).min(left) as usize;
            sink(&buf[..take])?;
            left -= take as u64;
        }
        return Ok(());
    }
    sink(&early)?;
    loop {
        let n = recv(conn, &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        sink(&buf[..n])?;
    }
}

fn open_output(cfg: &Config) -> Result<Box<dyn Write>, Failure> {
    match &cfg.output {
        Some(path) => {
            let file = File::create(path).map_err(|e| Failure {
                code: exit::WRITE,
                msg: format!("cannot create {path}: {e}"),
            })?;
            Ok(Box::new(BufWriter::with_capacity(64 * 1024, file)))
        }
        None => Ok(Box::new(BufWriter::with_capacity(64 * 1024, io::stdout()))),
    }
}

fn load_trust(cfg: &Config) -> Result<TrustStore, Failure> {
    let path = cfg.cacert.as_deref().unwrap_or(tls::SYSTEM_BUNDLE);
    tls::load_trust(path).map_err(|e| Failure {
        code: exit::CA_FILE,
        msg: format!("cannot load CA certificates from {path}: {e}"),
    })
}

fn run(cfg: &Config) -> Result<(), Failure> {
    let mut url = Url::parse(&cfg.url).map_err(|e| url_failure(e, &cfg.url))?;
    let mut method = cfg
        .method
        .clone()
        .unwrap_or_else(|| if cfg.data.is_some() { "POST" } else { "GET" }.into());
    let mut body = cfg.data.as_deref();
    let mut headers = cfg.headers.clone();
    let mut trust = None;
    let first_host = url.host.clone();

    for _ in 0..=MAX_REDIRECTS {
        if url.scheme == Scheme::Https && trust.is_none() {
            trust = Some(load_trust(cfg)?);
        }
        let mut conn = connect(&url, trust.as_ref(), cfg.verbose)?;
        let req = build_request(cfg, &url, &method, body, &headers);
        conn.write_all(&req)
            .and_then(|()| conn.flush())
            .map_err(|e| io_failure(e, exit::SEND, "sending the request"))?;
        let mut carried = Vec::new();
        let (head, early) = loop {
            let (head, rest) = read_head(&mut conn, carried, cfg.verbose)?;
            if !head.is_interim() {
                break (head, rest);
            }
            carried = rest;
        };

        if cfg.follow
            && head.is_redirect()
            && let Some(location) = &head.location
        {
            let next = url.join(location).map_err(|e| url_failure(e, location))?;
            if cfg.verbose {
                eprintln!("* Following redirect to {location}");
            }
            if matches!(head.status, 301..=303) && method == "POST" {
                method = "GET".into();
                body = None;
            }
            if !next.same_origin(&url) {
                headers.retain(|h| !header_named(h, "authorization") && !header_named(h, "cookie"));
            }
            if next.host != first_host {
                headers.retain(|h| !header_named(h, "host"));
            }
            url = next;
            continue;
        }
        if cfg.fail_on_http_error && head.status >= 400 {
            return fail(
                exit::HTTP_ERROR,
                format!("the requested URL returned error: {}", head.status),
            );
        }
        let mut out = open_output(cfg)?;
        if !head.has_body(&method) {
            return out.flush().map_err(|e| Failure {
                code: exit::WRITE,
                msg: format!("writing the body failed: {e}"),
            });
        }
        let mut sink = |bytes: &[u8]| {
            out.write_all(bytes).map_err(|e| Failure {
                code: exit::WRITE,
                msg: format!("writing the body failed: {e}"),
            })
        };
        stream_body(&mut conn, &head, early, &mut sink)?;
        if let Conn::Tls(tls) = &mut conn {
            let _ = tls.close();
        }
        return out.flush().map_err(|e| Failure {
            code: exit::WRITE,
            msg: format!("writing the body failed: {e}"),
        });
    }
    fail(
        exit::TOO_MANY_REDIRECTS,
        format!("more than {MAX_REDIRECTS} redirects"),
    )
}

fn exit_with(f: Failure, report: bool) -> ! {
    if report {
        eprintln!("curl: ({}) {}", f.code, f.msg);
        if f.code == exit::USAGE {
            eprintln!("{USAGE}");
        }
    }
    std::process::exit(f.code);
}

pub fn curl_main(args: Vec<String>) -> ! {
    process::ignore_signal(slopos_abi::signal::SIGPIPE);
    let cfg = parse_args(&args).unwrap_or_else(|f| exit_with(f, true));
    match run(&cfg) {
        Ok(()) => std::process::exit(0),
        Err(f) => exit_with(f, !cfg.silent || cfg.show_error),
    }
}

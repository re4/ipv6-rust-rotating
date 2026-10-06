use std::{fmt, net::Ipv6Addr};

#[derive(Debug)]
pub struct ParsedRequest {
    pub kind: RequestKind,
    pub proxy_authorization: Option<String>,
}

#[derive(Debug)]
pub enum RequestKind {
    Connect {
        host: String,
        port: u16,
    },
    Forward {
        host: String,
        port: u16,
        upstream_head: Vec<u8>,
    },
}

#[derive(Debug)]
pub struct ParseError(pub &'static str);

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for ParseError {}

pub fn parse_request(head: &[u8]) -> Result<ParsedRequest, ParseError> {
    let text =
        std::str::from_utf8(head).map_err(|_| ParseError("request headers are not UTF-8"))?;
    let text = text
        .strip_suffix("\r\n\r\n")
        .ok_or(ParseError("request headers are incomplete"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or(ParseError("missing request line"))?;
    let parts = request_line.split_ascii_whitespace().collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(ParseError("malformed request line"));
    }

    let method = parts[0];
    let request_target = parts[1];
    let version = parts[2];
    if !is_token(method) {
        return Err(ParseError("invalid HTTP method"));
    }
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(ParseError("only HTTP/1.0 and HTTP/1.1 are supported"));
    }

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() || line.starts_with([' ', '\t']) {
            return Err(ParseError("invalid or folded HTTP header"));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(ParseError("malformed HTTP header"))?;
        if !is_token(name) {
            return Err(ParseError("invalid HTTP header name"));
        }
        let value = value.trim_matches([' ', '\t']);
        if value
            .bytes()
            .any(|byte| (byte < 0x20 && byte != b'\t') || byte == 0x7f)
        {
            return Err(ParseError("invalid control character in HTTP header"));
        }
        headers.push((name, value));
    }

    let authorization_values = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("Proxy-Authorization"))
        .map(|(_, value)| *value)
        .collect::<Vec<_>>();
    if authorization_values.len() > 1 {
        return Err(ParseError("duplicate Proxy-Authorization header"));
    }
    let proxy_authorization = authorization_values
        .first()
        .map(|value| (*value).to_owned());

    let kind = if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = parse_authority(request_target, None)?;
        RequestKind::Connect { host, port }
    } else {
        let (host, port, origin_form) = parse_forward_target(request_target, &headers)?;
        let mut upstream_head = Vec::with_capacity(head.len());
        upstream_head.extend_from_slice(method.as_bytes());
        upstream_head.push(b' ');
        upstream_head.extend_from_slice(origin_form.as_bytes());
        upstream_head.push(b' ');
        upstream_head.extend_from_slice(version.as_bytes());
        upstream_head.extend_from_slice(b"\r\n");

        for (name, value) in &headers {
            if matches_hop_by_hop_or_replaced(name) {
                continue;
            }
            upstream_head.extend_from_slice(name.as_bytes());
            upstream_head.extend_from_slice(b": ");
            upstream_head.extend_from_slice(value.as_bytes());
            upstream_head.extend_from_slice(b"\r\n");
        }
        upstream_head.extend_from_slice(b"Host: ");
        upstream_head.extend_from_slice(format_authority(&host, port, 80).as_bytes());
        upstream_head.extend_from_slice(b"\r\nConnection: close\r\n\r\n");

        RequestKind::Forward {
            host,
            port,
            upstream_head,
        }
    };

    Ok(ParsedRequest {
        kind,
        proxy_authorization,
    })
}

fn parse_forward_target(
    target: &str,
    headers: &[(&str, &str)],
) -> Result<(String, u16, String), ParseError> {
    if target.contains('#') {
        return Err(ParseError(
            "URI fragments are not valid proxy request targets",
        ));
    }

    if target.starts_with('/') {
        let host_values = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("Host"))
            .map(|(_, value)| *value)
            .collect::<Vec<_>>();
        if host_values.len() != 1 {
            return Err(ParseError(
                "origin-form requests require exactly one Host header",
            ));
        }
        let (host, port) = parse_authority(host_values[0], Some(80))?;
        return Ok((host, port, target.to_owned()));
    }

    let (scheme, remainder) = target.split_once("://").ok_or(ParseError(
        "forward proxy requests must use an absolute http:// URI",
    ))?;
    if !scheme.eq_ignore_ascii_case("http") {
        return Err(ParseError("use CONNECT for HTTPS destinations"));
    }
    if remainder.is_empty() {
        return Err(ParseError("absolute URI has no authority"));
    }

    let boundary = remainder
        .char_indices()
        .find(|(_, character)| matches!(character, '/' | '?'))
        .map(|(index, _)| index)
        .unwrap_or(remainder.len());
    let authority = &remainder[..boundary];
    let suffix = &remainder[boundary..];
    let origin_form = if suffix.is_empty() {
        "/".to_owned()
    } else if suffix.starts_with('?') {
        format!("/{suffix}")
    } else {
        suffix.to_owned()
    };
    let (host, port) = parse_authority(authority, Some(80))?;
    Ok((host, port, origin_form))
}

pub fn parse_authority(
    authority: &str,
    default_port: Option<u16>,
) -> Result<(String, u16), ParseError> {
    if authority.is_empty() || authority.contains('@') {
        return Err(ParseError("invalid target authority"));
    }

    if let Some(rest) = authority.strip_prefix('[') {
        let close = rest
            .find(']')
            .ok_or(ParseError("unterminated IPv6 literal"))?;
        let host = &rest[..close];
        host.parse::<Ipv6Addr>()
            .map_err(|_| ParseError("invalid bracketed IPv6 literal"))?;
        let suffix = &rest[close + 1..];
        let port = if suffix.is_empty() {
            default_port.ok_or(ParseError("target port is required"))?
        } else {
            parse_explicit_port(
                suffix
                    .strip_prefix(':')
                    .ok_or(ParseError("invalid text after IPv6 literal"))?,
            )?
        };
        return Ok((host.to_owned(), port));
    }

    if authority.matches(':').count() > 1 {
        return Err(ParseError("IPv6 literals must be enclosed in brackets"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, parse_explicit_port(port)?),
        None => (
            authority,
            default_port.ok_or(ParseError("target port is required"))?,
        ),
    };
    validate_host(host)?;
    Ok((host.to_owned(), port))
}

fn parse_explicit_port(value: &str) -> Result<u16, ParseError> {
    let port = value
        .parse::<u16>()
        .map_err(|_| ParseError("invalid target port"))?;
    if port == 0 {
        return Err(ParseError("target port zero is not allowed"));
    }
    Ok(port)
}

fn validate_host(host: &str) -> Result<(), ParseError> {
    if host.is_empty() || host.len() > 253 || !host.is_ascii() {
        return Err(ParseError("invalid target host"));
    }
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return Ok(());
    }

    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return Err(ParseError("invalid target host"));
    }
    for label in host.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(ParseError("invalid target host"));
        }
    }
    Ok(())
}

fn format_authority(host: &str, port: u16, default_port: u16) -> String {
    let host = if host.parse::<Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    if port == default_port {
        host
    } else {
        format!("{host}:{port}")
    }
}

fn matches_hop_by_hop_or_replaced(name: &str) -> bool {
    [
        "Proxy-Authorization",
        "Proxy-Connection",
        "Connection",
        "Keep-Alive",
        "Host",
    ]
    .iter()
    .any(|blocked| name.eq_ignore_ascii_case(blocked))
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect_with_authentication() {
        let request = parse_request(
            b"CONNECT example.com:443 HTTP/1.1\r\nProxy-Authorization: Basic Zm9vOmJhcg==\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            request.proxy_authorization.as_deref(),
            Some("Basic Zm9vOmJhcg==")
        );
        assert!(matches!(
            request.kind,
            RequestKind::Connect { ref host, port } if host == "example.com" && port == 443
        ));
    }

    #[test]
    fn rewrites_absolute_form_and_removes_proxy_headers() {
        let request = parse_request(
            b"GET http://example.com:8080/a?q=1 HTTP/1.1\r\nHost: wrong.example\r\nProxy-Connection: keep-alive\r\nProxy-Authorization: Basic eDp5\r\nX-Test: yes\r\n\r\n",
        )
        .unwrap();
        let RequestKind::Forward { upstream_head, .. } = request.kind else {
            panic!("expected a forward request");
        };
        let rewritten = String::from_utf8(upstream_head).unwrap();
        assert!(rewritten.starts_with("GET /a?q=1 HTTP/1.1\r\n"));
        assert!(rewritten.contains("Host: example.com:8080\r\n"));
        assert!(rewritten.contains("Connection: close\r\n"));
        assert!(!rewritten.contains("Proxy-"));
        assert!(!rewritten.contains("wrong.example"));
    }

    #[test]
    fn parses_bracketed_ipv6_authority() {
        assert_eq!(
            parse_authority("[2606:4700:4700::1111]:443", None).unwrap(),
            ("2606:4700:4700::1111".into(), 443)
        );
    }
}

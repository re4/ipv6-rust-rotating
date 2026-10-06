use std::{
    collections::HashSet,
    fmt, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

#[cfg(any(target_os = "android", target_os = "linux"))]
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{
    net::{TcpSocket, TcpStream, lookup_host},
    time::timeout,
};

use crate::address_pool::AddressPool;

pub struct Connector {
    pool: Arc<AddressPool>,
    allow_ipv4_fallback: bool,
    block_private_targets: bool,
    connect_timeout: Duration,
}

#[derive(Debug)]
pub enum ConnectError {
    Resolve(io::Error),
    Blocked,
    NoIpv6,
    Pool(io::Error),
    Connect(io::Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(error) => write!(formatter, "DNS resolution failed: {error}"),
            Self::Blocked => formatter.write_str("all resolved target addresses are blocked"),
            Self::NoIpv6 => {
                formatter.write_str("target has no IPv6 address and IPv4 fallback is disabled")
            }
            Self::Pool(error) => write!(formatter, "could not select an IPv6 source: {error}"),
            Self::Connect(error) => write!(formatter, "outbound connection failed: {error}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl Connector {
    pub fn new(
        pool: Arc<AddressPool>,
        allow_ipv4_fallback: bool,
        block_private_targets: bool,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            pool,
            allow_ipv4_fallback,
            block_private_targets,
            connect_timeout,
        }
    }

    pub async fn connect(
        &self,
        host: &str,
        port: u16,
    ) -> Result<(TcpStream, Option<Ipv6Addr>), ConnectError> {
        let resolved = timeout(self.connect_timeout, lookup_host((host, port)))
            .await
            .map_err(|_| {
                ConnectError::Resolve(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "DNS resolution timed out",
                ))
            })?
            .map_err(ConnectError::Resolve)?;
        let mut seen = HashSet::new();
        let mut addresses = resolved
            .filter(|address| seen.insert(*address))
            .collect::<Vec<_>>();
        if self.block_private_targets {
            addresses.retain(|address| is_public_destination(address.ip()));
        }
        if addresses.is_empty() {
            return Err(ConnectError::Blocked);
        }

        // A rotating IPv6 path is always tried before the optional IPv4 path.
        addresses.sort_by_key(|address| if address.is_ipv6() { 0 } else { 1 });
        let has_ipv6 = addresses.iter().any(SocketAddr::is_ipv6);
        if !has_ipv6 && !self.allow_ipv4_fallback {
            return Err(ConnectError::NoIpv6);
        }

        let source = if has_ipv6 {
            Some(self.pool.next_address().map_err(ConnectError::Pool)?)
        } else {
            None
        };
        let mut last_error = None;

        for remote in addresses {
            let result = if remote.is_ipv6() {
                let source = source.expect("source exists when an IPv6 target exists");
                timeout(self.connect_timeout, connect_ipv6(remote, source)).await
            } else if self.allow_ipv4_fallback {
                timeout(self.connect_timeout, connect_ipv4(remote)).await
            } else {
                continue;
            };

            match result {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    let used_source = if remote.is_ipv6() { source } else { None };
                    return Ok((stream, used_source));
                }
                Ok(Err(error)) => last_error = Some(error),
                Err(_) => {
                    last_error = Some(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("connection to {remote} timed out"),
                    ));
                }
            }
        }

        Err(ConnectError::Connect(last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no usable target address")
        })))
    }
}

async fn connect_ipv6(remote: SocketAddr, source: Ipv6Addr) -> io::Result<TcpStream> {
    let socket = new_ipv6_socket()?;
    socket.bind(SocketAddr::new(IpAddr::V6(source), 0))?;
    socket.connect(remote).await
}

fn new_ipv6_socket() -> io::Result<TcpSocket> {
    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_nonblocking(true)?;
        socket.set_freebind_v6(true)?;
        let stream: std::net::TcpStream = socket.into();
        Ok(TcpSocket::from_std_stream(stream))
    }

    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    {
        TcpSocket::new_v6()
    }
}

async fn connect_ipv4(remote: SocketAddr) -> io::Result<TcpStream> {
    TcpSocket::new_v4()?.connect(remote).await
}

pub fn is_public_destination(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !matches!(
        (a, b, c),
        (0, _, _)
            | (10, _, _)
            | (100, 64..=127, _)
            | (127, _, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 0, 0)
            | (192, 0, 2)
            | (192, 168, _)
            | (198, 18..=19, _)
            | (198, 51, 100)
            | (203, 0, 113)
            | (224..=255, _, _)
    )
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    // Public global-unicast allocations currently live in 2000::/3. Keeping
    // this check narrow blocks loopback, ULA, link-local, multicast, mapped
    // IPv4, documentation ranges, and other special-use space.
    let segments = ip.segments();
    (segments[0] & 0xe000) == 0x2000 && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_internal_and_special_destinations() {
        for value in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "192.168.1.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(!is_public_destination(value.parse().unwrap()), "{value}");
        }
    }

    #[test]
    fn allows_public_destinations() {
        for value in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(is_public_destination(value.parse().unwrap()), "{value}");
        }
    }
}

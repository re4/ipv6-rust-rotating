use std::{
    collections::HashSet,
    env, fmt,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use ipnet::{IpNet, Ipv6Net};

pub struct Config {
    pub listen_addr: SocketAddr,
    pub ipv6_prefix: Ipv6Net,
    pub excluded_ipv6: HashSet<Ipv6Addr>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub allowed_clients: Vec<IpNet>,
    pub allow_unauthenticated: bool,
    pub allow_ipv4_fallback: bool,
    pub block_private_targets: bool,
    pub allowed_ports: PortPolicy,
    pub max_connections: usize,
    pub max_header_bytes: usize,
    pub header_timeout: Duration,
    pub connect_timeout: Duration,
    pub shutdown_grace: Duration,
}

#[derive(Clone)]
pub struct PortPolicy {
    ranges: Vec<(u16, u16)>,
    allow_all: bool,
}

impl PortPolicy {
    pub fn allows(&self, port: u16) -> bool {
        self.allow_all
            || self
                .ranges
                .iter()
                .any(|(start, end)| (*start..=*end).contains(&port))
    }

    fn parse(value: &str) -> Result<Self, ConfigError> {
        if value.trim() == "*" {
            return Ok(Self {
                ranges: Vec::new(),
                allow_all: true,
            });
        }

        let mut ranges = Vec::new();
        for item in value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            let (start, end) = match item.split_once('-') {
                Some((start, end)) => (parse_port(start)?, parse_port(end)?),
                None => {
                    let port = parse_port(item)?;
                    (port, port)
                }
            };
            if start > end {
                return Err(ConfigError(format!(
                    "invalid descending port range: {item}"
                )));
            }
            ranges.push((start, end));
        }

        if ranges.is_empty() {
            return Err(ConfigError(
                "R6P_ALLOWED_PORTS must be '*' or a comma-separated list such as 80,443,8000-8010"
                    .into(),
            ));
        }

        Ok(Self {
            ranges,
            allow_all: false,
        })
    }
}

pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ConfigError").field(&self.0).finish()
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let listen_addr = parse_env("R6P_LISTEN_ADDR", "0.0.0.0:8080")?;
        let ipv6_prefix = required("R6P_IPV6_PREFIX")?
            .parse::<Ipv6Net>()
            .map_err(|error| ConfigError(format!("invalid R6P_IPV6_PREFIX: {error}")))?;
        if ipv6_prefix.prefix_len() == 128 {
            return Err(ConfigError(
                "R6P_IPV6_PREFIX must be /127 or larger so addresses can rotate".into(),
            ));
        }

        let excluded_ipv6 = optional("R6P_EXCLUDE_IPV6")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                value.parse::<Ipv6Addr>().map_err(|error| {
                    ConfigError(format!(
                        "invalid address in R6P_EXCLUDE_IPV6 ({value}): {error}"
                    ))
                })
            })
            .collect::<Result<HashSet<_>, _>>()?;

        let allow_unauthenticated = parse_bool_env("R6P_ALLOW_UNAUTHENTICATED", false)?;
        let username = optional("R6P_USERNAME");
        let password = optional("R6P_PASSWORD");
        if !allow_unauthenticated
            && (username.as_deref().is_none_or(str::is_empty)
                || password.as_deref().is_none_or(str::is_empty))
        {
            return Err(ConfigError(
                "set non-empty R6P_USERNAME and R6P_PASSWORD (or explicitly set R6P_ALLOW_UNAUTHENTICATED=true)"
                    .into(),
            ));
        }
        if username.as_deref().is_some_and(|value| value.contains(':')) {
            return Err(ConfigError("R6P_USERNAME cannot contain ':'".into()));
        }

        let max_connections = parse_env("R6P_MAX_CONNECTIONS", "512")?;
        if max_connections == 0 {
            return Err(ConfigError(
                "R6P_MAX_CONNECTIONS must be greater than zero".into(),
            ));
        }
        let max_header_bytes = parse_env("R6P_MAX_HEADER_BYTES", "32768")?;
        if !(1024..=1_048_576).contains(&max_header_bytes) {
            return Err(ConfigError(
                "R6P_MAX_HEADER_BYTES must be between 1024 and 1048576".into(),
            ));
        }

        Ok(Self {
            listen_addr,
            ipv6_prefix,
            excluded_ipv6,
            username,
            password,
            allowed_clients: parse_client_networks(
                &optional("R6P_ALLOWED_CLIENTS").unwrap_or_default(),
            )?,
            allow_unauthenticated,
            allow_ipv4_fallback: parse_bool_env("R6P_ALLOW_IPV4_FALLBACK", false)?,
            block_private_targets: parse_bool_env("R6P_BLOCK_PRIVATE_TARGETS", true)?,
            allowed_ports: PortPolicy::parse(
                &optional("R6P_ALLOWED_PORTS").unwrap_or_else(|| "80,443".into()),
            )?,
            max_connections,
            max_header_bytes,
            header_timeout: Duration::from_secs(parse_env("R6P_HEADER_TIMEOUT_SECS", "10")?),
            connect_timeout: Duration::from_secs(parse_env("R6P_CONNECT_TIMEOUT_SECS", "10")?),
            shutdown_grace: Duration::from_secs(parse_env("R6P_SHUTDOWN_GRACE_SECS", "10")?),
        })
    }

    pub fn client_allowed(&self, address: IpAddr) -> bool {
        client_is_allowed(&self.allowed_clients, address)
    }
}

fn parse_client_networks(value: &str) -> Result<Vec<IpNet>, ConfigError> {
    if value.trim().is_empty() || value.trim() == "*" {
        return Ok(Vec::new());
    }

    value
        .split(',')
        .map(str::trim)
        .filter(|network| !network.is_empty())
        .map(|network| {
            network
                .parse::<IpNet>()
                .map(|network| network.trunc())
                .map_err(|error| {
                    ConfigError(format!(
                        "invalid network in R6P_ALLOWED_CLIENTS ({network}): {error}"
                    ))
                })
        })
        .collect()
}

fn client_is_allowed(networks: &[IpNet], address: IpAddr) -> bool {
    if networks.is_empty() || networks.iter().any(|network| network.contains(&address)) {
        return true;
    }

    // A dual-stack IPv6 listener may report an IPv4 client as ::ffff:a.b.c.d.
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .is_some_and(|address| networks.iter().any(|network| network.contains(&address))),
        IpAddr::V4(_) => false,
    }
}

fn optional(name: &str) -> Option<String> {
    env::var(name).ok()
}

fn required(name: &str) -> Result<String, ConfigError> {
    optional(name)
        .ok_or_else(|| ConfigError(format!("missing required environment variable {name}")))
}

fn parse_env<T>(name: &str, default: &str) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    optional(name)
        .unwrap_or_else(|| default.to_owned())
        .parse::<T>()
        .map_err(|error| ConfigError(format!("invalid {name}: {error}")))
}

fn parse_bool_env(name: &str, default: bool) -> Result<bool, ConfigError> {
    let Some(value) = optional(name) else {
        return Ok(default);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError(format!(
            "invalid {name}: expected true/false, yes/no, on/off, or 1/0"
        ))),
    }
}

fn parse_port(value: &str) -> Result<u16, ConfigError> {
    let port = value
        .trim()
        .parse::<u16>()
        .map_err(|error| ConfigError(format!("invalid port {value}: {error}")))?;
    if port == 0 {
        return Err(ConfigError("port zero is not allowed".into()));
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_allowlist_handles_both_ip_families() {
        let networks = parse_client_networks("198.51.100.8/32,2001:db8:1::/64").unwrap();
        assert!(client_is_allowed(
            &networks,
            "198.51.100.8".parse().unwrap()
        ));
        assert!(client_is_allowed(
            &networks,
            "::ffff:198.51.100.8".parse().unwrap()
        ));
        assert!(client_is_allowed(
            &networks,
            "2001:db8:1::42".parse().unwrap()
        ));
        assert!(!client_is_allowed(
            &networks,
            "198.51.100.9".parse().unwrap()
        ));
    }
}

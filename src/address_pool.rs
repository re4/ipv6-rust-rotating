use std::{collections::HashSet, io, net::Ipv6Addr, sync::Mutex};

use ipnet::Ipv6Net;

/// Walks the host portion of a prefix with an odd stride. Because the address
/// space is a power of two, every address is visited before the sequence wraps.
pub struct AddressPool {
    network: u128,
    host_mask: u128,
    state: Mutex<PoolState>,
    excluded: HashSet<Ipv6Addr>,
}

struct PoolState {
    next: u128,
    step: u128,
}

impl AddressPool {
    pub fn new(prefix: Ipv6Net, excluded: HashSet<Ipv6Addr>) -> io::Result<Self> {
        if prefix.prefix_len() == 128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "R6P_IPV6_PREFIX must contain at least two addresses (a /127 or larger pool)",
            ));
        }

        if let Some(address) = excluded.iter().find(|address| !prefix.contains(*address)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("excluded address {address} is outside {prefix}"),
            ));
        }

        let prefix_len = prefix.prefix_len();
        if prefix_len > 0 {
            let capacity = 1_u128 << (128 - prefix_len);
            if excluded.len() as u128 >= capacity {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "R6P_EXCLUDE_IPV6 excludes the entire configured prefix",
                ));
            }
        }
        let host_mask = u128::MAX >> prefix_len;
        let network = u128::from(prefix.network());
        let next = rand::random::<u128>() & host_mask;
        // An odd step is coprime with a power-of-two address-space size.
        let step = (rand::random::<u128>() | 1) & host_mask;

        Ok(Self {
            network,
            host_mask,
            state: Mutex::new(PoolState { next, step }),
            excluded,
        })
    }

    pub fn next_address(&self) -> io::Result<Ipv6Addr> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("IPv6 address pool lock was poisoned"))?;

        // Configuration normally excludes only one or two addresses. This cap
        // also makes a fully-excluded tiny prefix fail instead of looping.
        let attempts = self.excluded.len().saturating_add(2).max(16);
        for _ in 0..attempts {
            let host = state.next & self.host_mask;
            state.next = state.next.wrapping_add(state.step) & self.host_mask;
            let candidate = Ipv6Addr::from(self.network | host);
            if !self.excluded.contains(&candidate) {
                return Ok(candidate);
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no usable IPv6 address remains in the configured prefix",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_addresses_stay_in_prefix_and_rotate() {
        let prefix: Ipv6Net = "2001:db8:1234:5678::/120".parse().unwrap();
        let pool = AddressPool::new(prefix, HashSet::new()).unwrap();
        let first = pool.next_address().unwrap();
        let second = pool.next_address().unwrap();

        assert!(prefix.contains(&first));
        assert!(prefix.contains(&second));
        assert_ne!(first, second);
    }

    #[test]
    fn excluded_addresses_are_skipped() {
        let prefix: Ipv6Net = "2001:db8::/127".parse().unwrap();
        let excluded = HashSet::from(["2001:db8::".parse().unwrap()]);
        let pool = AddressPool::new(prefix, excluded).unwrap();

        for _ in 0..4 {
            assert_eq!(
                pool.next_address().unwrap(),
                "2001:db8::1".parse::<Ipv6Addr>().unwrap()
            );
        }
    }
}

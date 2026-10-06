# Rotating IPv6 proxy

An authenticated HTTP forward proxy for a Linux VPS. Every accepted client TCP
connection receives a different source address from your IPv6 prefix. HTTPS is
supported with the standard `CONNECT` method; ordinary `http://` proxy requests
are supported too.

The service does not alter or decrypt HTTPS traffic. Rotation is per client TCP
connection, so a client that keeps one tunnel open keeps the same egress address
until it reconnects.

## VPS recommendation

[GravHosting](https://gravhosting.com/) is our recommended VPS provider for
running this proxy. Its KVM VPS plans offer root access and locations in the US
and Europe. Before ordering, confirm that the specific plan includes a usable
IPv6 prefix and permits outbound traffic from multiple addresses in that prefix;
the proxy needs both. See [GravHosting VPS plans](https://gravhosting.com/pricing)
or ask its support team about the IPv6 allocation for your chosen plan.

## Before you deploy

Your provider must delegate an IPv6 prefix to the VPS and permit arbitrary source
addresses from it. A **routed** `/64` is the simplest setup. An address range shown
in a control panel is not necessarily routed; confirm this with the provider.

The server uses Linux `IPV6_FREEBIND`, so it does not add thousands of addresses
to an interface. The included network unit adds one local route for the prefix so
return traffic is delivered to the outbound sockets. If the prefix is directly
on-link rather than routed, the upstream router also needs Neighbor Discovery
responses; see `deploy/ndppd.conf.example` and your provider's instructions.

Do not use the `2001:db8::/32` examples in this repository. That block is reserved
for documentation and cannot reach the public Internet.

## Automatic install (Ubuntu/Debian)

Clone this project onto the VPS, then run:

```bash
git clone https://github.com/re4/ipv6-rust-rotating.git
cd ipv6-rust-rotating
sudo bash deploy/install.sh
```

The installer performs the complete setup:

- installs build and network dependencies;
- finds the default IPv6 interface;
- detects and normalizes a single visible global IPv6 prefix;
- detects the current SSH client's IP and makes it the proxy client allowlist;
- excludes IPv6 addresses already assigned to the VPS;
- generates a strong proxy password;
- builds and tests the Rust release;
- installs the binary, configuration, local-prefix route, and systemd services;
- configures `ndppd` automatically when the prefix appears directly on-link;
- starts everything and verifies that an address from the prefix reaches an
  external IPv6 address-check endpoint.

The installer prints the proxy URL and username. It saves the generated password
in `/etc/rotating-ipv6-proxy.env`, readable only by root. Open that file on the
VPS to retrieve it; never commit or share the real configuration file.

Automatic prefix discovery is intentionally conservative. If the VPS has only a
`/128`, several global prefixes, or a delegated prefix that is not present in its
local address/route tables, pass the provider-assigned block explicitly:

```bash
sudo bash deploy/install.sh \
  --prefix YOUR_DELEGATED_PREFIX/64 \
  --client-cidr YOUR_PUBLIC_CLIENT_IP/32
```

Useful options include `--interface eth0`, `--listen 0.0.0.0:8080`,
`--ipv4-fallback`, and `--ndp auto|on|off`. See every option with:

```bash
bash deploy/install.sh --help
```

Re-running the installer updates the binary and services while preserving the
existing prefix, credentials, listener, client allowlist, and exclusions unless
overridden. The previous environment file is retained as
`/etc/rotating-ipv6-proxy.env.previous`.

## Manual build and install

Install the system prerequisites and a current stable Rust toolchain, then build
on the VPS (the project requires Rust 1.85 or newer):

```bash
sudo apt-get update
sudo apt-get install -y build-essential curl iproute2
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
rustup toolchain install stable
git clone https://github.com/re4/ipv6-rust-rotating.git
cd ipv6-rust-rotating
cargo test --locked
cargo build --release --locked
```

Install the binary and service definitions:

```bash
sudo install -m 0755 target/release/rotating-ipv6-proxy /usr/local/bin/
sudo install -m 0644 deploy/rotating-ipv6-proxy.service /etc/systemd/system/
sudo install -m 0644 deploy/rotating-ipv6-proxy-network.service /etc/systemd/system/
sudo install -m 0600 deploy/rotating-ipv6-proxy.env.example /etc/rotating-ipv6-proxy.env
sudoedit /etc/rotating-ipv6-proxy.env
```

Set `R6P_IPV6_PREFIX` to the real delegated prefix. Put the VPS's already assigned
IPv6 address in `R6P_EXCLUDE_IPV6`. Generate a password, for example with
`openssl rand -hex 24`, and replace both example credentials.

Start the service:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now rotating-ipv6-proxy-network.service
sudo systemctl enable --now rotating-ipv6-proxy.service
sudo systemctl status rotating-ipv6-proxy.service
```

The binary is unprivileged; only the small one-shot network unit receives
`CAP_NET_ADMIN`, solely to install the prefix's local route.

## Firewall it before use

Authentication is required by default, but also restrict TCP port 8080 to your
own client IP in the VPS firewall or the provider firewall. For example, with
UFW, add the specific allow rule before the deny rule:

```bash
sudo ufw allow from YOUR_PUBLIC_CLIENT_IP to any port 8080 proto tcp
sudo ufw deny 8080/tcp
```

Keep your SSH rule intact and inspect `sudo ufw status numbered` before enabling
UFW remotely. Never expose the service with
`R6P_ALLOW_UNAUTHENTICATED=true` unless a separate private network or firewall
fully controls access.

HTTP Basic proxy credentials are not encrypted between the client and this
listener. Over an untrusted network, put the proxy behind WireGuard, an SSH
tunnel, or another encrypted private path in addition to the firewall rule.

## Test rotation

Run this from the allowed client machine, replacing the VPS address and login:

```bash
for attempt in 1 2 3 4 5; do
  curl --silent --show-error \
    --proxy http://VPS_IPV4:8080 \
    --proxy-user 'proxy-user:your-password' \
    https://api64.ipify.org
  echo
done
```

You should see a different address inside the configured prefix each time.
`curl` creates a new proxy TCP connection for each invocation. To inspect logs:

```bash
sudo journalctl -u rotating-ipv6-proxy.service -f
```

If every request returns `502`, first verify that the destination has an AAAA
record and then inspect the route from one usable source address:

```bash
ip -6 route get 2606:4700:4700::1111 from YOUR_PREFIX_ADDRESS
ip -6 route show table local
```

If outbound SYN packets leave but replies never arrive, the prefix is not routed
to the VPS, provider source filtering is rejecting it, or an on-link prefix needs
NDP proxying.

## Configuration

All configuration is through environment variables.

| Variable | Default | Meaning |
|---|---:|---|
| `R6P_IPV6_PREFIX` | required | Delegated IPv6 CIDR; must contain at least two addresses |
| `R6P_LISTEN_ADDR` | `0.0.0.0:8080` | Client-facing proxy listener |
| `R6P_USERNAME` / `R6P_PASSWORD` | required | HTTP Basic proxy credentials |
| `R6P_ALLOWED_CLIENTS` | unrestricted | Comma-separated client IPv4/IPv6 CIDRs; the automatic installer sets this to the current SSH client plus loopback |
| `R6P_EXCLUDE_IPV6` | empty | Comma-separated addresses the rotator must skip |
| `R6P_ALLOWED_PORTS` | `80,443` | Comma-separated ports/ranges, or `*` |
| `R6P_ALLOW_IPV4_FALLBACK` | `false` | Use the VPS's ordinary IPv4 when a target has no working IPv6; this path does not rotate |
| `R6P_BLOCK_PRIVATE_TARGETS` | `true` | Reject loopback, private, link-local, documentation, multicast, and other special-use targets after DNS resolution |
| `R6P_MAX_CONNECTIONS` | `512` | Concurrent client limit |
| `R6P_MAX_HEADER_BYTES` | `32768` | Maximum initial proxy request-header size |
| `R6P_HEADER_TIMEOUT_SECS` | `10` | Time allowed to send initial headers |
| `R6P_CONNECT_TIMEOUT_SECS` | `10` | Timeout for each resolved target address |
| `R6P_SHUTDOWN_GRACE_SECS` | `10` | Time existing tunnels may drain on shutdown |

`R6P_ALLOWED_PORTS=*`, private-target access, unauthenticated mode, and IPv4
fallback are explicit opt-ins. The defaults are intended for a public-web proxy,
not an internal network gateway.

## Operational notes

- One client TCP connection maps to one IPv6 address. HTTP clients that pool
  connections should disable pooling or reconnect when they want a new address.
- DNS is resolved on the VPS. Only resolved public destinations are attempted
  when private-target blocking is enabled, which also limits DNS-rebinding abuse.
- Plain HTTP requests are forwarded one response per client connection with
  `Connection: close`. HTTPS `CONNECT` tunnels remain open normally.
- This is a transport proxy, not an anonymity system. Logs, DNS, TLS/browser
  fingerprints, and application accounts can still identify a client.
- Use only addresses and destinations you are authorized to use, and follow the
  VPS provider's acceptable-use policy.

## License

This project is licensed under [GPL-3.0-only](LICENSE).

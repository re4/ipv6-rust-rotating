#!/usr/bin/env bash
set -Eeuo pipefail
IFS=$'\n\t'

R6P_SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
R6P_PROJECT_DIR="$(cd -- "${R6P_SCRIPT_DIR}/.." && pwd -P)"
R6P_ENV_FILE="/etc/rotating-ipv6-proxy.env"
R6P_BINARY="/usr/local/bin/rotating-ipv6-proxy"
R6P_PROXY_UNIT="/etc/systemd/system/rotating-ipv6-proxy.service"
R6P_NETWORK_UNIT="/etc/systemd/system/rotating-ipv6-proxy-network.service"

R6P_ARG_PREFIX=""
R6P_ARG_INTERFACE=""
R6P_ARG_LISTEN=""
R6P_ARG_USERNAME=""
R6P_ARG_PASSWORD=""
R6P_NDP_MODE="auto"
R6P_UNATTENDED=false
R6P_ALLOW_ANY_CLIENT=false
R6P_ENABLE_IPV4_FALLBACK=false
R6P_DISABLE_EGRESS_TEST=false
R6P_CLIENT_INPUTS=()

r6p_log() {
    printf '[rotating-ipv6-proxy] %s\n' "$*"
}

r6p_warn() {
    printf '[rotating-ipv6-proxy] WARNING: %s\n' "$*" >&2
}

r6p_die() {
    printf '[rotating-ipv6-proxy] ERROR: %s\n' "$*" >&2
    exit 1
}

r6p_usage() {
    cat <<'USAGE'
Usage: sudo ./deploy/install.sh [options]

Build, configure, and start the rotating IPv6 proxy on Debian or Ubuntu.

Options:
  --prefix CIDR          Delegated IPv6 prefix. Auto-detected when unambiguous.
  --interface NAME       IPv6 egress interface. Defaults to the default-route interface.
  --listen ADDR:PORT     Proxy listener. Default: 0.0.0.0:8080.
  --client-cidr CIDR     Allowed proxy client. Repeat for more than one client.
                         Defaults to the current SSH client's IP.
  --allow-any-client     Do not restrict clients by source IP (not recommended).
  --username NAME        Proxy username. Default: proxy-user.
  --password VALUE       Proxy password. Default: generated 48-character hex value.
  --ipv4-fallback        Allow non-rotating IPv4 egress for IPv4-only destinations.
  --ndp MODE             Neighbor Discovery proxy mode: auto, on, or off. Default: auto.
  --no-egress-test       Do not test the installed proxy against an external IPv6 endpoint.
  --unattended           Never prompt; fail when detection is ambiguous.
  -h, --help             Show this help.

Examples:
  sudo ./deploy/install.sh
  sudo ./deploy/install.sh --prefix YOUR_DELEGATED_PREFIX/64 --client-cidr YOUR_CLIENT_IP/32
USAGE
}

while (($#)); do
    case "$1" in
        --prefix)
            (($# >= 2)) || r6p_die "--prefix requires a value"
            R6P_ARG_PREFIX="$2"
            shift 2
            ;;
        --interface)
            (($# >= 2)) || r6p_die "--interface requires a value"
            R6P_ARG_INTERFACE="$2"
            shift 2
            ;;
        --listen)
            (($# >= 2)) || r6p_die "--listen requires a value"
            R6P_ARG_LISTEN="$2"
            shift 2
            ;;
        --client-cidr)
            (($# >= 2)) || r6p_die "--client-cidr requires a value"
            R6P_CLIENT_INPUTS+=("$2")
            shift 2
            ;;
        --allow-any-client)
            R6P_ALLOW_ANY_CLIENT=true
            shift
            ;;
        --username)
            (($# >= 2)) || r6p_die "--username requires a value"
            R6P_ARG_USERNAME="$2"
            shift 2
            ;;
        --password)
            (($# >= 2)) || r6p_die "--password requires a value"
            R6P_ARG_PASSWORD="$2"
            shift 2
            ;;
        --ipv4-fallback)
            R6P_ENABLE_IPV4_FALLBACK=true
            shift
            ;;
        --ndp)
            (($# >= 2)) || r6p_die "--ndp requires auto, on, or off"
            R6P_NDP_MODE="$2"
            shift 2
            ;;
        --no-egress-test)
            R6P_DISABLE_EGRESS_TEST=true
            shift
            ;;
        --unattended)
            R6P_UNATTENDED=true
            shift
            ;;
        -h|--help)
            r6p_usage
            exit 0
            ;;
        *)
            r6p_die "unknown option: $1"
            ;;
    esac
done

[[ "${EUID}" -eq 0 ]] || r6p_die "run this installer as root: sudo ./deploy/install.sh"
[[ "$(uname -s)" == "Linux" ]] || r6p_die "this installer supports Linux only"
[[ -d /run/systemd/system ]] || r6p_die "systemd is required"
[[ -f "${R6P_PROJECT_DIR}/Cargo.toml" ]] || r6p_die "run the installer from this project checkout"
[[ "${R6P_NDP_MODE}" =~ ^(auto|on|off)$ ]] || r6p_die "--ndp must be auto, on, or off"

if ! command -v apt-get >/dev/null 2>&1; then
    r6p_die "automatic package installation currently supports Debian and Ubuntu (apt-get)"
fi

r6p_log "installing required system packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential ca-certificates curl iproute2 openssl python3

r6p_read_env_value() {
    local R6P_ENV_KEY="$1"
    [[ -f "${R6P_ENV_FILE}" ]] || return 1
    awk -v key="${R6P_ENV_KEY}" 'index($0, key "=") == 1 { print substr($0, length(key) + 2); exit }' "${R6P_ENV_FILE}"
}

r6p_normalize_prefix() {
    python3 - "$1" <<'PY'
import ipaddress
import sys

raw = sys.argv[1]
try:
    network = ipaddress.ip_network(raw, strict=False)
except ValueError as error:
    print(f"invalid IPv6 prefix {raw!r}: {error}", file=sys.stderr)
    raise SystemExit(1)

if network.version != 6:
    print(f"{raw!r} is not an IPv6 prefix", file=sys.stderr)
    raise SystemExit(1)
if network.prefixlen >= 128:
    print("the rotating prefix must contain at least two addresses", file=sys.stderr)
    raise SystemExit(1)
public = ipaddress.ip_network("2000::/3")
documentation = ipaddress.ip_network("2001:db8::/32")
if not network.subnet_of(public) or network.subnet_of(documentation):
    print(f"{network} is not public global-unicast space", file=sys.stderr)
    raise SystemExit(1)
print(network)
PY
}

r6p_candidate_prefix() {
    python3 - "$1" <<'PY'
import ipaddress
import sys

try:
    network = ipaddress.ip_network(sys.argv[1], strict=False)
    public = ipaddress.ip_network("2000::/3")
    documentation = ipaddress.ip_network("2001:db8::/32")
    if (
        network.version == 6
        and network.prefixlen < 128
        and network.subnet_of(public)
        and not network.subnet_of(documentation)
    ):
        print(network)
except ValueError:
    pass
PY
}

r6p_normalize_client_network() {
    python3 - "$1" <<'PY'
import ipaddress
import sys

raw = sys.argv[1].strip()
try:
    if "/" not in raw:
        address = ipaddress.ip_address(raw)
        raw = f"{address}/{32 if address.version == 4 else 128}"
    print(ipaddress.ip_network(raw, strict=False))
except ValueError as error:
    print(f"invalid client IP/CIDR {sys.argv[1]!r}: {error}", file=sys.stderr)
    raise SystemExit(1)
PY
}

r6p_address_if_inside() {
    python3 - "$1" "$2" <<'PY'
import ipaddress
import sys

network = ipaddress.ip_network(sys.argv[1], strict=False)
address = ipaddress.ip_interface(sys.argv[2]).ip
if address in network:
    print(address)
PY
}

r6p_validate_listen() {
    python3 - "$1" <<'PY'
import ipaddress
import sys

value = sys.argv[1]
try:
    if value.startswith("["):
        close = value.index("]")
        host = value[1:close]
        if value[close + 1:close + 2] != ":":
            raise ValueError("missing port")
        port = int(value[close + 2:])
    else:
        host, raw_port = value.rsplit(":", 1)
        port = int(raw_port)
    ipaddress.ip_address(host)
    if not 1 <= port <= 65535:
        raise ValueError("port must be between 1 and 65535")
except (ValueError, IndexError) as error:
    print(f"invalid listen address {value!r}: {error}", file=sys.stderr)
    raise SystemExit(1)
print(port)
PY
}

R6P_INTERFACE="${R6P_ARG_INTERFACE}"
if [[ -z "${R6P_INTERFACE}" ]]; then
    R6P_INTERFACE="$(ip -6 route show default | awk '{ for (i = 1; i <= NF; i++) if ($i == "dev") { print $(i + 1); exit } }')"
fi
[[ -n "${R6P_INTERFACE}" ]] || r6p_die "no default IPv6 route was found; pass --interface after configuring IPv6"
ip link show dev "${R6P_INTERFACE}" >/dev/null 2>&1 || r6p_die "network interface ${R6P_INTERFACE} does not exist"
r6p_log "using IPv6 interface ${R6P_INTERFACE}"

R6P_PREFIX_SOURCE="argument"
R6P_PREFIX="${R6P_ARG_PREFIX}"
if [[ -z "${R6P_PREFIX}" ]]; then
    R6P_PREFIX="$(r6p_read_env_value R6P_IPV6_PREFIX || true)"
    R6P_PREFIX_SOURCE="existing configuration"
fi

if [[ -z "${R6P_PREFIX}" ]]; then
    R6P_PREFIX_SOURCE="automatic detection"
    declare -A R6P_SEEN_PREFIXES=()
    R6P_PREFIX_CANDIDATES=()
    while read -r R6P_RAW_CIDR; do
        [[ -n "${R6P_RAW_CIDR}" ]] || continue
        R6P_CANDIDATE="$(r6p_candidate_prefix "${R6P_RAW_CIDR}")"
        [[ -n "${R6P_CANDIDATE}" ]] || continue
        if [[ -z "${R6P_SEEN_PREFIXES[${R6P_CANDIDATE}]+present}" ]]; then
            R6P_SEEN_PREFIXES["${R6P_CANDIDATE}"]=1
            R6P_PREFIX_CANDIDATES+=("${R6P_CANDIDATE}")
        fi
    done < <(
        ip -6 -o addr show dev "${R6P_INTERFACE}" scope global | awk '{ print $4 }'
        ip -6 -o route show table main | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /:/ && $i ~ /\//) print $i }'
        ip -6 -o route show table local | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /:/ && $i ~ /\//) print $i }'
    )

    if ((${#R6P_PREFIX_CANDIDATES[@]} == 1)); then
        R6P_PREFIX="${R6P_PREFIX_CANDIDATES[0]}"
    elif ((${#R6P_PREFIX_CANDIDATES[@]} == 0)); then
        r6p_die "no usable delegated prefix was visible. Pass it explicitly with --prefix YOUR_IPV6_SUBNET/CIDR"
    elif [[ "${R6P_UNATTENDED}" == true || ! -t 0 ]]; then
        printf 'Detected multiple possible prefixes:\n' >&2
        printf '  %s\n' "${R6P_PREFIX_CANDIDATES[@]}" >&2
        r6p_die "prefix detection is ambiguous; rerun with --prefix"
    else
        printf 'Detected multiple possible IPv6 prefixes:\n'
        for R6P_INDEX in "${!R6P_PREFIX_CANDIDATES[@]}"; do
            printf '  %d) %s\n' "$((R6P_INDEX + 1))" "${R6P_PREFIX_CANDIDATES[${R6P_INDEX}]}"
        done
        read -r -p 'Choose a prefix number: ' R6P_CHOICE
        [[ "${R6P_CHOICE}" =~ ^[0-9]+$ ]] || r6p_die "invalid prefix selection"
        ((R6P_CHOICE >= 1 && R6P_CHOICE <= ${#R6P_PREFIX_CANDIDATES[@]})) || r6p_die "prefix selection is out of range"
        R6P_PREFIX="${R6P_PREFIX_CANDIDATES[$((R6P_CHOICE - 1))]}"
    fi
fi
R6P_PREFIX="$(r6p_normalize_prefix "${R6P_PREFIX}")"
r6p_log "using IPv6 prefix ${R6P_PREFIX} (${R6P_PREFIX_SOURCE})"

R6P_LISTEN="${R6P_ARG_LISTEN}"
if [[ -z "${R6P_LISTEN}" ]]; then
    R6P_LISTEN="$(r6p_read_env_value R6P_LISTEN_ADDR || true)"
fi
R6P_LISTEN="${R6P_LISTEN:-0.0.0.0:8080}"
R6P_LISTEN_PORT="$(r6p_validate_listen "${R6P_LISTEN}")"

R6P_USERNAME="${R6P_ARG_USERNAME}"
if [[ -z "${R6P_USERNAME}" ]]; then
    R6P_USERNAME="$(r6p_read_env_value R6P_USERNAME || true)"
fi
R6P_USERNAME="${R6P_USERNAME:-proxy-user}"
[[ "${R6P_USERNAME}" =~ ^[A-Za-z0-9._-]+$ ]] || r6p_die "username may contain only letters, numbers, dot, underscore, and hyphen"

R6P_PASSWORD="${R6P_ARG_PASSWORD}"
if [[ -z "${R6P_PASSWORD}" ]]; then
    R6P_PASSWORD="$(r6p_read_env_value R6P_PASSWORD || true)"
fi
if [[ -z "${R6P_PASSWORD}" || "${R6P_PASSWORD}" == "replace-with-a-long-random-password" ]]; then
    R6P_PASSWORD="$(openssl rand -hex 24)"
fi
[[ "${R6P_PASSWORD}" =~ ^[A-Za-z0-9._~-]+$ ]] || r6p_die "password may contain only letters, numbers, dot, underscore, tilde, and hyphen"

R6P_ALLOWED_CLIENTS=""
if [[ "${R6P_ALLOW_ANY_CLIENT}" == false && ${#R6P_CLIENT_INPUTS[@]} -eq 0 ]]; then
    R6P_ALLOWED_CLIENTS="$(r6p_read_env_value R6P_ALLOWED_CLIENTS || true)"
    if [[ "${R6P_ALLOWED_CLIENTS}" == "*" ]]; then
        R6P_ALLOW_ANY_CLIENT=true
        R6P_ALLOWED_CLIENTS=""
    elif [[ -n "${R6P_ALLOWED_CLIENTS}" ]]; then
        IFS=',' read -r -a R6P_CLIENT_INPUTS <<<"${R6P_ALLOWED_CLIENTS}"
        IFS=$'\n\t'
        R6P_ALLOWED_CLIENTS=""
    fi
fi

if [[ "${R6P_ALLOW_ANY_CLIENT}" == false && -z "${R6P_ALLOWED_CLIENTS}" && ${#R6P_CLIENT_INPUTS[@]} -eq 0 ]]; then
    R6P_SSH_CONNECTION="${SSH_CONNECTION:-}"
    R6P_SSH_CLIENT="${R6P_SSH_CONNECTION%% *}"
    if [[ -n "${R6P_SSH_CONNECTION}" && -n "${R6P_SSH_CLIENT}" ]]; then
        R6P_CLIENT_INPUTS+=("${R6P_SSH_CLIENT}")
        r6p_log "restricting clients to current SSH source ${R6P_SSH_CLIENT}"
    elif [[ "${R6P_UNATTENDED}" == true || ! -t 0 ]]; then
        r6p_die "could not detect an SSH client; pass --client-cidr or explicitly pass --allow-any-client"
    else
        read -r -p 'Allowed client IP/CIDR (enter "any" to allow all): ' R6P_CLIENT_ANSWER
        if [[ "${R6P_CLIENT_ANSWER}" == "any" ]]; then
            R6P_ALLOW_ANY_CLIENT=true
        elif [[ -n "${R6P_CLIENT_ANSWER}" ]]; then
            R6P_CLIENT_INPUTS+=("${R6P_CLIENT_ANSWER}")
        else
            r6p_die "a client IP/CIDR is required"
        fi
    fi
fi

if [[ "${R6P_ALLOW_ANY_CLIENT}" == false ]]; then
    R6P_CLIENT_INPUTS+=("127.0.0.1/32" "::1/128")
    declare -A R6P_SEEN_CLIENTS=()
    R6P_NORMALIZED_CLIENTS=()
    for R6P_CLIENT_INPUT in "${R6P_CLIENT_INPUTS[@]}"; do
        R6P_NORMALIZED_CLIENT="$(r6p_normalize_client_network "${R6P_CLIENT_INPUT}")"
        if [[ -z "${R6P_SEEN_CLIENTS[${R6P_NORMALIZED_CLIENT}]+present}" ]]; then
            R6P_SEEN_CLIENTS["${R6P_NORMALIZED_CLIENT}"]=1
            R6P_NORMALIZED_CLIENTS+=("${R6P_NORMALIZED_CLIENT}")
        fi
    done
    R6P_ALLOWED_CLIENTS="$(IFS=,; printf '%s' "${R6P_NORMALIZED_CLIENTS[*]}")"
fi

if [[ "${R6P_ALLOW_ANY_CLIENT}" == true ]]; then
    R6P_ALLOWED_CLIENTS="*"
    r6p_warn "client source filtering is disabled; use a provider firewall"
else
    r6p_log "client allowlist: ${R6P_ALLOWED_CLIENTS}"
fi

declare -A R6P_EXCLUDED_ADDRESSES=()
R6P_EXISTING_EXCLUDES="$(r6p_read_env_value R6P_EXCLUDE_IPV6 || true)"
if [[ -n "${R6P_EXISTING_EXCLUDES}" ]]; then
    IFS=',' read -r -a R6P_EXISTING_EXCLUDE_ARRAY <<<"${R6P_EXISTING_EXCLUDES}"
    IFS=$'\n\t'
    for R6P_ADDRESS in "${R6P_EXISTING_EXCLUDE_ARRAY[@]}"; do
        [[ -n "${R6P_ADDRESS}" ]] || continue
        R6P_ADDRESS="$(r6p_address_if_inside "${R6P_PREFIX}" "${R6P_ADDRESS}")"
        [[ -n "${R6P_ADDRESS}" ]] && R6P_EXCLUDED_ADDRESSES["${R6P_ADDRESS}"]=1
    done
fi
while read -r R6P_ASSIGNED_CIDR; do
    [[ -n "${R6P_ASSIGNED_CIDR}" ]] || continue
    R6P_ADDRESS="$(r6p_address_if_inside "${R6P_PREFIX}" "${R6P_ASSIGNED_CIDR}")"
    [[ -n "${R6P_ADDRESS}" ]] && R6P_EXCLUDED_ADDRESSES["${R6P_ADDRESS}"]=1
done < <(ip -6 -o addr show scope global | awk '{ print $4 }')
R6P_EXCLUDE_IPV6=""
if ((${#R6P_EXCLUDED_ADDRESSES[@]})); then
    R6P_EXCLUDE_IPV6="$(IFS=,; printf '%s' "${!R6P_EXCLUDED_ADDRESSES[*]}")"
fi

R6P_IPV4_FALLBACK="$(r6p_read_env_value R6P_ALLOW_IPV4_FALLBACK || true)"
R6P_IPV4_FALLBACK="${R6P_IPV4_FALLBACK:-false}"
if [[ "${R6P_ENABLE_IPV4_FALLBACK}" == true ]]; then
    R6P_IPV4_FALLBACK=true
fi

R6P_CONNECTED_PREFIX=false
if ip -6 route show table main exact "${R6P_PREFIX}" 2>/dev/null \
    | awk -v wanted_dev="${R6P_INTERFACE}" '
        {
            has_dev = 0
            has_via = 0
            for (i = 1; i <= NF; i++) {
                if ($i == "dev" && $(i + 1) == wanted_dev) has_dev = 1
                if ($i == "via") has_via = 1
            }
            if (has_dev && !has_via) found = 1
        }
        END { exit(found ? 0 : 1) }
    '; then
    R6P_CONNECTED_PREFIX=true
fi
R6P_ENABLE_NDP=false
case "${R6P_NDP_MODE}" in
    on) R6P_ENABLE_NDP=true ;;
    off) R6P_ENABLE_NDP=false ;;
    auto) R6P_ENABLE_NDP="${R6P_CONNECTED_PREFIX}" ;;
esac

R6P_NDPPD_CONFIG="/etc/ndppd.conf"
if [[ "${R6P_ENABLE_NDP}" == true && -f "${R6P_NDPPD_CONFIG}" ]] \
    && ! grep -q 'Managed by rotating-ipv6-proxy' "${R6P_NDPPD_CONFIG}"; then
    r6p_die "${R6P_NDPPD_CONFIG} already exists and is not managed by this installer; merge deploy/ndppd.conf.example manually or rerun with --ndp off if the prefix is routed"
fi

R6P_CARGO_BIN="$(command -v cargo || true)"
R6P_NEEDS_TOOLCHAIN=false
if [[ -z "${R6P_CARGO_BIN}" ]] || ! R6P_CARGO_DETAILS="$("${R6P_CARGO_BIN}" --version 2>/dev/null)"; then
    R6P_NEEDS_TOOLCHAIN=true
else
    R6P_CARGO_VERSION="$(awk '{ print $2 }' <<<"${R6P_CARGO_DETAILS}")"
    if [[ "${R6P_CARGO_VERSION}" =~ ^([0-9]+)\.([0-9]+)\. ]]; then
        if ((BASH_REMATCH[1] < 1 || (BASH_REMATCH[1] == 1 && BASH_REMATCH[2] < 85))); then
            R6P_NEEDS_TOOLCHAIN=true
        fi
    else
        R6P_NEEDS_TOOLCHAIN=true
    fi
fi

if [[ "${R6P_NEEDS_TOOLCHAIN}" == true ]]; then
    R6P_TOOLCHAIN_ROOT="/opt/rotating-ipv6-proxy-toolchain"
    R6P_RUSTUP_HOME="${R6P_TOOLCHAIN_ROOT}/rustup"
    R6P_CARGO_HOME="${R6P_TOOLCHAIN_ROOT}/cargo"
    install -d -m 0755 "${R6P_RUSTUP_HOME}" "${R6P_CARGO_HOME}"
    r6p_log "installing a current Rust toolchain under ${R6P_TOOLCHAIN_ROOT}"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | env RUSTUP_HOME="${R6P_RUSTUP_HOME}" CARGO_HOME="${R6P_CARGO_HOME}" \
            sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable
    R6P_CARGO_BIN="${R6P_CARGO_HOME}/bin/cargo"
fi

R6P_BUILD_TARGET="$(mktemp -d /tmp/r6p-build.XXXXXXXX)"
r6p_cleanup() {
    if [[ -n "${R6P_BUILD_TARGET:-}" && -d "${R6P_BUILD_TARGET}" && "${R6P_BUILD_TARGET}" == /tmp/r6p-build.* ]]; then
        rm -rf -- "${R6P_BUILD_TARGET}"
    fi
}
trap r6p_cleanup EXIT

r6p_log "testing and building the Rust service"
if [[ "${R6P_NEEDS_TOOLCHAIN}" == true ]]; then
    RUSTUP_HOME="${R6P_RUSTUP_HOME}" CARGO_HOME="${R6P_CARGO_HOME}" \
        CARGO_TARGET_DIR="${R6P_BUILD_TARGET}" "${R6P_CARGO_BIN}" test \
        --manifest-path "${R6P_PROJECT_DIR}/Cargo.toml" --locked --quiet
    RUSTUP_HOME="${R6P_RUSTUP_HOME}" CARGO_HOME="${R6P_CARGO_HOME}" \
        CARGO_TARGET_DIR="${R6P_BUILD_TARGET}" "${R6P_CARGO_BIN}" build \
        --manifest-path "${R6P_PROJECT_DIR}/Cargo.toml" --release --locked --quiet
else
    CARGO_TARGET_DIR="${R6P_BUILD_TARGET}" "${R6P_CARGO_BIN}" test \
        --manifest-path "${R6P_PROJECT_DIR}/Cargo.toml" --locked --quiet
    CARGO_TARGET_DIR="${R6P_BUILD_TARGET}" "${R6P_CARGO_BIN}" build \
        --manifest-path "${R6P_PROJECT_DIR}/Cargo.toml" --release --locked --quiet
fi

install -m 0755 "${R6P_BUILD_TARGET}/release/rotating-ipv6-proxy" "${R6P_BINARY}"
install -m 0644 "${R6P_SCRIPT_DIR}/rotating-ipv6-proxy.service" "${R6P_PROXY_UNIT}"
install -m 0644 "${R6P_SCRIPT_DIR}/rotating-ipv6-proxy-network.service" "${R6P_NETWORK_UNIT}"

R6P_ENV_TEMP="$(mktemp /etc/rotating-ipv6-proxy.env.XXXXXXXX)"
chmod 0600 "${R6P_ENV_TEMP}"
cat >"${R6P_ENV_TEMP}" <<EOF
# Generated by deploy/install.sh. Re-run the installer after changing the prefix.
R6P_IPV6_PREFIX=${R6P_PREFIX}
R6P_EXCLUDE_IPV6=${R6P_EXCLUDE_IPV6}
R6P_NETWORK_INTERFACE=${R6P_INTERFACE}
R6P_LISTEN_ADDR=${R6P_LISTEN}
R6P_USERNAME=${R6P_USERNAME}
R6P_PASSWORD=${R6P_PASSWORD}
R6P_ALLOWED_CLIENTS=${R6P_ALLOWED_CLIENTS}
R6P_ALLOWED_PORTS=80,443
R6P_ALLOW_IPV4_FALLBACK=${R6P_IPV4_FALLBACK}
R6P_BLOCK_PRIVATE_TARGETS=true
R6P_MAX_CONNECTIONS=512
R6P_MAX_HEADER_BYTES=32768
R6P_HEADER_TIMEOUT_SECS=10
R6P_CONNECT_TIMEOUT_SECS=10
R6P_SHUTDOWN_GRACE_SECS=10
EOF
if [[ -f "${R6P_ENV_FILE}" ]]; then
    install -m 0600 "${R6P_ENV_FILE}" "${R6P_ENV_FILE}.previous"
fi
mv -f -- "${R6P_ENV_TEMP}" "${R6P_ENV_FILE}"
chmod 0600 "${R6P_ENV_FILE}"

if [[ "${R6P_ENABLE_NDP}" == true ]]; then
    r6p_log "connected/on-link prefix detected; configuring ndppd on ${R6P_INTERFACE}"
    apt-get install -y -qq ndppd
    R6P_NDPPD_TEMP="$(mktemp /etc/ndppd.conf.XXXXXXXX)"
    cat >"${R6P_NDPPD_TEMP}" <<EOF
# Managed by rotating-ipv6-proxy deploy/install.sh
proxy ${R6P_INTERFACE} {
    router yes
    timeout 500
    ttl 30000

    rule ${R6P_PREFIX} {
        static
    }
}
EOF
    chmod 0644 "${R6P_NDPPD_TEMP}"
    mv -f -- "${R6P_NDPPD_TEMP}" "${R6P_NDPPD_CONFIG}"
    systemctl enable ndppd.service >/dev/null
    systemctl restart ndppd.service
    if ! systemctl is-active --quiet ndppd.service; then
        journalctl -u ndppd.service -n 30 --no-pager >&2 || true
        r6p_die "ndppd did not start"
    fi
else
    r6p_log "NDP proxying is not required by the selected mode (${R6P_NDP_MODE})"
    if [[ -f "${R6P_NDPPD_CONFIG}" ]] \
        && grep -q 'Managed by rotating-ipv6-proxy' "${R6P_NDPPD_CONFIG}" \
        && systemctl list-unit-files ndppd.service >/dev/null 2>&1; then
        systemctl disable --now ndppd.service >/dev/null || true
    fi
fi

r6p_log "enabling network and proxy services"
systemctl daemon-reload
systemctl enable rotating-ipv6-proxy-network.service >/dev/null
systemctl restart rotating-ipv6-proxy-network.service
systemctl enable rotating-ipv6-proxy.service >/dev/null
systemctl restart rotating-ipv6-proxy.service

if ! systemctl is-active --quiet rotating-ipv6-proxy-network.service; then
    journalctl -u rotating-ipv6-proxy-network.service -n 30 --no-pager >&2 || true
    r6p_die "the subnet route service did not start"
fi
if ! systemctl is-active --quiet rotating-ipv6-proxy.service; then
    journalctl -u rotating-ipv6-proxy.service -n 30 --no-pager >&2 || true
    r6p_die "the proxy service did not start"
fi

# Type=simple services become "active" just before ExecStart finishes binding.
# Wait for the socket so a fast installer run cannot report a false failure.
R6P_LISTENER_READY=false
for ((R6P_WAIT_ATTEMPT = 0; R6P_WAIT_ATTEMPT < 40; R6P_WAIT_ATTEMPT++)); do
    if ss -H -ltn "sport = :${R6P_LISTEN_PORT}" | grep -q .; then
        R6P_LISTENER_READY=true
        break
    fi
    sleep 0.25
done
if [[ "${R6P_LISTENER_READY}" == false ]]; then
    journalctl -u rotating-ipv6-proxy.service -n 30 --no-pager >&2 || true
    r6p_die "proxy service is active but did not listen on port ${R6P_LISTEN_PORT}"
fi

R6P_EGRESS_OK=unknown
if [[ "${R6P_DISABLE_EGRESS_TEST}" == false ]]; then
    R6P_LOCAL_PROXY=""
    case "${R6P_LISTEN}" in
        0.0.0.0:*|127.0.0.1:*) R6P_LOCAL_PROXY="http://127.0.0.1:${R6P_LISTEN_PORT}" ;;
        \[::\]:*|\[::1\]:*) R6P_LOCAL_PROXY="http://[::1]:${R6P_LISTEN_PORT}" ;;
    esac
    if [[ -n "${R6P_LOCAL_PROXY}" ]]; then
        r6p_log "checking rotating IPv6 egress"
        R6P_EGRESS_ADDRESS=""
        if R6P_CURL_RESULT="$(
            curl --noproxy '' --silent --show-error --fail \
                --retry 3 --retry-delay 1 --retry-connrefused --retry-all-errors --max-time 20 \
                --proxy "${R6P_LOCAL_PROXY}" \
                --proxy-user "${R6P_USERNAME}:${R6P_PASSWORD}" \
                https://api64.ipify.org 2>&1
        )"; then
            R6P_EGRESS_ADDRESS="${R6P_CURL_RESULT}"
        else
            r6p_warn "proxy egress request failed: ${R6P_CURL_RESULT}"
        fi
        if python3 - "${R6P_PREFIX}" "${R6P_EGRESS_ADDRESS}" <<'PY'
import ipaddress
import sys

try:
    raise SystemExit(0 if ipaddress.ip_address(sys.argv[2].strip()) in ipaddress.ip_network(sys.argv[1]) else 1)
except ValueError:
    raise SystemExit(1)
PY
        then
            R6P_EGRESS_OK=true
            r6p_log "egress check passed with ${R6P_EGRESS_ADDRESS}"
        else
            R6P_EGRESS_OK=false
            r6p_warn "the service is running, but the external IPv6 egress check failed"
            journalctl -u rotating-ipv6-proxy.service -n 15 --no-pager >&2 || true
        fi
    else
        r6p_warn "skipping egress test for non-wildcard listen address ${R6P_LISTEN}"
    fi
fi

R6P_ACCESS_ADDRESS="$(
    ip -4 route get 1.1.1.1 2>/dev/null \
        | awk '{ for (i = 1; i <= NF; i++) if ($i == "src") { print $(i + 1); exit } }' \
        || true
)"
R6P_ACCESS_ADDRESS="${R6P_ACCESS_ADDRESS:-YOUR_VPS_IP}"

printf '\nInstallation complete.\n'
printf '  Proxy URL:       http://%s:%s\n' "${R6P_ACCESS_ADDRESS}" "${R6P_LISTEN_PORT}"
printf '  Username:        %s\n' "${R6P_USERNAME}"
printf '  Password:        saved in %s (root only)\n' "${R6P_ENV_FILE}"
printf '  IPv6 prefix:     %s\n' "${R6P_PREFIX}"
printf '  IPv6 interface:  %s\n' "${R6P_INTERFACE}"
printf '  Allowed clients: %s\n' "${R6P_ALLOWED_CLIENTS}"
printf '  NDP proxy:       %s\n' "${R6P_ENABLE_NDP}"
printf '  Configuration:   %s\n' "${R6P_ENV_FILE}"
printf '\nOpen the configuration file on the VPS to retrieve the password. View logs with:\n'
printf '  journalctl -u rotating-ipv6-proxy.service -f\n'

if [[ "${R6P_EGRESS_OK}" == false ]]; then
    r6p_die "installation succeeded but subnet egress is not working; check provider routing/source filtering and the service logs above"
fi

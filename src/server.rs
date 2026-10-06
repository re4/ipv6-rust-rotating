use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
    time::timeout,
};
use tracing::{debug, info, warn};

use crate::{
    address_pool::AddressPool,
    auth::Authenticator,
    config::Config,
    http::{RequestKind, parse_request},
    outbound::{ConnectError, Connector, is_public_destination},
};

struct State {
    auth: Authenticator,
    connector: Connector,
    config: Config,
}

pub async fn run(config: Config) -> io::Result<()> {
    let pool = Arc::new(AddressPool::new(
        config.ipv6_prefix,
        config.excluded_ipv6.clone(),
    )?);
    let auth = if config.allow_unauthenticated {
        Authenticator::disabled()
    } else {
        Authenticator::required(
            config.username.as_deref().expect("validated username"),
            config.password.as_deref().expect("validated password"),
        )
    };
    let connector = Connector::new(
        Arc::clone(&pool),
        config.allow_ipv4_fallback,
        config.block_private_targets,
        config.connect_timeout,
    );
    let listener = TcpListener::bind(config.listen_addr).await?;
    let permits = Arc::new(Semaphore::new(config.max_connections));
    let state = Arc::new(State {
        auth,
        connector,
        config,
    });

    info!(
        listen = %state.config.listen_addr,
        prefix = %state.config.ipv6_prefix,
        max_connections = state.config.max_connections,
        ipv4_fallback = state.config.allow_ipv4_fallback,
        "proxy is ready"
    );
    if state.config.allow_unauthenticated {
        warn!("authentication is disabled; restrict the listener with a firewall");
    }
    if !is_public_destination(IpAddr::V6(state.config.ipv6_prefix.network())) {
        warn!(
            prefix = %state.config.ipv6_prefix,
            "configured source prefix does not appear to be public global-unicast space"
        );
    }

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let mut tasks = JoinSet::new();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut client, peer) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => {
                        warn!(%error, "failed to accept client");
                        continue;
                    }
                };
                if !state.config.client_allowed(peer.ip()) {
                    warn!(client = %peer, "client is outside R6P_ALLOWED_CLIENTS");
                    continue;
                }
                let _ = client.set_nodelay(true);

                let permit = match Arc::clone(&permits).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tasks.spawn(async move {
                            let _ = send_plain_response(
                                &mut client,
                                "503 Service Unavailable",
                                &[],
                            ).await;
                        });
                        continue;
                    }
                };

                let state = Arc::clone(&state);
                tasks.spawn(async move {
                    if let Err(error) = handle_client(client, peer, state, permit).await {
                        debug!(client = %peer, %error, "client connection ended with an error");
                    }
                });
            }
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result {
                    warn!(%error, "client task panicked");
                }
            }
            _ = &mut shutdown => {
                info!("shutdown signal received");
                break;
            }
        }
    }

    drop(listener);
    let grace = state.config.shutdown_grace;
    let drained = timeout(grace, async {
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                warn!(%error, "client task panicked during shutdown");
            }
        }
    })
    .await;
    if drained.is_err() {
        warn!(
            ?grace,
            remaining = tasks.len(),
            "shutdown grace expired; closing tunnels"
        );
        tasks.abort_all();
    }

    Ok(())
}

async fn handle_client(
    mut client: TcpStream,
    peer: SocketAddr,
    state: Arc<State>,
    _permit: OwnedSemaphorePermit,
) -> io::Result<()> {
    let buffered = match read_headers(
        &mut client,
        state.config.max_header_bytes,
        state.config.header_timeout,
    )
    .await
    {
        Ok(buffered) => buffered,
        Err(HeaderReadError::TimedOut) => {
            send_plain_response(&mut client, "408 Request Timeout", &[]).await?;
            return Ok(());
        }
        Err(HeaderReadError::TooLarge) => {
            send_plain_response(&mut client, "431 Request Header Fields Too Large", &[]).await?;
            return Ok(());
        }
        Err(HeaderReadError::Closed) => return Ok(()),
        Err(HeaderReadError::Io(error)) => return Err(error),
    };
    let (head, already_read) = buffered.bytes.split_at(buffered.header_end);
    let request = match parse_request(head) {
        Ok(request) => request,
        Err(error) => {
            debug!(client = %peer, %error, "rejected malformed proxy request");
            send_plain_response(&mut client, "400 Bad Request", &[]).await?;
            return Ok(());
        }
    };

    if !state
        .auth
        .is_authorized(request.proxy_authorization.as_deref())
    {
        warn!(client = %peer, "proxy authentication failed");
        send_plain_response(
            &mut client,
            "407 Proxy Authentication Required",
            &[("Proxy-Authenticate", "Basic realm=\"rotating-ipv6-proxy\"")],
        )
        .await?;
        return Ok(());
    }

    let (host, port) = match &request.kind {
        RequestKind::Connect { host, port }
        | RequestKind::Forward {
            host,
            port,
            upstream_head: _,
        } => (host.clone(), *port),
    };
    if !state.config.allowed_ports.allows(port) {
        warn!(client = %peer, target = %host, port, "target port is not allowed");
        send_plain_response(&mut client, "403 Forbidden", &[]).await?;
        return Ok(());
    }

    let (mut upstream, source) = match state.connector.connect(&host, port).await {
        Ok(connection) => connection,
        Err(error) => {
            warn!(client = %peer, target = %host, port, %error, "outbound connection failed");
            let status = if matches!(error, ConnectError::Blocked) {
                "403 Forbidden"
            } else {
                "502 Bad Gateway"
            };
            send_plain_response(&mut client, status, &[]).await?;
            return Ok(());
        }
    };

    match request.kind {
        RequestKind::Connect { .. } => {
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;
            if !already_read.is_empty() {
                upstream.write_all(already_read).await?;
            }
        }
        RequestKind::Forward { upstream_head, .. } => {
            upstream.write_all(&upstream_head).await?;
            if !already_read.is_empty() {
                upstream.write_all(already_read).await?;
            }
        }
    }

    let source_label = source
        .map(|address| address.to_string())
        .unwrap_or_else(|| "IPv4 fallback".to_owned());
    info!(
        client = %peer,
        target = %host,
        port,
        ipv6_source = %source_label,
        "proxy tunnel opened"
    );
    let (client_to_target, target_to_client) =
        copy_bidirectional(&mut client, &mut upstream).await?;
    debug!(
        client = %peer,
        target = %host,
        port,
        client_to_target,
        target_to_client,
        "proxy tunnel closed"
    );
    Ok(())
}

struct BufferedHeaders {
    bytes: Vec<u8>,
    header_end: usize,
}

enum HeaderReadError {
    TimedOut,
    TooLarge,
    Closed,
    Io(io::Error),
}

async fn read_headers(
    stream: &mut TcpStream,
    max_bytes: usize,
    deadline: std::time::Duration,
) -> Result<BufferedHeaders, HeaderReadError> {
    let operation = async {
        let mut bytes = Vec::with_capacity(max_bytes.min(8192));
        let mut chunk = [0_u8; 4096];
        loop {
            let read = stream.read(&mut chunk).await.map_err(HeaderReadError::Io)?;
            if read == 0 {
                return Err(HeaderReadError::Closed);
            }
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let header_end = position + 4;
                if header_end > max_bytes {
                    return Err(HeaderReadError::TooLarge);
                }
                return Ok(BufferedHeaders { bytes, header_end });
            }
            if bytes.len() > max_bytes {
                return Err(HeaderReadError::TooLarge);
            }
        }
    };

    timeout(deadline, operation)
        .await
        .map_err(|_| HeaderReadError::TimedOut)?
}

async fn send_plain_response(
    stream: &mut TcpStream,
    status: &str,
    extra_headers: &[(&str, &str)],
) -> io::Result<()> {
    let mut response = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n");
    for (name, value) in extra_headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes()).await
}

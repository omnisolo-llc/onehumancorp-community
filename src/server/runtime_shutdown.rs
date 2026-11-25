use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::watch;

/// Cooperative shutdown token shared across HTTP, gRPC, and background workers.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    tx: Arc<watch::Sender<bool>>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self { tx: Arc::new(tx) }
    }

    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    pub fn watch_receiver(&self) -> watch::Receiver<bool> {
        self.tx.subscribe()
    }

    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        if *rx.borrow() {
            return;
        }
        while rx.changed().await.is_ok() {
            if *rx.borrow() {
                return;
            }
        }
    }
}

/// Spawn a task that cancels `shutdown` upon receiving `SIGINT` or `SIGTERM`.
pub fn install_signal_handler(shutdown: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            tracing::info!("Received SIGINT; initiating graceful shutdown");
                        }
                        _ = sigterm.recv() => {
                            tracing::info!("Received SIGTERM; initiating graceful shutdown");
                        }
                        _ = shutdown.cancelled() => {
                            return;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        "Failed to register SIGTERM handler ({error}); falling back to SIGINT"
                    );
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            tracing::info!("Received SIGINT; initiating graceful shutdown");
                        }
                        _ = shutdown.cancelled() => {
                            return;
                        }
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Received shutdown signal; initiating graceful shutdown");
                }
                _ = shutdown.cancelled() => {
                    return;
                }
            }
        }
        shutdown.cancel();
    });
}

/// Resolve the HTTP/WebSocket bind host. Standalone mode defaults to loopback
/// (`127.0.0.1`) unless explicitly overridden via `OMNISOLO_BIND_HOST`.
pub fn resolve_http_bind_host(standalone: bool, configured_host: Option<&str>) -> String {
    if let Some(host) = configured_host.map(str::trim).filter(|h| !h.is_empty()) {
        return host.to_string();
    }
    if standalone {
        "127.0.0.1".to_string()
    } else {
        "0.0.0.0".to_string()
    }
}

pub fn resolve_http_bind_addr(
    standalone: bool,
    configured_host: Option<&str>,
    port: u16,
) -> Result<SocketAddr, std::io::Error> {
    let host = resolve_http_bind_host(standalone, configured_host);
    if let Ok(ip) = IpAddr::from_str(&host) {
        return Ok(SocketAddr::new(ip, port));
    }
    let candidate = format!("{host}:{port}");
    candidate.parse::<SocketAddr>().map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid HTTP bind address '{candidate}': {error}"),
        )
    })
}

pub async fn bind_http_listener(
    standalone: bool,
) -> Result<(tokio::net::TcpListener, SocketAddr), std::io::Error> {
    let port = std::env::var("OMNISOLO_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(18789);
    let configured_host = std::env::var("OMNISOLO_BIND_HOST").ok();
    let addr = resolve_http_bind_addr(standalone, configured_host.as_deref(), port)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound_addr = listener.local_addr().unwrap_or(addr);
    Ok((listener, bound_addr))
}

/// Serve Axum HTTP/WebSocket and Tonic gRPC concurrently with coordinated
/// graceful shutdown. If either server exits or errors, the other is signalled
/// to drain immediately.
pub async fn serve_http_and_grpc<F>(
    listener: tokio::net::TcpListener,
    mesh_addr: SocketAddr,
    app: axum::Router,
    grpc_future: F,
    shutdown: CancellationToken,
) -> Result<(), std::io::Error>
where
    F: Future<Output = Result<(), tonic::transport::Error>> + Send,
{
    let http_shutdown = shutdown.clone();
    let http_cancel_on_exit = shutdown.clone();
    let http_future = async move {
        tracing::info!("Mesh WebSocket server listening on {}", mesh_addr);
        let result = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            http_shutdown.cancelled().await;
        })
        .await;
        http_cancel_on_exit.cancel();
        if let Err(ref error) = result {
            ::server_telemetry::record_error_signal("[bug] Mesh server error");
            tracing::error!("Mesh HTTP/WebSocket server exited with error: {}", error);
        }
        result
    };

    let grpc_cancel_on_exit = shutdown.clone();
    let wrapped_grpc = async move {
        let result = grpc_future.await;
        grpc_cancel_on_exit.cancel();
        if let Err(ref error) = result {
            ::server_telemetry::record_error_signal("[bug] gRPC server error");
            tracing::error!("gRPC server exited with error: {}", error);
        }
        result.map_err(std::io::Error::other)
    };

    tokio::try_join!(http_future, wrapped_grpc)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn cancellation_token_notifies_waiters_and_watch_receivers() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        let mut rx = token.watch_receiver();
        let waiter = {
            let t = token.clone();
            tokio::spawn(async move {
                t.cancelled().await;
                true
            })
        };
        token.cancel();
        assert!(token.is_cancelled());
        assert!(waiter.await.unwrap());
        rx.changed().await.unwrap();
        assert!(*rx.borrow());
    }

    #[test]
    fn http_bind_defaults_to_loopback_in_standalone_and_supports_explicit_override() {
        assert_eq!(resolve_http_bind_host(true, None), "127.0.0.1");
        assert_eq!(resolve_http_bind_host(true, Some("  ")), "127.0.0.1");
        assert_eq!(resolve_http_bind_host(false, None), "0.0.0.0");
        assert_eq!(resolve_http_bind_host(true, Some("0.0.0.0")), "0.0.0.0");
        assert_eq!(
            resolve_http_bind_addr(true, None, 18789).unwrap(),
            "127.0.0.1:18789".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            resolve_http_bind_addr(true, Some("::1"), 18789).unwrap(),
            "[::1]:18789".parse::<SocketAddr>().unwrap()
        );
        assert!(resolve_http_bind_addr(true, Some("not a host"), 18789).is_err());
    }

    #[tokio::test]
    async fn serve_http_and_grpc_serves_requests_and_shuts_down_gracefully() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));
        let shutdown = CancellationToken::new();
        let grpc_shutdown = shutdown.clone();
        let grpc_future = async move {
            grpc_shutdown.cancelled().await;
            Ok::<(), tonic::transport::Error>(())
        };

        let server_shutdown = shutdown.clone();
        let server_task = tokio::spawn(async move {
            serve_http_and_grpc(listener, addr, app, grpc_future, server_shutdown)
                .await
                .map_err(|e| e.to_string())
        });

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(
            response.contains("200 OK"),
            "unexpected response: {response}"
        );
        assert!(response.ends_with("ok"), "unexpected body: {response}");

        shutdown.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), server_task)
            .await
            .expect("server must terminate promptly on shutdown")
            .unwrap();
        assert!(result.is_ok(), "expected clean shutdown, got: {result:?}");
    }
}

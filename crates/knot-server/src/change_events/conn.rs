//! The connection to the receiver: connect, hello, send batches, keepalive,
//! reconnect with backoff.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::RngExt as _;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::WireMessage;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Exponential backoff; each delay is drawn from `[cur/2, cur]`.
pub(super) struct Backoff {
    min: Duration,
    max: Duration,
    cur: Duration,
}

impl Backoff {
    pub(super) fn new(min: Duration, max: Duration) -> Self {
        Self { min, max, cur: min }
    }
    pub(super) fn next(&mut self) -> Duration {
        let cur = self.cur;
        self.cur = (self.cur * 2).min(self.max);
        let half = cur / 2;
        let span = (cur - half).as_millis() as u64;
        half + Duration::from_millis(rand::rng().random_range(0..=span))
    }
    pub(super) fn reset(&mut self) {
        self.cur = self.min;
    }
}

pub(super) struct ConnConfig {
    pub url: String,
    pub auth: Option<HeaderValue>,
    pub instance_id: Uuid,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub ping_interval: Duration,
    pub connect_timeout: Duration,
    pub send_timeout: Duration,
    pub tls: Arc<rustls::ClientConfig>,
}

/// rustls with the ring provider and the webpki roots, built explicitly so
/// it does not depend on a process-wide default provider.
pub(super) fn tls_config() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    Arc::new(cfg)
}

enum End {
    Dropped,
    Shutdown,
}

fn set_connected(flag: &AtomicBool, up: bool) {
    flag.store(up, Ordering::Release);
    metrics::gauge!("knot_change_event_hook_connected").set(if up { 1.0 } else { 0.0 });
}

async fn connect(cfg: &ConnConfig) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = cfg.url.as_str().into_client_request()?;
    if let Some(auth) = &cfg.auth {
        request.headers_mut().insert("authorization", auth.clone());
    }
    let (socket, _response) = tokio_tungstenite::connect_async_tls_with_config(
        request,
        None,
        false,
        Some(Connector::Rustls(cfg.tls.clone())),
    )
    .await?;
    Ok(socket)
}

pub(super) async fn run(
    cfg: ConnConfig,
    mut batches: mpsc::Receiver<Vec<WireMessage>>,
    connected: Arc<AtomicBool>,
    shutdown: CancellationToken,
) {
    let mut backoff = Backoff::new(cfg.backoff_min, cfg.backoff_max);
    set_connected(&connected, false);
    loop {
        let attempt = tokio::select! {
            _ = shutdown.cancelled() => return,
            r = timeout(cfg.connect_timeout, connect(&cfg)) => r,
        };
        match attempt {
            Ok(Ok(socket)) => {
                metrics::counter!("knot_change_event_hook_connects_total", "result" => "ok")
                    .increment(1);
                tracing::info!("change event hook: connected");
                backoff.reset();
                set_connected(&connected, true);
                let end = session(socket, &cfg, &mut batches, &shutdown).await;
                set_connected(&connected, false);
                // Nothing is carried over to the next connection.
                let mut lost = 0usize;
                while let Ok(batch) = batches.try_recv() {
                    lost += batch.len();
                }
                if lost > 0 {
                    metrics::counter!(
                        "knot_change_event_hook_events_dropped_total",
                        "reason" => "disconnected"
                    )
                    .increment(lost as u64);
                }
                if matches!(end, End::Shutdown) {
                    return;
                }
                tracing::warn!("change event hook: connection lost, reconnecting");
            }
            Ok(Err(e)) => {
                metrics::counter!("knot_change_event_hook_connects_total", "result" => "error")
                    .increment(1);
                tracing::warn!(error = %e, "change event hook: connect failed");
            }
            Err(_) => {
                metrics::counter!("knot_change_event_hook_connects_total", "result" => "error")
                    .increment(1);
                tracing::warn!("change event hook: connect timed out");
            }
        }
        let delay = backoff.next();
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

async fn send(socket: &mut Socket, msg: Message, cfg: &ConnConfig) -> bool {
    matches!(
        timeout(cfg.send_timeout, socket.send(msg)).await,
        Ok(Ok(()))
    )
}

/// Close frame on shutdown, bounded so a stuck receiver cannot delay it.
async fn goodbye(mut socket: Socket) -> End {
    let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
    End::Shutdown
}

async fn session(
    mut socket: Socket,
    cfg: &ConnConfig,
    batches: &mut mpsc::Receiver<Vec<WireMessage>>,
    shutdown: &CancellationToken,
) -> End {
    let hello = WireMessage::Hello {
        instance_id: cfg.instance_id,
        version: env!("CARGO_PKG_VERSION"),
    };
    let text = serde_json::to_string(&hello).expect("hello serializes");
    if !send(&mut socket, Message::text(text), cfg).await {
        return End::Dropped;
    }
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::now() + cfg.ping_interval,
        cfg.ping_interval,
    );
    let mut unanswered = 0u32;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return goodbye(socket).await,
            batch = batches.recv() => {
                // The coalescer ends on shutdown too, so this races the arm
                // above: say goodbye either way.
                let Some(batch) = batch else { return goodbye(socket).await };
                let total = batch.len();
                for (i, msg) in batch.into_iter().enumerate() {
                    let text = serde_json::to_string(&msg).expect("event serializes");
                    if !send(&mut socket, Message::text(text), cfg).await {
                        metrics::counter!(
                            "knot_change_event_hook_events_dropped_total",
                            "reason" => "send_failed"
                        )
                        .increment((total - i) as u64);
                        return End::Dropped;
                    }
                    metrics::counter!("knot_change_event_hook_events_sent_total").increment(1);
                }
            }
            _ = ping.tick() => {
                if unanswered >= 2 {
                    tracing::warn!("change event hook: receiver stopped answering pings");
                    return End::Dropped;
                }
                unanswered += 1;
                if !send(&mut socket, Message::Ping(Vec::new().into()), cfg).await {
                    return End::Dropped;
                }
            }
            frame = socket.next() => match frame {
                Some(Ok(Message::Pong(_))) => unanswered = 0,
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return End::Dropped,
                Some(Ok(other)) => {
                    tracing::debug!(?other, "change event hook: ignoring frame from receiver");
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn backoff_doubles_within_jitter_bounds_and_resets() {
        let (min, max) = (Duration::from_millis(100), Duration::from_millis(800));
        let mut b = Backoff::new(min, max);
        for cur in [100u64, 200, 400, 800, 800, 800] {
            let d = b.next();
            let cur = Duration::from_millis(cur);
            assert!(
                d >= cur / 2 && d <= cur,
                "{d:?} not in [{:?}, {cur:?}]",
                cur / 2
            );
        }
        b.reset();
        assert!(b.next() <= min);
    }
}

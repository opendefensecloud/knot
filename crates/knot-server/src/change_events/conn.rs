//! The connection to the receiver: connect, hello, send batches, keepalive,
//! reconnect with backoff.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

use super::{Batch, WireMessage};

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

/// Publish the current connection generation (0: disconnected).
fn set_connected(flag: &AtomicU64, epoch: u64) {
    flag.store(epoch, Ordering::Release);
    metrics::gauge!("knot_change_event_hook_connected").set(if epoch != 0 { 1.0 } else { 0.0 });
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
    mut batches: mpsc::Receiver<Batch>,
    connected: Arc<AtomicU64>,
    shutdown: CancellationToken,
) {
    let mut backoff = Backoff::new(cfg.backoff_min, cfg.backoff_max);
    // Connection generations, from 1; each connection gets a fresh one.
    let mut epoch = 0u64;
    set_connected(&connected, 0);
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
                epoch += 1;
                set_connected(&connected, epoch);
                let end = session(socket, epoch, &cfg, &mut batches, &shutdown).await;
                set_connected(&connected, 0);
                // Nothing is carried over to the next connection. A batch the
                // coalescer is still flushing for this epoch can land after
                // this drain; the next session drops it by its epoch.
                let mut lost = 0usize;
                while let Ok(batch) = batches.try_recv() {
                    lost += batch.messages.len();
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
    epoch: u64,
    cfg: &ConnConfig,
    batches: &mut mpsc::Receiver<Batch>,
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
                let total = batch.messages.len();
                if batch.epoch != epoch {
                    // Flushed for an earlier connection: never sent on this one.
                    metrics::counter!(
                        "knot_change_event_hook_events_dropped_total",
                        "reason" => "disconnected"
                    )
                    .increment(total as u64);
                    continue;
                }
                for (i, msg) in batch.messages.into_iter().enumerate() {
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

    async fn next_text(ws: &mut WebSocketStream<TcpStream>) -> serde_json::Value {
        loop {
            match ws.next().await.expect("open").expect("frame") {
                Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    async fn accept(listener: &tokio::net::TcpListener) -> WebSocketStream<TcpStream> {
        let (tcp, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("hook connected in time")
            .unwrap();
        tokio_tungstenite::accept_async(tcp).await.unwrap()
    }

    fn event(doc_id: Uuid) -> WireMessage {
        WireMessage::PageChanged {
            doc_id,
            workspace_id: Uuid::nil(),
            kind: super::super::ChangeKind::Content,
            at: chrono::Utc::now(),
        }
    }

    /// Spec D8: a batch the coalescer flushed for a connection that has
    /// dropped since (it checked `connected` before the drop, sent after the
    /// drain) is not sent on the next connection.
    #[tokio::test]
    async fn a_batch_of_an_earlier_connection_is_not_sent_on_the_next() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cfg = ConnConfig {
            url: format!("ws://{}", listener.local_addr().unwrap()),
            auth: None,
            instance_id: Uuid::new_v4(),
            backoff_min: Duration::from_millis(10),
            backoff_max: Duration::from_millis(10),
            ping_interval: Duration::from_secs(3600),
            connect_timeout: Duration::from_secs(5),
            send_timeout: Duration::from_secs(5),
            tls: tls_config(),
        };
        let (tx, rx) = mpsc::channel(16);
        let connected = Arc::new(AtomicU64::new(0));
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run(cfg, rx, connected.clone(), shutdown.clone()));

        let mut first = accept(&listener).await;
        assert_eq!(next_text(&mut first).await["type"], "hello");
        let stale = connected.load(Ordering::Acquire);
        assert_ne!(stale, 0, "connected during the first session");
        drop(first);

        let mut second = accept(&listener).await;
        assert_eq!(next_text(&mut second).await["type"], "hello");
        let current = connected.load(Ordering::Acquire);
        assert!(current != 0 && current != stale, "{stale} -> {current}");

        let (old, new) = (Uuid::new_v4(), Uuid::new_v4());
        tx.send(Batch {
            epoch: stale,
            messages: vec![event(old)],
        })
        .await
        .unwrap();
        tx.send(Batch {
            epoch: current,
            messages: vec![event(new)],
        })
        .await
        .unwrap();
        let got = timeout(Duration::from_secs(5), next_text(&mut second))
            .await
            .expect("event in time");
        assert_eq!(got["doc_id"], new.to_string(), "{got}");

        shutdown.cancel();
        task.await.unwrap();
    }
}

//! Postgres LISTEN/NOTIFY Bus.
//!
//! One dedicated `tokio_postgres` connection per replica owns LISTEN for
//! every doc this replica has rooms for. Demuxes incoming Notifications
//! by channel name into per-doc mpsc senders.
//!
//! Channel naming:
//!   doc:<uuid>       — payload = "<seq>" as decimal text
//!   presence:<uuid>  — payload = url-safe base64 of bytes

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dashmap::DashMap;
use futures_util::{StreamExt, stream};
use rustls::pki_types::pem::PemObject;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_postgres::config::SslMode;
use tokio_postgres_rustls::MakeRustlsConnect;
use uuid::Uuid;

use crate::bus::{Bus, BusError, Subscription};

const PRESENCE_PAYLOAD_CAP_B64: usize = 6 * 1024;
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Default)]
struct DocChannels {
    update_tx: Vec<mpsc::Sender<i64>>,
    presence_tx: Vec<mpsc::Sender<Vec<u8>>>,
}

#[derive(Clone)]
pub struct PgBus {
    // Swappable so the supervisor can replace the client after a reconnect.
    // The lock is only ever held to clone the Arc out — never across an await.
    client: Arc<Mutex<Arc<tokio_postgres::Client>>>,
    subscriptions: Arc<DashMap<Uuid, DocChannels>>,
}

/// TLS material pulled out of a libpq-style connection URL. tokio_postgres's
/// own parser rejects the `sslcert`/`sslkey`/`sslrootcert` keywords (and the
/// `verify-*` sslmode values), so we strip them here and hand them to rustls.
struct PgTls {
    root_cert: Option<PathBuf>,
    client_cert: Option<PathBuf>,
    client_key: Option<PathBuf>,
}

/// Split a database URL into a `tokio_postgres::Config` (with the libpq `ssl*`
/// query params removed so it parses) plus the extracted TLS material. sslmode
/// maps onto `Config::ssl_mode`: `disable` → Disable, `prefer`/unset → Prefer,
/// `require`/`verify-ca`/`verify-full` → Require. rustls always verifies the
/// server cert against the supplied root, giving verify-full semantics when a
/// root cert is present.
fn split_url_tls(database_url: &str) -> Result<(tokio_postgres::Config, PgTls), BusError> {
    let (base, query) = match database_url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (database_url, None),
    };

    let mut ssl_mode: Option<&str> = None;
    let mut root_cert = None;
    let mut client_cert = None;
    let mut client_key = None;
    let mut kept: Vec<&str> = Vec::new();

    if let Some(q) = query {
        for pair in q.split('&').filter(|s| !s.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "sslmode" => ssl_mode = Some(value),
                "sslrootcert" => root_cert = Some(PathBuf::from(value)),
                "sslcert" => client_cert = Some(PathBuf::from(value)),
                "sslkey" => client_key = Some(PathBuf::from(value)),
                _ => kept.push(pair),
            }
        }
    }

    let rebuilt = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };

    let mut config = rebuilt
        .parse::<tokio_postgres::Config>()
        .map_err(|e| BusError::Io(e.to_string()))?;

    config.ssl_mode(match ssl_mode {
        Some("disable") => SslMode::Disable,
        Some("require" | "verify-ca" | "verify-full") => SslMode::Require,
        _ => SslMode::Prefer,
    });

    Ok((
        config,
        PgTls {
            root_cert,
            client_cert,
            client_key,
        },
    ))
}

/// Build a rustls-backed TLS connector from the extracted TLS material. A root
/// cert enables server verification; a client cert+key enables mTLS (required
/// by CNPG's `clientcert=verify-full`). With no material the connector still
/// builds and is simply unused when the server doesn't negotiate TLS.
fn build_tls_connector(tls: &PgTls) -> Result<MakeRustlsConnect, BusError> {
    let mut roots = rustls::RootCertStore::empty();
    if let Some(path) = &tls.root_cert {
        for cert in load_certs(path)? {
            roots
                .add(cert)
                .map_err(|e| BusError::Io(format!("add root cert: {e}")))?;
        }
    }

    // Both ring and aws-lc-rs are in the dependency graph, so rustls cannot
    // auto-select a process-default provider — pick ring explicitly.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| BusError::Io(format!("rustls protocol versions: {e}")))?
        .with_root_certificates(roots);

    let config = match (&tls.client_cert, &tls.client_key) {
        (Some(cert), Some(key)) => builder
            .with_client_auth_cert(load_certs(cert)?, load_key(key)?)
            .map_err(|e| BusError::Io(format!("client auth cert: {e}")))?,
        _ => builder.with_no_client_auth(),
    };

    Ok(MakeRustlsConnect::new(config))
}

fn load_certs(path: &Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, BusError> {
    let data =
        std::fs::read(path).map_err(|e| BusError::Io(format!("read {}: {e}", path.display())))?;
    rustls::pki_types::CertificateDer::pem_slice_iter(&data)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| BusError::Io(format!("parse certs {}: {e}", path.display())))
}

fn load_key(path: &Path) -> Result<rustls::pki_types::PrivateKeyDer<'static>, BusError> {
    let data =
        std::fs::read(path).map_err(|e| BusError::Io(format!("read {}: {e}", path.display())))?;
    rustls::pki_types::PrivateKeyDer::from_pem_slice(&data)
        .map_err(|e| BusError::Io(format!("parse key {}: {e}", path.display())))
}

impl PgBus {
    /// Connect a dedicated tokio_postgres client and spawn a supervised demux
    /// task. The initial connect is fail-fast; thereafter the supervisor
    /// reconnects with backoff and re-issues LISTEN for every active
    /// subscription, so a transient DB blip no longer permanently kills
    /// cross-pod fan-out.
    pub async fn connect(database_url: &str) -> Result<Self, BusError> {
        let (config, tls) = split_url_tls(database_url)?;
        let connector = build_tls_connector(&tls)?;
        let (client, connection) = config
            .connect(connector.clone())
            .await
            .map_err(|e| BusError::Io(e.to_string()))?;

        let client_slot = Arc::new(Mutex::new(Arc::new(client)));
        let subscriptions: Arc<DashMap<Uuid, DocChannels>> = Arc::new(DashMap::new());

        let slot = client_slot.clone();
        let subs = subscriptions.clone();
        tokio::spawn(async move {
            let mut next_conn = Some(connection);
            loop {
                let mut connection = next_conn.take().expect("connection present each iteration");

                // Drive THIS connection in a dedicated task so the client can
                // make progress (tokio_postgres requires the connection to be
                // polled). It returns when the connection errors or closes.
                let driver_subs = subs.clone();
                let driver = tokio::spawn(async move {
                    let stream = stream::poll_fn(|cx| connection.poll_message(cx));
                    tokio::pin!(stream);
                    while let Some(msg) = stream.next().await {
                        match msg {
                            Ok(tokio_postgres::AsyncMessage::Notification(n)) => {
                                Self::route(&driver_subs, n.channel(), n.payload());
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(error=?e, "pg bus connection error");
                                break;
                            }
                        }
                    }
                });

                // The driver is now polling, so LISTEN executes resolve. On the
                // first pass `subs` is empty (no-op); after a reconnect this
                // re-subscribes every active doc so fan-out resumes.
                let client = slot.lock().unwrap().clone();
                for entry in subs.iter() {
                    let doc_id = *entry.key();
                    let _ = client
                        .execute(&format!("LISTEN \"doc:{doc_id}\""), &[])
                        .await;
                    let _ = client
                        .execute(&format!("LISTEN \"presence:{doc_id}\""), &[])
                        .await;
                }

                // Block until the connection dies.
                let _ = driver.await;
                tracing::warn!("pg bus connection lost; reconnecting");

                // Reconnect: try immediately, back off on repeated failure.
                let (new_client, new_conn) = loop {
                    match config.connect(connector.clone()).await {
                        Ok(cc) => break cc,
                        Err(e) => {
                            tracing::warn!(error=?e, "pg bus reconnect failed; retrying");
                            tokio::time::sleep(RECONNECT_BACKOFF).await;
                        }
                    }
                };
                *slot.lock().unwrap() = Arc::new(new_client);
                next_conn = Some(new_conn);
                tracing::info!("pg bus reconnected");
            }
        });

        Ok(Self {
            client: client_slot,
            subscriptions,
        })
    }

    /// Current client handle. The lock is held only long enough to clone the
    /// Arc — never across the subsequent await — so a reconnect swap never
    /// blocks publishers.
    fn current_client(&self) -> Arc<tokio_postgres::Client> {
        self.client.lock().unwrap().clone()
    }

    fn route(subscriptions: &Arc<DashMap<Uuid, DocChannels>>, channel: &str, payload: &str) {
        if let Some(rest) = channel.strip_prefix("doc:") {
            let Ok(doc_id) = Uuid::parse_str(rest) else {
                return;
            };
            let Ok(seq) = payload.parse::<i64>() else {
                return;
            };
            if let Some(mut e) = subscriptions.get_mut(&doc_id) {
                e.update_tx.retain(|tx| tx.try_send(seq).is_ok());
            }
        } else if let Some(rest) = channel.strip_prefix("presence:") {
            let Ok(doc_id) = Uuid::parse_str(rest) else {
                return;
            };
            let Ok(bytes) = URL_SAFE_NO_PAD.decode(payload) else {
                return;
            };
            if let Some(mut e) = subscriptions.get_mut(&doc_id) {
                e.presence_tx
                    .retain(|tx| tx.try_send(bytes.clone()).is_ok());
            }
        }
    }
}

#[async_trait]
impl Bus for PgBus {
    async fn publish(&self, doc_id: Uuid, seq: i64) -> Result<(), BusError> {
        // pg_notify() binds the channel + payload as parameters, so no SQL
        // string is built from values (defence-in-depth; both are internal).
        self.current_client()
            .execute(
                "SELECT pg_notify($1, $2)",
                &[&format!("doc:{doc_id}"), &seq.to_string()],
            )
            .await
            .map_err(|e| BusError::Io(e.to_string()))?;
        Ok(())
    }

    async fn publish_presence(&self, doc_id: Uuid, payload: Vec<u8>) -> Result<(), BusError> {
        let encoded = URL_SAFE_NO_PAD.encode(&payload);
        if encoded.len() > PRESENCE_PAYLOAD_CAP_B64 {
            tracing::debug!(len = encoded.len(), "drop oversize presence frame");
            return Ok(());
        }
        self.current_client()
            .execute(
                "SELECT pg_notify($1, $2)",
                &[&format!("presence:{doc_id}"), &encoded],
            )
            .await
            .map_err(|e| BusError::Io(e.to_string()))?;
        Ok(())
    }

    async fn subscribe(&self, doc_id: Uuid) -> Result<Subscription, BusError> {
        let (ut, ur) = mpsc::channel::<i64>(256);
        let (pt, pr) = mpsc::channel::<Vec<u8>>(256);
        let was_new = !self.subscriptions.contains_key(&doc_id);
        let mut entry = self.subscriptions.entry(doc_id).or_default();
        entry.update_tx.push(ut);
        entry.presence_tx.push(pt);
        drop(entry);
        if was_new {
            let client = self.current_client();
            client
                .execute(&format!("LISTEN \"doc:{doc_id}\""), &[])
                .await
                .map_err(|e| BusError::Io(e.to_string()))?;
            client
                .execute(&format!("LISTEN \"presence:{doc_id}\""), &[])
                .await
                .map_err(|e| BusError::Io(e.to_string()))?;
        }
        Ok(Subscription {
            updates: ur,
            presence: pr,
        })
    }

    async fn unsubscribe(&self, doc_id: Uuid) -> Result<(), BusError> {
        let still_active = self
            .subscriptions
            .get(&doc_id)
            .map(|e| e.update_tx.iter().any(|t| !t.is_closed()))
            .unwrap_or(false);
        if !still_active {
            self.subscriptions.remove(&doc_id);
            let client = self.current_client();
            let _ = client
                .execute(&format!("UNLISTEN \"doc:{doc_id}\""), &[])
                .await;
            let _ = client
                .execute(&format!("UNLISTEN \"presence:{doc_id}\""), &[])
                .await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tls_parsing_tests {
    use super::*;

    #[test]
    fn splits_ssl_params_and_maps_verify_full_to_require() {
        let url = "postgresql://knot@knot-db-rw:5432/knot?sslmode=verify-full\
                   &sslcert=/db-certs/tls.crt&sslkey=/db-certs/tls.key&sslrootcert=/db-certs/ca.crt";
        let (config, tls) = split_url_tls(url).expect("split");
        // ssl* params are stripped so tokio_postgres can parse the rest.
        assert_eq!(config.get_dbname(), Some("knot"));
        assert_eq!(config.get_user(), Some("knot"));
        assert_eq!(config.get_ssl_mode(), SslMode::Require);
        assert_eq!(
            tls.root_cert.as_deref(),
            Some(Path::new("/db-certs/ca.crt"))
        );
        assert_eq!(
            tls.client_cert.as_deref(),
            Some(Path::new("/db-certs/tls.crt"))
        );
        assert_eq!(
            tls.client_key.as_deref(),
            Some(Path::new("/db-certs/tls.key"))
        );
    }

    #[test]
    fn plain_url_parses_with_no_tls_material() {
        let (config, tls) = split_url_tls("postgresql://u:p@localhost:5432/db").expect("split");
        assert_eq!(config.get_dbname(), Some("db"));
        assert!(tls.root_cert.is_none());
        assert!(tls.client_cert.is_none());
        assert!(tls.client_key.is_none());
    }
}

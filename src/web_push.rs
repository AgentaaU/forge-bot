//! Persistent browser subscriptions and bounded, asynchronous Web Push delivery.
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use web_push::{ContentEncoding, SubscriptionInfo, VapidSignatureBuilder, WebPushMessageBuilder};

use crate::{
    config::{Config, NotificationTransport},
    notify::Notification,
};

#[derive(Clone, Serialize, Deserialize)]
struct Subscription {
    recipient: String,
    subscription: SubscriptionInfo,
}

#[async_trait::async_trait]
trait PushClient: Send + Sync {
    async fn send(&self, message: web_push::WebPushMessage) -> Result<u16>;
}

struct HttpPushClient(reqwest::Client);

#[async_trait::async_trait]
impl PushClient for HttpPushClient {
    async fn send(&self, message: web_push::WebPushMessage) -> Result<u16> {
        let request = web_push::request_builder::build_request::<Vec<u8>>(message);
        let mut outgoing = self.0.post(request.uri().to_string());
        for (name, value) in request.headers() {
            outgoing = outgoing.header(name.as_str(), value.as_bytes());
        }
        Ok(outgoing
            .body(request.into_body())
            .send()
            .await?
            .status()
            .as_u16())
    }
}

pub struct PushService {
    key: Vec<u8>,
    pub public_key: String,
    subject: String,
    hosts: Vec<String>,
    path: PathBuf,
    subscriptions: Mutex<Vec<Subscription>>,
    client: Arc<dyn PushClient>,
    sender: mpsc::Sender<Notification>,
}

impl PushService {
    pub fn new(config: &Config) -> Result<Option<Arc<Self>>> {
        Self::with_client(
            config,
            Arc::new(HttpPushClient(
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(15))
                    .build()?,
            )),
        )
    }

    fn with_client(config: &Config, client: Arc<dyn PushClient>) -> Result<Option<Arc<Self>>> {
        let options = &config.notifications;
        if options.transport == NotificationTransport::Polling {
            return Ok(None);
        }
        let Some(path) = &options.vapid_private_key_path else {
            return Ok(None);
        };
        let subject = options
            .vapid_subject
            .clone()
            .context("notifications.vapid_subject is required with a VAPID key")?;
        let contact = url::Url::parse(&subject)?;
        anyhow::ensure!(
            matches!(contact.scheme(), "mailto" | "https"),
            "VAPID subject must be mailto: or HTTPS"
        );
        let key = std::fs::read(path).context("read VAPID private key")?;
        let public_key = URL_SAFE_NO_PAD
            .encode(VapidSignatureBuilder::from_pem_no_sub(key.as_slice())?.get_public_key());
        let path = config.session.dir.join("push-subscriptions.json");
        let subscriptions = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("read push subscriptions")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        let (sender, mut receiver) = mpsc::channel::<Notification>(256);
        let service = Arc::new(Self {
            key,
            public_key,
            subject,
            path,
            subscriptions: Mutex::new(subscriptions),
            hosts: if options.push_hosts.is_empty() {
                vec![
                    "fcm.googleapis.com".into(),
                    "updates.push.services.mozilla.com".into(),
                    "web.push.apple.com".into(),
                ]
            } else {
                options.push_hosts.clone()
            },
            client,
            sender,
        });
        let worker = Arc::downgrade(&service);
        tokio::runtime::Handle::try_current()
            .context("web push requires an async runtime")?
            .spawn(async move {
                while let Some(notification) = receiver.recv().await {
                    let Some(service) = worker.upgrade() else {
                        break;
                    };
                    service.deliver(&notification).await;
                }
            });
        Ok(Some(service))
    }

    fn validate_endpoint(&self, endpoint: &str) -> Result<()> {
        let url = url::Url::parse(endpoint)?;
        anyhow::ensure!(
            url.scheme() == "https"
                && url.port_or_known_default() == Some(443)
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && self
                    .hosts
                    .iter()
                    .any(|host| Some(host.as_str()) == url.host_str()),
            "untrusted push endpoint"
        );
        Ok(())
    }

    pub fn subscribe(&self, recipient: &str, subscription: SubscriptionInfo) -> Result<()> {
        self.validate_endpoint(&subscription.endpoint)?;
        // Validate the browser's encryption material before persisting it.
        self.message(&subscription, b"{}")?;
        let mut entries = self
            .subscriptions
            .lock()
            .expect("push subscriptions mutex poisoned");
        let mut updated = entries.clone();
        updated.retain(|entry| entry.subscription.endpoint != subscription.endpoint);
        if updated.len() >= 1024 {
            bail!("push subscription limit reached")
        }
        updated.push(Subscription {
            recipient: recipient.to_ascii_lowercase(),
            subscription,
        });
        self.persist(&updated)?;
        *entries = updated;
        Ok(())
    }

    pub fn unsubscribe(&self, endpoint: &str) -> Result<()> {
        let mut entries = self
            .subscriptions
            .lock()
            .expect("push subscriptions mutex poisoned");
        let mut updated = entries.clone();
        updated.retain(|entry| entry.subscription.endpoint != endpoint);
        self.persist(&updated)?;
        *entries = updated;
        Ok(())
    }

    fn persist(&self, entries: &[Subscription]) -> Result<()> {
        let parent = self.path.parent().context("subscription directory")?;
        std::fs::create_dir_all(parent)?;
        let temporary = self.path.with_extension("json.tmp");
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(entries)?)?;
        file.sync_all()?;
        std::fs::rename(temporary, &self.path)?;
        Ok(())
    }

    fn message(
        &self,
        subscription: &SubscriptionInfo,
        payload: &[u8],
    ) -> Result<web_push::WebPushMessage> {
        let mut signature = VapidSignatureBuilder::from_pem(self.key.as_slice(), subscription)?;
        signature.add_claim("sub", self.subject.as_str());
        let mut message = WebPushMessageBuilder::new(subscription);
        message.set_vapid_signature(signature.build()?);
        message.set_ttl(86400);
        message.set_payload(ContentEncoding::Aes128Gcm, payload);
        Ok(message.build()?)
    }

    pub fn enqueue(&self, notification: Notification) {
        if self.sender.try_send(notification).is_err() {
            tracing::warn!("web push queue full or closed; notification remains in polling log");
        }
    }

    async fn deliver(&self, notification: &Notification) {
        let entries: Vec<_> = self
            .subscriptions
            .lock()
            .expect("push subscriptions mutex poisoned")
            .iter()
            .filter(|entry| {
                entry
                    .recipient
                    .eq_ignore_ascii_case(&notification.recipient)
            })
            .cloned()
            .collect();
        let payload = serde_json::to_vec(notification).expect("serialize notification");
        for entry in entries {
            let result = self.send(&entry.subscription, &payload).await;
            match result {
                Ok(404 | 410) => {
                    if self.unsubscribe(&entry.subscription.endpoint).is_err() {
                        tracing::warn!("could not remove expired push subscription");
                    }
                }
                Ok(status) if (200..300).contains(&status) => {}
                // Endpoints and encryption secrets are deliberately absent from logs.
                _ => {
                    tracing::warn!(recipient = %entry.recipient, "web push delivery failed; notification remains in polling log")
                }
            }
        }
    }

    async fn send(&self, subscription: &SubscriptionInfo, payload: &[u8]) -> Result<u16> {
        self.validate_endpoint(&subscription.endpoint)?;
        self.client.send(self.message(subscription, payload)?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NotificationsConfig;

    fn config(dir: &std::path::Path) -> Config {
        let key = dir.join("vapid.pem");
        std::fs::write(&key, include_bytes!("../tests/fixtures/vapid-test.pem")).unwrap();
        let mut config = Config::default();
        config.session.dir = dir.join("state");
        config.notifications.vapid_private_key_path = Some(key);
        config.notifications.vapid_subject = Some("mailto:operator@example.com".into());
        config
    }

    fn subscription(endpoint: &str) -> SubscriptionInfo {
        SubscriptionInfo::new(
            endpoint,
            "BGa4N1PI79lboMR_YrwCiCsgp35DRvedt7opHcf0yM3iOBTSoQYqQLwWxAfRKE6tsDnReWmhsImkhDF_DBdkNSU",
            "EvcWjEgzr4rbvhfi3yds0A",
        )
    }

    #[tokio::test]
    async fn persists_rebinds_and_removes_subscriptions_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let service = PushService::new(&config).unwrap().unwrap();
        let endpoint = "https://fcm.googleapis.com/push/test";
        service.subscribe("Alice", subscription(endpoint)).unwrap();
        service.subscribe("Bob", subscription(endpoint)).unwrap();
        let loaded = PushService::new(&config).unwrap().unwrap();
        assert_eq!(loaded.public_key, service.public_key);
        let entries = loaded.subscriptions.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].recipient, "bob");
        drop(entries);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&loaded.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        loaded.unsubscribe(endpoint).unwrap();
        assert!(
            PushService::new(&config)
                .unwrap()
                .unwrap()
                .subscriptions
                .lock()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn rejects_untrusted_endpoints_and_invalid_encryption_keys() {
        let dir = tempfile::tempdir().unwrap();
        let service = PushService::new(&config(dir.path())).unwrap().unwrap();
        for endpoint in [
            "http://fcm.googleapis.com/test",
            "https://127.0.0.1/test",
            "https://fcm.googleapis.com.evil.example/test",
            "https://fcm.googleapis.com:8443/test",
            "https://user:password@fcm.googleapis.com/test",
            "https://fcm.googleapis.com/test#fragment",
            "invalid",
        ] {
            assert!(
                service.subscribe("alice", subscription(endpoint)).is_err(),
                "{endpoint}"
            );
        }
        let mut invalid = subscription("https://fcm.googleapis.com/test");
        invalid.keys.auth = "invalid".into();
        assert!(service.subscribe("alice", invalid).is_err());
        assert!(service.subscriptions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn builds_encrypted_vapid_request_with_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let service = PushService::new(&config(dir.path())).unwrap().unwrap();
        let subscription = subscription("https://fcm.googleapis.com/test");
        let request = web_push::request_builder::build_request::<Vec<u8>>(
            service.message(&subscription, b"private request").unwrap(),
        );
        assert_eq!(request.headers()["ttl"], "86400");
        assert_eq!(request.headers()["content-encoding"], "aes128gcm");
        let authorization = request.headers()["authorization"].to_str().unwrap();
        assert!(authorization.starts_with("vapid t="));
        assert!(authorization.ends_with(&service.public_key));
        assert!(
            !request
                .body()
                .windows(15)
                .any(|window| window == b"private request")
        );
        assert!(request.body().len() > 15);
    }

    #[tokio::test]
    async fn validates_configuration_and_leaves_push_unavailable_without_a_key() {
        assert_eq!(
            NotificationsConfig::default().transport,
            NotificationTransport::WebPush
        );
        assert!(PushService::new(&Config::default()).unwrap().is_none());
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        config.notifications.transport = NotificationTransport::Polling;
        assert!(PushService::new(&config).unwrap().is_none());
        config.notifications.transport = NotificationTransport::WebPush;
        config.notifications.vapid_subject = None;
        assert!(PushService::new(&config).is_err());
        config.notifications.vapid_subject = Some("ftp://example.com".into());
        assert!(PushService::new(&config).is_err());
        config.notifications.vapid_subject = Some("https://example.com".into());
        std::fs::write(
            config
                .notifications
                .vapid_private_key_path
                .as_ref()
                .unwrap(),
            "bad key",
        )
        .unwrap();
        assert!(PushService::new(&config).is_err());
    }

    #[tokio::test]
    async fn persistence_failure_does_not_change_live_subscriptions() {
        let dir = tempfile::tempdir().unwrap();
        let service = PushService::new(&config(dir.path())).unwrap().unwrap();
        std::fs::write(service.path.parent().unwrap(), "not a directory").unwrap();
        assert!(
            service
                .subscribe("alice", subscription("https://fcm.googleapis.com/test"))
                .is_err()
        );
        assert!(service.subscriptions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn honors_custom_push_host_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        config.notifications.push_hosts = vec!["push.example.com".into()];
        let service = PushService::new(&config).unwrap().unwrap();
        assert!(
            service
                .validate_endpoint("https://push.example.com/test")
                .is_ok()
        );
        assert!(
            service
                .validate_endpoint("https://fcm.googleapis.com/test")
                .is_err()
        );
    }
    struct MockClient {
        status: u16,
        sent: Mutex<Vec<String>>,
        signal: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl PushClient for MockClient {
        async fn send(&self, message: web_push::WebPushMessage) -> Result<u16> {
            self.sent.lock().unwrap().push(message.endpoint.to_string());
            self.signal.notify_one();
            Ok(self.status)
        }
    }

    fn mock(status: u16) -> Arc<MockClient> {
        Arc::new(MockClient {
            status,
            sent: Mutex::new(Vec::new()),
            signal: tokio::sync::Notify::new(),
        })
    }

    fn notification() -> Notification {
        Notification {
            id: 1,
            generation: "test-generation".into(),
            recipient: "ALICE".into(),
            author: "bot".into(),
            repository: "owner/repo".into(),
            location: "https://forge.example/issue/1".into(),
            message: "need help".into(),
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn routes_push_to_all_recipient_devices_and_removes_only_expired_endpoints() {
        for status in [201, 404, 410, 429, 500] {
            let dir = tempfile::tempdir().unwrap();
            let client = mock(status);
            let config = config(dir.path());
            let service = PushService::with_client(&config, client.clone())
                .unwrap()
                .unwrap();
            service
                .subscribe(
                    "alice",
                    subscription("https://fcm.googleapis.com/alice-one"),
                )
                .unwrap();
            service
                .subscribe(
                    "alice",
                    subscription("https://fcm.googleapis.com/alice-two"),
                )
                .unwrap();
            service
                .subscribe("bob", subscription("https://fcm.googleapis.com/bob"))
                .unwrap();
            service.deliver(&notification()).await;
            let sent = client.sent.lock().unwrap();
            assert_eq!(sent.len(), 2);
            assert!(sent.iter().all(|endpoint| endpoint.contains("alice")));
            let expected = if matches!(status, 404 | 410) { 1 } else { 3 };
            assert_eq!(service.subscriptions.lock().unwrap().len(), expected);
            assert_eq!(
                PushService::new(&config)
                    .unwrap()
                    .unwrap()
                    .subscriptions
                    .lock()
                    .unwrap()
                    .len(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn enqueue_delivers_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let client = mock(201);
        let service = PushService::with_client(&config(dir.path()), client.clone())
            .unwrap()
            .unwrap();
        service
            .subscribe("alice", subscription("https://fcm.googleapis.com/alice"))
            .unwrap();
        service.enqueue(notification());
        tokio::time::timeout(Duration::from_secs(2), client.signal.notified())
            .await
            .unwrap();
        assert_eq!(client.sent.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn http_client_sends_encrypted_headers_and_does_not_follow_redirects() {
        use axum::{
            Router,
            body::Bytes,
            http::{HeaderMap, StatusCode},
            routing::post,
        };
        let app = Router::new()
            .route(
                "/expired",
                post(|headers: HeaderMap, body: Bytes| async move {
                    assert_eq!(headers["ttl"], "86400");
                    assert_eq!(headers["content-encoding"], "aes128gcm");
                    assert!(
                        headers["authorization"]
                            .to_str()
                            .unwrap()
                            .starts_with("vapid t=")
                    );
                    assert!(!body.is_empty());
                    StatusCode::GONE
                }),
            )
            .route(
                "/redirect",
                post(|| async { (StatusCode::TEMPORARY_REDIRECT, [("location", "/expired")]) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let service = PushService::new(&config(dir.path())).unwrap().unwrap();
        let subscription = subscription("https://fcm.googleapis.com/test");
        for (path, expected) in [("expired", 410), ("redirect", 307)] {
            let mut message = service.message(&subscription, b"test payload").unwrap();
            // Exercise the HTTP adapter locally; service registration still requires HTTPS.
            message.endpoint = format!("http://{address}/{path}").parse().unwrap();
            assert_eq!(service.client.send(message).await.unwrap(), expected);
        }
        server.abort();
    }
}

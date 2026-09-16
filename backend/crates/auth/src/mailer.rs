//! The mail-delivery seam.
//!
//! The identity module needs to send exactly two kinds of mail — the address
//! verification link and the password-reset link — but *how* those messages reach
//! a human is a deployment decision, not a domain one. [`Mailer`] is the seam:
//! the service depends on the trait, and a deployment supplies an implementation.
//!
//! # What ships here
//!
//! [`InMemoryMailer`] is the development and test transport. It keeps every
//! message it is handed, so a test can assert on it and a developer can read the
//! link straight out of the process log. It sends nothing anywhere, which is
//! exactly what "no mail provider configured" should mean.
//!
//! # What a production deployment must supply
//!
//! An SMTP- or API-backed implementation of [`Mailer`] (for example a thin
//! wrapper over `lettre` or a transactional-mail HTTP API), injected through
//! [`crate::AuthServiceBuilder::mailer`]. That implementation is the only thing
//! that has to change: no use case in this crate knows a host name or a port.
//! The server currently refuses to start when `SMTP_URL` is set, because shipping
//! a build that silently *pretends* to deliver password-reset mail would be a
//! security failure, not a missing feature.
//!
//! # Why sending never blocks a request
//!
//! [`crate::AuthService`] hands a message to a `tokio::spawn`ed task and returns
//! to the request immediately. A slow provider therefore cannot make a
//! registration — or a password-reset request — hang; the worst case is a log line
//! saying the delivery failed.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::Notify;

/// A boxed, sendable future.
///
/// The trait must be usable from async code and object-safe (`Arc<dyn Mailer>`),
/// so it returns a boxed future rather than using `async fn` in the trait. One
/// allocation per message is invisible beside the network round trip a real
/// provider makes.
pub type MailFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One message to deliver.
///
/// Plain text only: both messages are a single link plus a sentence, and a plain
/// body is what every provider accepts without a template engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    /// Recipient address, already normalised to lowercase.
    pub to: String,
    /// Subject line, already localised.
    pub subject: String,
    /// Plain-text body, already localised; contains the link.
    pub text: String,
}

/// A delivery failure.
///
/// Deliberately opaque at the seam: the service logs it and moves on, and it must
/// never leak the link or the recipient into an HTTP response.
#[derive(Debug, thiserror::Error)]
#[error("mail delivery failed: {0}")]
pub struct MailError(pub String);

impl MailError {
    /// Wrap a provider error message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Delivers an [`OutgoingMessage`].
///
/// Implementations must be safe to share across tasks (the service holds them in
/// an `Arc`) and must not panic — a failed send is a `MailError`, never a crash.
pub trait Mailer: Send + Sync {
    /// Deliver one message.
    fn send<'a>(&'a self, message: OutgoingMessage) -> MailFuture<'a, Result<(), MailError>>;
}

/// The development and test transport: keeps messages, sends nothing.
///
/// Every captured message is also logged, which is how a developer running the
/// server locally "sees" the mail without a provider — the verification or reset
/// link is in the log. Tests do not rely on the log; they read
/// [`InMemoryMailer::messages`].
#[derive(Debug, Default)]
pub struct InMemoryMailer {
    messages: Mutex<Vec<OutgoingMessage>>,
    /// Woken whenever a message arrives, so [`Self::wait_for_count`] is a bounded
    /// await rather than a polling sleep.
    notify: Notify,
}

impl InMemoryMailer {
    /// Build an empty mailbox.
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of every captured message, oldest first.
    pub fn messages(&self) -> Vec<OutgoingMessage> {
        self.lock().clone()
    }

    /// How many messages have been captured.
    pub fn count(&self) -> usize {
        self.lock().len()
    }

    /// Every captured message addressed to `to` (case-insensitively).
    pub fn messages_to(&self, to: &str) -> Vec<OutgoingMessage> {
        self.lock()
            .iter()
            .filter(|message| message.to.eq_ignore_ascii_case(to))
            .cloned()
            .collect()
    }

    /// The most recent message addressed to `to`, if any.
    pub fn latest_to(&self, to: &str) -> Option<OutgoingMessage> {
        self.messages_to(to).pop()
    }

    /// Forget everything captured.
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Wait, up to `timeout`, for at least `expected` messages to have arrived.
    ///
    /// Returns whether the count was reached. This is the deterministic handshake
    /// tests use: dispatch happens on a spawned task, so a test waits for the
    /// mailbox to fill instead of sleeping a guessed interval.
    pub async fn wait_for_count(&self, expected: usize, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            if self.count() >= expected {
                return true;
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }

            // Register for the wake-up *before* re-checking, so a message that
            // arrives between the check and the await is not missed.
            let notified = self.notify.notified();
            if self.count() >= expected {
                return true;
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return false;
            }
        }
    }

    /// Wait for one more message than `seen` and return the new messages.
    ///
    /// Convenience for the common test shape "act, then wait for exactly the mail
    /// that act produced".
    pub async fn wait_for_more(&self, seen: usize, timeout: Duration) -> Vec<OutgoingMessage> {
        if self.wait_for_count(seen + 1, timeout).await {
            self.messages().split_off(seen)
        } else {
            Vec::new()
        }
    }

    /// The first `http(s)` URL in a captured message's body.
    ///
    /// Both messages carry exactly one link; this is how a test "follows" it.
    pub fn first_link(message: &OutgoingMessage) -> Option<String> {
        let start = message.text.find("http")?;
        let rest = &message.text[start..];
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());

        Some(rest[..end].to_owned())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<OutgoingMessage>> {
        // Nothing here panics while holding the lock, so poisoning would mean a
        // panic elsewhere; the mailbox is still consistent and the service must
        // not take the process down over a test transport.
        self.messages
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

impl Mailer for InMemoryMailer {
    fn send<'a>(&'a self, message: OutgoingMessage) -> MailFuture<'a, Result<(), MailError>> {
        Box::pin(async move {
            // Visible to a developer running the server: the link is right here.
            // A production transport must NOT log bodies (they contain tokens).
            tracing::info!(
                to = %message.to,
                subject = %message.subject,
                "development mail transport: captured an outbound email (not delivered)"
            );
            tracing::info!(body = %message.text, "captured email body");

            self.lock().push(message);
            self.notify.notify_waiters();

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{InMemoryMailer, Mailer, OutgoingMessage};

    fn message(to: &str, text: &str) -> OutgoingMessage {
        OutgoingMessage {
            to: to.to_owned(),
            subject: "验证你的邮箱".to_owned(),
            text: text.to_owned(),
        }
    }

    #[tokio::test]
    async fn captured_messages_are_readable_and_addressable() {
        let mailer = InMemoryMailer::new();

        mailer
            .send(message(
                "alice@example.com",
                "点击 https://jiuyue.test/x 完成验证",
            ))
            .await
            .expect("the in-memory transport cannot fail");

        assert_eq!(mailer.count(), 1);
        assert_eq!(mailer.messages_to("ALICE@example.com").len(), 1);
        assert!(mailer.messages_to("bob@example.com").is_empty());
        assert_eq!(
            InMemoryMailer::first_link(&mailer.latest_to("alice@example.com").expect("captured")),
            Some("https://jiuyue.test/x".to_owned())
        );

        mailer.clear();
        assert_eq!(mailer.count(), 0);
    }

    #[tokio::test]
    async fn waiting_for_a_message_resolves_when_it_arrives() {
        let mailer = InMemoryMailer::new();

        let delivery = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            mailer
                .send(message("alice@example.com", "body"))
                .await
                .expect("send");
        };

        let wait = mailer.wait_for_count(1, Duration::from_secs(2));
        let (_, reached) = tokio::join!(delivery, wait);

        assert!(reached, "the wait must be woken by the arriving message");
    }

    #[tokio::test]
    async fn waiting_times_out_when_nothing_arrives() {
        let mailer = InMemoryMailer::new();

        assert!(!mailer.wait_for_count(1, Duration::from_millis(10)).await);
    }
}

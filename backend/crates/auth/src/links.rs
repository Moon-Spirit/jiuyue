//! The two transactional emails this module sends, and the links they carry.
//!
//! Copy lives here rather than inline in the use cases so there is one place to
//! translate: every user-visible string is a plain-text constant or a small
//! format, and nothing about how a message is *delivered* leaks in. The service
//! decides **when** to send; this module decides **what** the mail says.
//!
//! Both bodies are Chinese, matching the product's Chinese-first stance. When an
//! i18n layer lands, each message becomes a per-locale lookup; until then the
//! strings are already isolated behind these functions.

use std::time::Duration;

use crate::mailer::OutgoingMessage;
use crate::repository::UserRow;

/// How the product signs its mail.
pub const PRODUCT_NAME: &str = "jiuyue · 九月";

/// The frontend route that redeems a verification link.
pub const VERIFY_EMAIL_PATH: &str = "/verify-email";

/// The frontend route that redeems a password-reset link.
pub const RESET_PASSWORD_PATH: &str = "/reset-password";

/// Build the absolute URL that goes into a verification email.
///
/// The token is base64url without padding, so it needs no percent-encoding.
pub fn verification_url(base_url: &str, token: &str) -> String {
    format!(
        "{}{VERIFY_EMAIL_PATH}?token={token}",
        base_url.trim_end_matches('/')
    )
}

/// Build the absolute URL that goes into a password-reset email.
pub fn reset_url(base_url: &str, token: &str) -> String {
    format!(
        "{}{RESET_PASSWORD_PATH}?token={token}",
        base_url.trim_end_matches('/')
    )
}

/// A human phrase for a link's lifetime, e.g. `24 小时` or `30 分钟`.
///
/// The copy promises a lifetime, so it must be derived from the configured TTL
/// rather than written as a literal that could drift away from it.
pub fn human_ttl(ttl: Duration) -> String {
    let seconds = ttl.as_secs();

    if seconds >= 3600 && seconds % 3600 == 0 {
        format!("{} 小时", seconds / 3600)
    } else if seconds >= 60 && seconds % 60 == 0 {
        format!("{} 分钟", seconds / 60)
    } else {
        format!("{seconds} 秒")
    }
}

/// The "verify your address" email.
pub fn verification_message(
    user: &UserRow,
    base_url: &str,
    token: &str,
    ttl: Duration,
) -> OutgoingMessage {
    let link = verification_url(base_url, token);
    let name = display_name(user);

    OutgoingMessage {
        to: user.email.clone(),
        subject: format!("验证你的邮箱，完成 {PRODUCT_NAME} 注册"),
        text: format!(
            "你好，{name}：\n\
             \n\
             感谢注册 {PRODUCT_NAME}。请点击下面的链接验证你的邮箱地址，完成账号激活：\n\
             \n\
             {link}\n\
             \n\
             链接在 {ttl} 内有效，并且只能使用一次。\n\
             如果这不是你本人的操作，忽略这封邮件即可，不会有任何影响。\n\
             \n\
             —— {PRODUCT_NAME}\n",
            ttl = human_ttl(ttl),
        ),
    }
}

/// The "reset your password" email.
pub fn reset_message(
    user: &UserRow,
    base_url: &str,
    token: &str,
    ttl: Duration,
) -> OutgoingMessage {
    let link = reset_url(base_url, token);
    let name = display_name(user);

    OutgoingMessage {
        to: user.email.clone(),
        subject: format!("重置你的 {PRODUCT_NAME} 密码"),
        text: format!(
            "你好，{name}：\n\
             \n\
             我们收到了重置这个账号密码的请求。点击下面的链接设置一个新密码：\n\
             \n\
             {link}\n\
             \n\
             链接在 {ttl} 内有效，并且只能使用一次。为安全起见，重置成功后所有设备都需要重新登录。\n\
             如果这不是你本人的操作，可以忽略这封邮件，你的密码不会改变。\n\
             \n\
             —— {PRODUCT_NAME}\n",
            ttl = human_ttl(ttl),
        ),
    }
}

/// The name a message greets the user by: their display name, or their `@handle`
/// when the display name is blank (which the registration path already prevents,
/// but a row edited elsewhere might not).
fn display_name(user: &UserRow) -> String {
    let name = user.display_name.trim();
    if name.is_empty() {
        user.username.clone()
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{human_ttl, reset_message, reset_url, verification_message, verification_url};
    use crate::repository::UserRow;

    fn user() -> UserRow {
        UserRow {
            id: "01JABC1234567890ABCDEFGHJ1".to_owned(),
            username: "alice".to_owned(),
            email: "alice@example.com".to_owned(),
            display_name: "Alice 爱丽丝".to_owned(),
            avatar_url: None,
            password_hash: Some("$argon2id$...".to_owned()),
            email_verified_at: None,
            created_at: sqlx::types::time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .expect("timestamp"),
        }
    }

    #[test]
    fn links_are_absolute_and_carry_the_token() {
        assert_eq!(
            verification_url("https://jiuyue.example/", "abc"),
            "https://jiuyue.example/verify-email?token=abc"
        );
        assert_eq!(
            reset_url("https://jiuyue.example", "xyz"),
            "https://jiuyue.example/reset-password?token=xyz"
        );
    }

    #[test]
    fn a_verification_message_greets_the_user_and_carries_one_link() {
        let message = verification_message(
            &user(),
            "https://jiuyue.example",
            "tok-1",
            Duration::from_secs(24 * 3600),
        );

        assert_eq!(message.to, "alice@example.com");
        assert!(message.subject.contains("验证你的邮箱"));
        assert!(message.text.contains("Alice 爱丽丝"));
        assert!(
            message
                .text
                .contains("https://jiuyue.example/verify-email?token=tok-1")
        );
        assert!(
            message.text.contains("24 小时"),
            "the copy names the real TTL"
        );
        assert!(message.text.contains("只能使用一次"));
    }

    #[test]
    fn a_reset_message_names_the_new_lifetime_and_the_link() {
        let message = reset_message(
            &user(),
            "https://jiuyue.example",
            "tok-2",
            Duration::from_secs(30 * 60),
        );

        assert!(message.subject.contains("重置"));
        assert!(
            message
                .text
                .contains("https://jiuyue.example/reset-password?token=tok-2")
        );
        assert!(message.text.contains("30 分钟"));
        assert!(message.text.contains("所有设备都需要重新登录"));
    }

    #[test]
    fn the_lifetime_phrase_is_derived_from_the_duration() {
        assert_eq!(human_ttl(Duration::from_secs(3600)), "1 小时");
        assert_eq!(human_ttl(Duration::from_secs(24 * 3600)), "24 小时");
        assert_eq!(human_ttl(Duration::from_secs(90)), "90 秒");
        assert_eq!(human_ttl(Duration::from_secs(5)), "5 秒");
    }
}

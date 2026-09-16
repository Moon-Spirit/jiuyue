//! Request validation — the authoritative copy of the registration rules.
//!
//! The frontend mirrors these rules for immediate feedback, but this module is
//! the one that decides. Every rejection is expressed as a [`FieldError`], never
//! as a formatted string, so the client can attach it to the right input and
//! branch on a stable code.
//!
//! Normalisation happens *before* validation and is part of the rule:
//!
//! - email and username are trimmed and lowercased, matching the `users` table's
//!   `CHECK (email = lower(email))` and its lowercase unique indexes. A user who
//!   types `Alice@Example.com` keeps their account; they simply cannot create a
//!   second one by changing the case.
//! - a blank display name means "use the username", which keeps the table's
//!   `char_length(display_name) BETWEEN 1 AND 64` check satisfied.

use jiuyue_contract::{FieldError, FieldErrorCode, LoginRequest, RegisterRequest};

/// Shortest accepted `@handle`, matching the `users_username_format` check.
pub const USERNAME_MIN_CHARS: usize = 3;
/// Longest accepted `@handle`, matching the `users_username_format` check.
pub const USERNAME_MAX_CHARS: usize = 32;
/// Shortest accepted password.
pub const PASSWORD_MIN_CHARS: usize = 8;
/// Longest accepted password, so a hostile body cannot force unbounded hashing.
pub const PASSWORD_MAX_CHARS: usize = 128;
/// Longest accepted email; also RFC 5321's practical limit.
pub const EMAIL_MAX_CHARS: usize = 254;
/// Longest accepted display name, matching the `users_display_name_length` check.
pub const DISPLAY_NAME_MAX_CHARS: usize = 64;

/// Trim and lowercase an email into its stored form.
pub fn normalize_email(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// Trim and lowercase a username into its stored form.
pub fn normalize_username(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// Resolve the display name actually stored for an account.
///
/// A blank or absent request value falls back to the username so the database
/// check for a 1–64 character name can never be violated by a valid submission.
pub fn display_name(requested: Option<&str>, username: &str) -> String {
    match requested.map(str::trim) {
        Some(value) if !value.is_empty() => value.to_owned(),
        _ => username.to_owned(),
    }
}

/// Validate a registration request, returning every problem found.
///
/// All fields are checked in one pass so the client can highlight everything at
/// once instead of fixing one error per round trip.
pub fn validate_registration(request: &RegisterRequest) -> Vec<FieldError> {
    let mut errors = Vec::new();

    let username = normalize_username(&request.username);
    push(&mut errors, check_username(&username));

    let email = normalize_email(&request.email);
    push(&mut errors, check_email(&email));

    push(&mut errors, check_password(&request.password));

    if let Some(requested) = request.display_name.as_deref() {
        push(&mut errors, check_display_name(requested));
    }

    errors
}

/// Validate a login request.
///
/// Email format is checked here too, so a typo comes back as a field error rather
/// than a generic "wrong credentials" that gives the user nothing to act on. The
/// check reveals nothing about which accounts exist.
pub fn validate_login(request: &LoginRequest) -> Vec<FieldError> {
    let mut errors = Vec::new();

    let email = normalize_email(&request.email);
    push(&mut errors, check_email(&email));

    if request.password.is_empty() {
        errors.push(problem("password", FieldErrorCode::Required, "请输入密码"));
    }

    errors
}

/// Whether a normalised email matches the accepted shape.
///
/// Deliberately structural rather than RFC-complete: exactly one `@`, both sides
/// non-empty, no whitespace, and a dotted domain. The frontend applies the
/// equivalent `^[^\s@]+@[^\s@]+\.[^\s@]+$` rule.
pub fn is_valid_email(email: &str) -> bool {
    if email.is_empty() || email.chars().count() > EMAIL_MAX_CHARS {
        return false;
    }
    if email.chars().any(char::is_whitespace) {
        return false;
    }

    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };

    if local.is_empty() || domain.is_empty() || !domain.contains('.') {
        return false;
    }
    // No empty label anywhere, so "a@.com", "a@b." and "a@b..c" are all refused.
    // The frontend's mirror applies the same rule to the same cases.
    if domain.split('.').any(str::is_empty) {
        return false;
    }

    true
}

/// Whether a normalised username matches `^[a-z0-9_]{3,32}$`.
pub fn is_valid_username(username: &str) -> bool {
    let count = username.chars().count();
    (USERNAME_MIN_CHARS..=USERNAME_MAX_CHARS).contains(&count)
        && username
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Whether a password satisfies the strength policy (length plus letter+digit).
pub fn is_strong_password(password: &str) -> bool {
    let count = password.chars().count();
    if !(PASSWORD_MIN_CHARS..=PASSWORD_MAX_CHARS).contains(&count) {
        return false;
    }
    password.chars().any(|c| c.is_ascii_alphabetic())
        && password.chars().any(|c| c.is_ascii_digit())
}

fn check_username(username: &str) -> Option<FieldError> {
    if username.is_empty() {
        return Some(problem(
            "username",
            FieldErrorCode::Required,
            "请输入用户名",
        ));
    }

    let count = username.chars().count();
    if count < USERNAME_MIN_CHARS {
        return Some(problem(
            "username",
            FieldErrorCode::TooShort,
            "用户名至少 3 个字符",
        ));
    }
    if count > USERNAME_MAX_CHARS {
        return Some(problem(
            "username",
            FieldErrorCode::TooLong,
            "用户名最多 32 个字符",
        ));
    }
    if !is_valid_username(username) {
        return Some(problem(
            "username",
            FieldErrorCode::InvalidFormat,
            "用户名只能包含小写字母、数字和下划线",
        ));
    }

    None
}

fn check_email(email: &str) -> Option<FieldError> {
    if email.is_empty() {
        return Some(problem("email", FieldErrorCode::Required, "请输入邮箱"));
    }
    if !is_valid_email(email) {
        return Some(problem(
            "email",
            FieldErrorCode::InvalidFormat,
            "邮箱格式不正确",
        ));
    }

    None
}

fn check_password(password: &str) -> Option<FieldError> {
    if password.is_empty() {
        return Some(problem("password", FieldErrorCode::Required, "请输入密码"));
    }

    let count = password.chars().count();
    if count < PASSWORD_MIN_CHARS {
        return Some(problem(
            "password",
            FieldErrorCode::TooShort,
            "密码至少 8 个字符",
        ));
    }
    if count > PASSWORD_MAX_CHARS {
        return Some(problem(
            "password",
            FieldErrorCode::TooLong,
            "密码最多 128 个字符",
        ));
    }
    if !is_strong_password(password) {
        return Some(problem(
            "password",
            FieldErrorCode::Weak,
            "密码需同时包含字母和数字",
        ));
    }

    None
}

fn check_display_name(display_name: &str) -> Option<FieldError> {
    let trimmed = display_name.trim();
    // Blank means "use the username", which `display_name` resolves later.
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() > DISPLAY_NAME_MAX_CHARS {
        return Some(problem(
            "display_name",
            FieldErrorCode::TooLong,
            "昵称最多 64 个字符",
        ));
    }

    None
}

fn problem(field: &str, code: FieldErrorCode, message: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        code,
        message: message.to_owned(),
    }
}

fn push(errors: &mut Vec<FieldError>, problem: Option<FieldError>) {
    if let Some(problem) = problem {
        errors.push(problem);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        display_name, is_strong_password, is_valid_email, is_valid_username, normalize_email,
        normalize_username, validate_login, validate_registration,
    };
    use jiuyue_contract::{FieldErrorCode, LoginRequest, RegisterRequest};

    fn registration(username: &str, email: &str, password: &str) -> RegisterRequest {
        RegisterRequest {
            username: username.to_owned(),
            email: email.to_owned(),
            password: password.to_owned(),
            display_name: None,
        }
    }

    /// The (field, code) pairs a validation run produced.
    fn problems(request: &RegisterRequest) -> Vec<(String, FieldErrorCode)> {
        validate_registration(request)
            .into_iter()
            .map(|error| (error.field, error.code))
            .collect()
    }

    #[test]
    fn a_well_formed_registration_has_no_problems() {
        let request = registration("alice", "alice@example.com", "secret123");

        assert!(validate_registration(&request).is_empty());
    }

    #[test]
    fn normalisation_lowercases_and_trims() {
        assert_eq!(normalize_email("  Alice@Example.COM "), "alice@example.com");
        assert_eq!(normalize_username("  Alice_01  "), "alice_01");
    }

    #[test]
    fn uppercase_input_is_normalised_not_rejected() {
        let request = registration("Alice", "Alice@EXAMPLE.com", "secret123");

        assert!(
            validate_registration(&request).is_empty(),
            "case must be normalised away, not treated as invalid"
        );
    }

    #[test]
    fn username_length_and_alphabet_rules_are_enforced() {
        assert_eq!(
            problems(&registration("ab", "a@example.com", "secret123")),
            [("username".to_owned(), FieldErrorCode::TooShort)]
        );

        let too_long = "a".repeat(33);
        assert_eq!(
            problems(&registration(&too_long, "a@example.com", "secret123")),
            [("username".to_owned(), FieldErrorCode::TooLong)]
        );

        for invalid in ["has space", "has-dash", "has.dot", "ééé"] {
            assert_eq!(
                problems(&registration(invalid, "a@example.com", "secret123")),
                [("username".to_owned(), FieldErrorCode::InvalidFormat)],
                "`{invalid}` must be rejected as a format problem"
            );
        }

        assert!(is_valid_username("alice_01"));
        assert!(!is_valid_username("ab"), "too short");
        assert!(!is_valid_username("UPPER"), "only lowercase is stored");
    }

    #[test]
    fn email_shape_rules_are_enforced() {
        assert!(is_valid_email("alice@example.com"));
        assert!(!is_valid_email("alice@example"));
        assert!(!is_valid_email("alice@@example.com"));
        assert!(!is_valid_email("@example.com"));
        assert!(!is_valid_email("alice@.com"));
        assert!(!is_valid_email("alice@example."));
        assert!(!is_valid_email("alice@example..com"));
        assert!(!is_valid_email("alice example@example.com"));

        assert_eq!(
            problems(&registration("alice", "not-an-email", "secret123")),
            [("email".to_owned(), FieldErrorCode::InvalidFormat)]
        );
    }

    #[test]
    fn password_policy_requires_length_letters_and_digits() {
        assert!(is_strong_password("secret123"));
        assert!(!is_strong_password("short1"));
        assert!(!is_strong_password("allletters"));
        assert!(!is_strong_password("12345678"));

        assert_eq!(
            problems(&registration("alice", "a@example.com", "short1")),
            [("password".to_owned(), FieldErrorCode::TooShort)]
        );
        assert_eq!(
            problems(&registration("alice", "a@example.com", "allletters")),
            [("password".to_owned(), FieldErrorCode::Weak)]
        );

        let too_long = format!("{}1", "a".repeat(128));
        assert_eq!(
            problems(&registration("alice", "a@example.com", &too_long)),
            [("password".to_owned(), FieldErrorCode::TooLong)]
        );
    }

    #[test]
    fn every_broken_field_is_reported_in_one_pass() {
        let request = registration("A", "nope", "short");

        let found: Vec<String> = validate_registration(&request)
            .into_iter()
            .map(|error| error.field)
            .collect();

        assert_eq!(found, ["username", "email", "password"]);
    }

    #[test]
    fn display_name_falls_back_to_the_username_when_blank() {
        assert_eq!(display_name(None, "alice"), "alice");
        assert_eq!(display_name(Some("   "), "alice"), "alice");
        assert_eq!(
            display_name(Some(" Alice Liddell "), "alice"),
            "Alice Liddell"
        );
    }

    #[test]
    fn an_overlong_display_name_is_rejected() {
        let mut request = registration("alice", "a@example.com", "secret123");
        request.display_name = Some("n".repeat(65));

        assert_eq!(
            problems(&request),
            [("display_name".to_owned(), FieldErrorCode::TooLong)]
        );
    }

    #[test]
    fn login_requires_a_well_formed_email_and_a_password() {
        let request = LoginRequest {
            email: "  Alice@Example.com ".to_owned(),
            password: String::new(),
        };

        let found = validate_login(&request);
        assert_eq!(found.len(), 1, "only the empty password is a problem here");
        assert_eq!(found[0].field, "password");
        assert_eq!(found[0].code, FieldErrorCode::Required);

        let malformed = LoginRequest {
            email: "nope".to_owned(),
            password: "secret123".to_owned(),
        };
        assert_eq!(
            validate_login(&malformed)[0].code,
            FieldErrorCode::InvalidFormat
        );
    }
}

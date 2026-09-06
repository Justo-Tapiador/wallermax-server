//! Authentication primitives: password hashing and JWT access tokens.
//!
//! Two building blocks live here, deliberately free of HTTP concerns so
//! they can be reused by any transport:
//!
//! - **Password hashing** with Argon2id (the winner of the Password Hashing
//!   Competition) using the recommended default parameters (19 MiB memory,
//!   2 iterations). Hashes are self-contained PHC strings that embed the
//!   salt and the parameters, so verification needs no extra state.
//! - **Access tokens** as JWTs signed with HMAC-SHA256. The [`JwtService`]
//!   owns the signing keys and the validation policy (expected issuer,
//!   expiry with 30 seconds of clock-skew leeway).
//!
//! Tokens carry the user id (`sub`), username, role (`admin` or `user`),
//! issue time, expiry and issuer. The stateless design lets any worker
//! verify requests without touching the database; handlers that need the
//! full record (or must react to deleted accounts) re-query the repository.

use std::time::{SystemTime, UNIX_EPOCH};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::User;

/// Minimum length of the JWT signing secret, in bytes.
///
/// HMAC-SHA256 keys below this size lack enough entropy against brute
/// force; the configuration layer rejects them at load time.
pub const MIN_JWT_SECRET_LEN: usize = 32;

/// Maximum accepted password length, in bytes.
///
/// Argon2 memory usage is independent of the password length, but the cap
/// keeps the input pipeline bounded and stops clients from wasting CPU on
/// pointless multi-kilobybyte passwords.
pub const MAX_PASSWORD_LEN: usize = 128;

/// Minimum accepted username length.
pub const MIN_USERNAME_LEN: usize = 3;

/// Maximum accepted username length.
pub const MAX_USERNAME_LEN: usize = 32;

/// Clock-skew tolerance for token expiry, in seconds.
const TOKEN_LEEWAY_SECS: u64 = 30;

/// Random bytes behind every refresh token (base64url: 43 characters).
const REFRESH_TOKEN_BYTES: usize = 32;

/// Maximum accepted refresh token length, in bytes.
///
/// Honest tokens are 43 bytes; the bound only rejects absurd inputs
/// before they reach the database layer.
pub const MAX_REFRESH_TOKEN_LEN: usize = 512;

/// Errors produced by the auth primitives.
///
/// Details are logged server-side but never returned to clients verbatim
/// (login failures are intentionally generic).
#[derive(Debug)]
pub enum AuthError {
    /// Password hashing failed at the crypto layer.
    Hash(String),
    /// Token signing or validation failed.
    Token(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Hash(message) | AuthError::Token(message) => {
                write!(formatter, "{message}")
            }
        }
    }
}

impl std::error::Error for AuthError {}

/// Hashes a password with Argon2id and a fresh random salt.
///
/// # Errors
///
/// Returns [`AuthError::Hash`] if the underlying Argon2 implementation
/// rejects the input (for example, exceeding its internal length limits).
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| AuthError::Hash(error.to_string()))
}

/// Verifies a password against a stored PHC string in constant time.
///
/// Returns `false` for a wrong password **and** for a malformed stored
/// hash: both mean "credentials rejected", and callers should not be able
/// to tell the two apart.
pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Generates a fresh opaque refresh token: 32 random bytes,
/// base64url-encoded without padding.
///
/// Only the SHA-256 hash of the value is persisted (see
/// [`hash_refresh_token`]); the value itself is shown to the client
/// exactly once, at issue time.
pub fn generate_refresh_token() -> String {
    let mut bytes = [0_u8; REFRESH_TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Hex-encoded SHA-256 digest of a refresh token value — the form
/// stored in (and looked up from) the database.
pub fn hash_refresh_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex(&digest)
}

/// Lowercase hex encoding of `bytes`.
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

/// Validates a registration username: 3-32 characters from `[A-Za-z0-9_-]`.
///
/// # Errors
///
/// Returns a human-readable explanation when the username does not comply.
pub fn validate_username(username: &str) -> Result<(), String> {
    if !(MIN_USERNAME_LEN..=MAX_USERNAME_LEN).contains(&username.len()) {
        return Err(format!(
            "username must be between {MIN_USERNAME_LEN} and {MAX_USERNAME_LEN} characters"
        ));
    }
    if !username
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
    {
        return Err("username may only contain letters, digits, `_` and `-`".to_owned());
    }
    Ok(())
}

/// Validates a password length against the configured minimum and the
/// global maximum.
///
/// # Errors
///
/// Returns a human-readable explanation when the password does not comply.
pub fn validate_password(password: &str, min_len: usize) -> Result<(), String> {
    if password.len() > MAX_PASSWORD_LEN {
        return Err(format!(
            "password must be at most {MAX_PASSWORD_LEN} characters"
        ));
    }
    if password.len() < min_len {
        return Err(format!("password must be at least {min_len} characters"));
    }
    Ok(())
}

/// JWT claims carried by wallermax access tokens.
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    /// Subject: the user id as a decimal string.
    pub sub: String,
    /// Username, kept for display purposes (`GET /api/auth/me`).
    pub username: String,
    /// Role granted by the token: `admin` or `user`.
    pub role: String,
    /// Issued-at, unix seconds.
    pub iat: u64,
    /// Expiry, unix seconds.
    pub exp: u64,
    /// Issuer, must match `[auth] issuer`.
    pub iss: String,
}

/// Signs and verifies the server's JWT access tokens.
///
/// One instance is created at startup from the `[auth]` configuration and
/// shared through the application state; it is immutable afterwards.
pub struct JwtService {
    encoding: EncodingKey,
    decoding: DecodingKey,
    validation: Validation,
    issuer: String,
    ttl_secs: u64,
}

impl JwtService {
    /// Builds the service from the `[auth]` configuration values.
    pub fn new(secret: &str, issuer: &str, token_ttl_secs: u64) -> Self {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[issuer]);
        validation.leeway = TOKEN_LEEWAY_SECS;

        Self {
            encoding: EncodingKey::from_secret(secret.as_bytes()),
            decoding: DecodingKey::from_secret(secret.as_bytes()),
            validation,
            issuer: issuer.to_owned(),
            ttl_secs: token_ttl_secs,
        }
    }

    /// Issues an access token for `user` starting at the current time.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Token`] if signing fails.
    pub fn issue_token(&self, user: &User) -> Result<String, AuthError> {
        self.issue_token_at(user, unix_now())
    }

    /// Issues an access token with an explicit issue time (unix seconds).
    ///
    /// Public with an injectable clock so tests (and embedders) can craft
    /// tokens with controlled lifetimes.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Token`] if signing fails.
    pub fn issue_token_at(&self, user: &User, now: u64) -> Result<String, AuthError> {
        let claims = Claims {
            sub: user.id.to_string(),
            username: user.username.clone(),
            role: user.role.as_str().to_owned(),
            iat: now,
            exp: now + self.ttl_secs,
            iss: self.issuer.clone(),
        };
        encode(&Header::default(), &claims, &self.encoding)
            .map_err(|error| AuthError::Token(error.to_string()))
    }

    /// Verifies a token's signature, expiry and issuer, returning its
    /// claims.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Token`] for invalid signatures, expired
    /// tokens, wrong issuers or malformed input.
    pub fn verify_token(&self, token: &str) -> Result<Claims, AuthError> {
        decode::<Claims>(token, &self.decoding, &self.validation)
            .map(|data| data.claims)
            .map_err(|error| AuthError::Token(error.to_string()))
    }
}

/// Current unix time in seconds (never panics, saturates at zero).
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::UserRole;

    /// Secret long enough to satisfy the minimum-length policy.
    const SECRET: &str = "unit-test-secret-0123456789abcdef0123456789";

    fn test_user() -> User {
        User {
            id: 42,
            username: String::from("alice"),
            password_hash: String::new(),
            role: UserRole::Admin,
            created_at: 0,
            last_login_at: None,
        }
    }

    #[test]
    fn password_hashing_roundtrip() {
        let hash = hash_password("correct horse battery staple").expect("hashing succeeds");

        assert!(verify_password("correct horse battery staple", &hash));
    }

    #[test]
    fn wrong_password_fails_verification() {
        let hash = hash_password("correct horse battery staple").expect("hashing succeeds");

        assert!(!verify_password("Tr0ub4dor&3", &hash));
    }

    #[test]
    fn malformed_hash_fails_closed() {
        assert!(!verify_password("password", "not-a-phc-string"));
        assert!(!verify_password("password", ""));
    }

    #[test]
    fn hashes_are_unique_and_phc_encoded() {
        let first = hash_password("same password").expect("first hash");
        let second = hash_password("same password").expect("second hash");

        // Fresh random salt per hash.
        assert_ne!(first, second);
        assert!(first.starts_with("$argon2id$"), "PHC format: {first}");
    }

    #[test]
    fn username_rules_are_enforced() {
        assert!(validate_username("alice").is_ok());
        assert!(validate_username("user-123_A").is_ok());

        assert!(validate_username("ab").is_err());
        assert!(validate_username(&"x".repeat(33)).is_err());
        assert!(validate_username("has spaces").is_err());
        assert!(validate_username("café").is_err());
    }

    #[test]
    fn password_rules_are_enforced() {
        assert!(validate_password("long enough", 8).is_ok());
        assert!(validate_password("short", 8).is_err());
        assert!(validate_password(&"x".repeat(MAX_PASSWORD_LEN + 1), 8).is_err());
    }

    #[test]
    fn token_roundtrip_carries_identity() {
        let service = JwtService::new(SECRET, "test-issuer", 3600);
        let token = service.issue_token(&test_user()).expect("token signs");

        let claims = service.verify_token(&token).expect("token verifies");
        assert_eq!(claims.sub, "42");
        assert_eq!(claims.username, "alice");
        assert_eq!(claims.role, "admin");
        assert_eq!(claims.iss, "test-issuer");
        assert!(claims.exp > claims.iat);
    }

    #[test]
    fn expired_tokens_are_rejected() {
        // Short-lived tokens issued 100 seconds ago expired 90+ seconds
        // before the verification leeway window.
        let service = JwtService::new(SECRET, "test-issuer", 10);
        let stale = service
            .issue_token_at(&test_user(), unix_now() - 100)
            .expect("token signs");

        assert!(service.verify_token(&stale).is_err());
    }

    #[test]
    fn tokens_signed_with_other_secrets_are_rejected() {
        let honest = JwtService::new(SECRET, "test-issuer", 3600);
        let attacker = JwtService::new(
            "another-secret-0123456789abcdef0123456789",
            "test-issuer",
            3600,
        );

        let forged = attacker.issue_token(&test_user()).expect("token signs");

        assert!(honest.verify_token(&forged).is_err());
    }

    #[test]
    fn tokens_from_other_issuers_are_rejected() {
        let service = JwtService::new(SECRET, "trusted-issuer", 3600);
        let stranger = JwtService::new(SECRET, "other-issuer", 3600);

        let token = stranger.issue_token(&test_user()).expect("token signs");

        assert!(service.verify_token(&token).is_err());
    }

    #[test]
    fn garbage_tokens_are_rejected() {
        let service = JwtService::new(SECRET, "test-issuer", 3600);

        for token in ["", "not-a-jwt", "a.b.c", "too.many.segments.here"] {
            assert!(service.verify_token(token).is_err(), "token: {token}");
        }
    }

    #[test]
    fn role_values_are_stable_strings() {
        assert_eq!(UserRole::Admin.as_str(), "admin");
        assert_eq!(UserRole::User.as_str(), "user");
        assert_eq!(UserRole::parse("admin"), Some(UserRole::Admin));
        assert_eq!(UserRole::parse("user"), Some(UserRole::User));
        assert_eq!(UserRole::parse("superuser"), None);
    }

    #[test]
    fn refresh_tokens_are_random_and_url_safe() {
        let first = generate_refresh_token();
        let second = generate_refresh_token();

        assert_eq!(first.len(), 43);
        assert_ne!(first, second);
        assert!(first
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'));
    }

    #[test]
    fn refresh_token_hashes_are_stable_and_hex() {
        let hash = hash_refresh_token("token-value");

        assert_eq!(hash.len(), 64);
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
        // Deterministic for the same input.
        assert_eq!(hash, hash_refresh_token("token-value"));
        assert_ne!(hash, hash_refresh_token("other-value"));
        // Matches the well-known SHA-256 test vector digest shape.
        assert_eq!(
            hash_refresh_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

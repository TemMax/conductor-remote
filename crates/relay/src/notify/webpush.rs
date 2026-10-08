//! VAPID keys, the JWT and aes128gcm payload encryption.
//!
//! Pure functions: no I/O beyond the OS random source. VAPID is RFC 8292, the payload
//! encryption RFC 8291 over a single RFC 8188 `aes128gcm` record.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{FieldBytes, PublicKey, SecretKey};
use sha2::Sha256;

/// The record size advertised in the body's header. One record carries the whole payload.
pub const RECORD_SIZE: u32 = 4096;
/// 4096 − (16 salt + 4 rs + 1 idlen + 65 key id) − (1 delimiter + 16 tag).
pub const MAX_PAYLOAD_BYTES: usize = 3993;
/// The JWT's `exp` is this many seconds after `now` (push services cap it at 24 hours).
pub const JWT_LIFETIME_SECS: u64 = 43_200;

const SALT_LEN: usize = 16;
const AUTH_LEN: usize = 16;
const POINT_LEN: usize = 65;
/// The delimiter that marks the only record as the last one.
const LAST_RECORD: u8 = 0x02;
const JWT_HEADER: &str = r#"{"typ":"JWT","alg":"ES256"}"#;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WebPushError {
    #[error("the push endpoint is not an https URL")]
    InvalidEndpoint,
    #[error("the subscription keys are not valid")]
    InvalidKey,
    #[error("the payload is {0} bytes, more than a push can carry")]
    PayloadTooLarge(usize),
    #[error("encryption failed")]
    Crypto,
}

/// The relay's VAPID key pair (P-256).
pub struct VapidKeys {
    secret: SecretKey,
}

impl VapidKeys {
    /// 32 random bytes from `getrandom::fill`, retried until they form a valid scalar.
    ///
    /// Panics only when the OS random source fails, which leaves nothing safe to do.
    pub fn generate() -> VapidKeys {
        let secret = random_secret().expect("the OS random source failed");
        VapidKeys { secret }
    }

    pub fn from_bytes(secret: &[u8]) -> Result<VapidKeys, WebPushError> {
        Ok(VapidKeys {
            secret: secret_from_bytes(secret).ok_or(WebPushError::InvalidKey)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes().into()
    }

    /// base64url (no padding) of the 65-byte uncompressed public point `0x04 || X || Y`.
    pub fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(uncompressed(&self.secret.public_key()))
    }
}

/// `scheme://host[:port]` of an https endpoint.
pub fn origin(endpoint: &str) -> Result<String, WebPushError> {
    let scheme = endpoint.get(..8).ok_or(WebPushError::InvalidEndpoint)?;
    if !scheme.eq_ignore_ascii_case("https://") {
        return Err(WebPushError::InvalidEndpoint);
    }
    let rest = &endpoint[8..];
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // An origin carries no user info.
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let (host, port) = match host_port.rsplit_once(':') {
        // A bracketed IPv6 host without a port has its colons inside the brackets.
        Some((host, port)) if !port.contains(']') => (host, Some(port)),
        _ => (host_port, None),
    };
    if host.is_empty() {
        return Err(WebPushError::InvalidEndpoint);
    }
    let host = host.to_ascii_lowercase();
    match port {
        None => Ok(format!("https://{host}")),
        Some(port) => {
            let number: u16 = port.parse().map_err(|_| WebPushError::InvalidEndpoint)?;
            if number == 443 {
                Ok(format!("https://{host}"))
            } else {
                Ok(format!("https://{host}:{number}"))
            }
        }
    }
}

/// `vapid t=<jwt>, k=<public key>`; JWT header `{"typ":"JWT","alg":"ES256"}`, claims
/// `{"aud":<origin>,"exp":<now_secs + 43200>,"sub":<subject>}` in that key order, signature the
/// raw 64-byte `r || s`, every part base64url without padding.
pub fn vapid_authorization(
    endpoint: &str,
    keys: &VapidKeys,
    subject: &str,
    now_secs: u64,
) -> Result<String, WebPushError> {
    let audience = origin(endpoint)?;
    let claims = serde_json::json!({
        "aud": audience,
        "exp": now_secs + JWT_LIFETIME_SECS,
        "sub": subject,
    });
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(JWT_HEADER),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    // ES256: ECDSA over SHA-256 of the signing input, deterministic nonce (RFC 6979).
    let signature: Signature = SigningKey::from(&keys.secret).sign(signing_input.as_bytes());
    // JOSE wants the fixed-size `r || s`, not DER.
    let raw: [u8; 64] = signature.to_bytes().into();
    Ok(format!(
        "vapid t={signing_input}.{}, k={}",
        URL_SAFE_NO_PAD.encode(raw),
        keys.public_key()
    ))
}

/// RFC 8291 encryption with a fresh ephemeral key and salt.
pub fn encrypt(p256dh: &str, auth: &str, payload: &[u8]) -> Result<Vec<u8>, WebPushError> {
    let ephemeral = random_secret().map_err(|_| WebPushError::Crypto)?;
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt).map_err(|_| WebPushError::Crypto)?;
    encrypt_with(p256dh, auth, payload, &ephemeral.to_bytes().into(), &salt)
}

/// The same with a given ephemeral secret and salt (for tests).
pub fn encrypt_with(
    p256dh: &str,
    auth: &str,
    payload: &[u8],
    ephemeral: &[u8; 32],
    salt: &[u8; 16],
) -> Result<Vec<u8>, WebPushError> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(WebPushError::PayloadTooLarge(payload.len()));
    }
    let ua_bytes = decode_exact::<POINT_LEN>(p256dh).ok_or(WebPushError::InvalidKey)?;
    let auth = decode_exact::<AUTH_LEN>(auth).ok_or(WebPushError::InvalidKey)?;
    if ua_bytes[0] != 0x04 {
        return Err(WebPushError::InvalidKey);
    }
    let ua_public = PublicKey::from_sec1_bytes(&ua_bytes).map_err(|_| WebPushError::InvalidKey)?;
    let as_secret = secret_from_bytes(ephemeral).ok_or(WebPushError::Crypto)?;
    let as_public = uncompressed(&as_secret.public_key());

    // 1. ECDH: the x coordinate of the shared point, 32 bytes.
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_public.as_affine());

    // 2. IKM, 32 bytes, bound to both public keys.
    let mut key_info = Vec::with_capacity(14 + 2 * POINT_LEN);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(&ua_bytes);
    key_info.extend_from_slice(&as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&auth), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|_| WebPushError::Crypto)?;

    // 3–4. Content-encryption key, 16 bytes, and nonce, 12 bytes.
    let content = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    content
        .expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| WebPushError::Crypto)?;
    let mut nonce = [0u8; 12];
    content
        .expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| WebPushError::Crypto)?;

    // 5. One record: payload || 0x02, no padding, no associated data; tag appended.
    let mut record = Vec::with_capacity(payload.len() + 1);
    record.extend_from_slice(payload);
    record.push(LAST_RECORD);
    let sealed = Aes128Gcm::new_from_slice(&cek)
        .map_err(|_| WebPushError::Crypto)?
        .encrypt(Nonce::from_slice(&nonce), record.as_slice())
        .map_err(|_| WebPushError::Crypto)?;

    // 6. salt(16) || rs(4, big-endian) || idlen(1) || keyid(65) || ciphertext || tag(16).
    let mut body = Vec::with_capacity(SALT_LEN + 4 + 1 + POINT_LEN + sealed.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(POINT_LEN as u8);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&sealed);
    Ok(body)
}

/// A P-256 secret from exactly 32 bytes that form a valid non-zero scalar.
fn secret_from_bytes(bytes: &[u8]) -> Option<SecretKey> {
    if bytes.len() != 32 {
        return None;
    }
    SecretKey::from_bytes(FieldBytes::from_slice(bytes)).ok()
}

fn random_secret() -> Result<SecretKey, getrandom::Error> {
    loop {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)?;
        if let Some(secret) = secret_from_bytes(&bytes) {
            return Ok(secret);
        }
    }
}

fn uncompressed(public: &PublicKey) -> [u8; POINT_LEN] {
    let point = public.to_encoded_point(false);
    let mut out = [0u8; POINT_LEN];
    out.copy_from_slice(point.as_bytes());
    out
}

fn decode_exact<const N: usize>(text: &str) -> Option<[u8; N]> {
    URL_SAFE_NO_PAD.decode(text).ok()?.try_into().ok()
}

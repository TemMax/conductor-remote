use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use conductor_remote::notify::webpush::{
    encrypt, encrypt_with, origin, vapid_authorization, VapidKeys, WebPushError, JWT_LIFETIME_SECS,
    MAX_PAYLOAD_BYTES, RECORD_SIZE,
};
use hkdf::Hkdf;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{FieldBytes, PublicKey, SecretKey};
use sha2::Sha256;

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(text: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD.decode(text).unwrap()
}

/// A user agent's key pair and auth secret, from fixed bytes.
struct Ua {
    secret: SecretKey,
    auth: [u8; 16],
}

impl Ua {
    fn new(secret: [u8; 32], auth: [u8; 16]) -> Ua {
        Ua {
            secret: SecretKey::from_bytes(FieldBytes::from_slice(&secret)).unwrap(),
            auth,
        }
    }

    fn public(&self) -> Vec<u8> {
        self.secret
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec()
    }

    fn p256dh(&self) -> String {
        b64(&self.public())
    }

    fn auth(&self) -> String {
        b64(&self.auth)
    }

    /// The receiving side of RFC 8291: rebuild the keys from the body's header and open the record.
    fn decrypt(&self, body: &[u8]) -> Vec<u8> {
        let salt = &body[..16];
        let record_size = u32::from_be_bytes(body[16..20].try_into().unwrap());
        assert_eq!(record_size, 4096);
        let id_len = usize::from(body[20]);
        assert_eq!(id_len, 65);
        let as_public_bytes = &body[21..21 + id_len];
        let as_public = PublicKey::from_sec1_bytes(as_public_bytes).unwrap();

        let shared =
            p256::ecdh::diffie_hellman(self.secret.to_nonzero_scalar(), as_public.as_affine());
        let mut key_info = b"WebPush: info\0".to_vec();
        key_info.extend_from_slice(&self.public());
        key_info.extend_from_slice(as_public_bytes);
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&self.auth), shared.raw_secret_bytes())
            .expand(&key_info, &mut ikm)
            .unwrap();
        let content = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let mut cek = [0u8; 16];
        content
            .expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .unwrap();
        let mut nonce = [0u8; 12];
        content
            .expand(b"Content-Encoding: nonce\0", &mut nonce)
            .unwrap();

        let mut plain = Aes128Gcm::new_from_slice(&cek)
            .unwrap()
            .decrypt(Nonce::from_slice(&nonce), &body[21 + id_len..])
            .unwrap();
        assert_eq!(plain.pop(), Some(0x02), "the only record ends with 0x02");
        plain
    }
}

fn test_ua() -> Ua {
    Ua::new([7u8; 32], [9u8; 16])
}

#[test]
fn rfc8291_example_reproduces_byte_for_byte() {
    let ua_secret = unb64("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94");
    let ua_public =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    let auth = "BTBZMqHH6r4Tts7J_aSIgg";
    let sender: [u8; 32] = unb64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")
        .try_into()
        .unwrap();
    let salt: [u8; 16] = unb64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
    let plaintext = b"When I grow up, I want to be a watermelon";
    let expected = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

    let body = encrypt_with(ua_public, auth, plaintext, &sender, &salt).unwrap();
    assert_eq!(b64(&body), expected);

    // The example's UA key pair opens it too.
    let ua = Ua::new(
        ua_secret.try_into().unwrap(),
        unb64(auth).try_into().unwrap(),
    );
    assert_eq!(ua.p256dh(), ua_public);
    assert_eq!(ua.decrypt(&body), plaintext);
}

#[test]
fn encrypt_round_trips_through_the_ua_key_pair() {
    let ua = test_ua();
    let payload = br#"{"title":"Done","body":"The turn ended"}"#;
    let body = encrypt(&ua.p256dh(), &ua.auth(), payload).unwrap();
    assert_eq!(body.len(), 16 + 4 + 1 + 65 + payload.len() + 1 + 16);
    assert_eq!(ua.decrypt(&body), payload);
}

#[test]
fn encrypt_uses_a_fresh_key_and_salt_each_time() {
    let ua = test_ua();
    let first = encrypt(&ua.p256dh(), &ua.auth(), b"same").unwrap();
    let second = encrypt(&ua.p256dh(), &ua.auth(), b"same").unwrap();
    assert_ne!(first[..16], second[..16], "salt");
    assert_ne!(first[21..86], second[21..86], "ephemeral public key");
}

#[test]
fn encrypt_with_is_deterministic() {
    let ua = test_ua();
    let ephemeral = [3u8; 32];
    let salt = [5u8; 16];
    let first = encrypt_with(&ua.p256dh(), &ua.auth(), b"hello", &ephemeral, &salt).unwrap();
    let second = encrypt_with(&ua.p256dh(), &ua.auth(), b"hello", &ephemeral, &salt).unwrap();
    assert_eq!(first, second);
    assert_eq!(ua.decrypt(&first), b"hello");
}

#[test]
fn body_header_carries_salt_record_size_and_ephemeral_key() {
    let ua = test_ua();
    let ephemeral = [3u8; 32];
    let salt = [5u8; 16];
    let body = encrypt_with(&ua.p256dh(), &ua.auth(), b"hello", &ephemeral, &salt).unwrap();

    assert_eq!(&body[..16], &salt);
    assert_eq!(&body[16..20], &RECORD_SIZE.to_be_bytes());
    assert_eq!(&body[16..20], &[0x00, 0x00, 0x10, 0x00]);
    assert_eq!(body[20], 65);
    let expected_public = SecretKey::from_bytes(FieldBytes::from_slice(&ephemeral))
        .unwrap()
        .public_key()
        .to_encoded_point(false);
    assert_eq!(&body[21..86], expected_public.as_bytes());
    assert_eq!(body[21], 0x04);
}

#[test]
fn payload_limit_is_3993_bytes() {
    let ua = test_ua();
    assert_eq!(MAX_PAYLOAD_BYTES, 3993);

    let fits = vec![b'a'; 3993];
    let body = encrypt(&ua.p256dh(), &ua.auth(), &fits).unwrap();
    assert_eq!(body.len(), 4096);
    assert_eq!(ua.decrypt(&body), fits);

    let too_large = vec![b'a'; 3994];
    assert_eq!(
        encrypt(&ua.p256dh(), &ua.auth(), &too_large),
        Err(WebPushError::PayloadTooLarge(3994))
    );
    assert_eq!(
        encrypt_with(&ua.p256dh(), &ua.auth(), &too_large, &[3u8; 32], &[5u8; 16]),
        Err(WebPushError::PayloadTooLarge(3994))
    );
}

#[test]
fn short_or_malformed_keys_are_invalid() {
    let ua = test_ua();
    let public = ua.public();

    let short_p256dh = b64(&public[..64]);
    assert_eq!(
        encrypt(&short_p256dh, &ua.auth(), b"x"),
        Err(WebPushError::InvalidKey)
    );

    let short_auth = b64(&[9u8; 15]);
    assert_eq!(
        encrypt(&ua.p256dh(), &short_auth, b"x"),
        Err(WebPushError::InvalidKey)
    );

    assert_eq!(
        encrypt("not base64!", &ua.auth(), b"x"),
        Err(WebPushError::InvalidKey)
    );

    let mut off_curve = public.clone();
    off_curve[64] ^= 1;
    assert_eq!(
        encrypt(&b64(&off_curve), &ua.auth(), b"x"),
        Err(WebPushError::InvalidKey)
    );
}

#[test]
fn origin_keeps_scheme_host_and_port_only() {
    assert_eq!(
        origin("https://push.example:8443/x/y").unwrap(),
        "https://push.example:8443"
    );
    assert_eq!(
        origin("https://fcm.googleapis.com/fcm/send/abc").unwrap(),
        "https://fcm.googleapis.com"
    );
    assert_eq!(
        origin("https://push.example").unwrap(),
        "https://push.example"
    );
}

#[test]
fn origin_refuses_anything_but_https() {
    assert_eq!(
        origin("http://push.example/x"),
        Err(WebPushError::InvalidEndpoint)
    );
    assert_eq!(origin("push.example/x"), Err(WebPushError::InvalidEndpoint));
    assert_eq!(origin("https:///x"), Err(WebPushError::InvalidEndpoint));
    assert_eq!(origin(""), Err(WebPushError::InvalidEndpoint));
}

#[test]
fn vapid_jwt_is_es256_over_the_exact_header_and_claims() {
    let keys = VapidKeys::generate();
    let now = 1_700_000_000;
    let authorization = vapid_authorization(
        "https://push.example:8443/x/y",
        &keys,
        "mailto:relay@example.invalid",
        now,
    )
    .unwrap();

    let rest = authorization.strip_prefix("vapid t=").unwrap();
    let (jwt, public_key) = rest.split_once(", k=").unwrap();
    assert_eq!(public_key, keys.public_key());

    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    for part in &parts {
        assert!(!part.contains('='), "no padding");
        assert!(URL_SAFE_NO_PAD.decode(part).is_ok(), "base64url");
    }

    let header = String::from_utf8(unb64(parts[0])).unwrap();
    assert_eq!(header, r#"{"typ":"JWT","alg":"ES256"}"#);

    let claims = String::from_utf8(unb64(parts[1])).unwrap();
    assert_eq!(
        claims,
        format!(
            r#"{{"aud":"https://push.example:8443","exp":{},"sub":"mailto:relay@example.invalid"}}"#,
            now + 43_200
        )
    );
    assert_eq!(JWT_LIFETIME_SECS, 43_200);

    let signature_bytes = unb64(parts[2]);
    assert_eq!(signature_bytes.len(), 64, "raw r || s, not DER");
    let signature = Signature::from_slice(&signature_bytes).unwrap();
    let verifying_key = VerifyingKey::from_sec1_bytes(&unb64(&keys.public_key())).unwrap();
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    verifying_key
        .verify(signing_input.as_bytes(), &signature)
        .unwrap();

    // A different claim does not verify under the same signature.
    let forged = format!("{}.{}", parts[0], b64(b"{}"));
    assert!(verifying_key.verify(forged.as_bytes(), &signature).is_err());
}

#[test]
fn vapid_authorization_refuses_http_endpoints() {
    let keys = VapidKeys::generate();
    assert_eq!(
        vapid_authorization("http://push.example/x", &keys, "mailto:a@b.invalid", 0),
        Err(WebPushError::InvalidEndpoint)
    );
}

#[test]
fn vapid_keys_round_trip_through_bytes() {
    let keys = VapidKeys::generate();
    let bytes = keys.to_bytes();
    let restored = VapidKeys::from_bytes(&bytes).unwrap();
    assert_eq!(restored.to_bytes(), bytes);
    assert_eq!(restored.public_key(), keys.public_key());

    let public = unb64(&keys.public_key());
    assert_eq!(public.len(), 65);
    assert_eq!(public[0], 0x04);

    assert_ne!(VapidKeys::generate().to_bytes(), bytes);
}

#[test]
fn vapid_keys_refuse_bad_secrets() {
    assert!(matches!(
        VapidKeys::from_bytes(&[1u8; 31]),
        Err(WebPushError::InvalidKey)
    ));
    assert!(matches!(
        VapidKeys::from_bytes(&[0u8; 32]),
        Err(WebPushError::InvalidKey)
    ));
    assert!(matches!(
        VapidKeys::from_bytes(&[0xffu8; 32]),
        Err(WebPushError::InvalidKey)
    ));
}

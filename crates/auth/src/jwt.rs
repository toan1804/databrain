//! Minimal RS256 JWT signing (key-pair auth, Google service accounts) and
//! PEM private-key parsing. Hand-rolled to avoid `jsonwebtoken`'s MSRV.
//!
//! RSA is aws-lc-rs (constant-time, the same library rustls and russh use
//! here), not the `rsa` crate: its 0.9 line has a timing side channel in
//! private-key operations (RUSTSEC-2023-0071, no fixed release).

use aws_lc_rs::digest::{SHA256, digest};
use aws_lc_rs::encoding::{AsDer, PublicKeyX509Der};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use pkcs8::EncryptedPrivateKeyInfo;
use pkcs8::der::pem;

use crate::AuthError;

/// An RSA private key for signing.
pub struct RsaPrivateKey(RsaKeyPair);

impl std::fmt::Debug for RsaPrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RsaPrivateKey(..)")
    }
}

/// Parse an RSA private key from PEM: PKCS#8 (optionally encrypted) or PKCS#1.
pub fn parse_private_key(pem_text: &str, passphrase: Option<&str>) -> Result<RsaPrivateKey, AuthError> {
    let invalid = |e: &dyn std::fmt::Display| AuthError::Invalid(format!("invalid RSA private key (PEM PKCS#8 or PKCS#1 expected): {e}"));
    let (label, der) = pem::decode_vec(pem_text.trim().as_bytes()).map_err(|e| invalid(&e))?;
    let key = match label {
        "ENCRYPTED PRIVATE KEY" => {
            let pass = passphrase.filter(|p| !p.is_empty()).ok_or_else(|| AuthError::Invalid("the private key is encrypted; enter its passphrase".into()))?;
            let info = EncryptedPrivateKeyInfo::try_from(der.as_slice()).map_err(|e| invalid(&e))?;
            let plain = info.decrypt(pass.as_bytes()).map_err(|e| AuthError::Invalid(format!("cannot decrypt private key: {e}")))?;
            RsaKeyPair::from_pkcs8(plain.as_bytes())
        }
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
        other => return Err(invalid(&format!("unexpected PEM block \"{other}\""))),
    };
    key.map(RsaPrivateKey).map_err(|e| invalid(&e))
}

/// `SHA256:<base64>` fingerprint of the public key (Snowflake format): the
/// SHA-256 of its DER SubjectPublicKeyInfo.
pub fn public_key_fingerprint(key: &RsaPrivateKey) -> Result<String, AuthError> {
    let der: PublicKeyX509Der = key.0.public_key().as_der().map_err(|e| AuthError::Invalid(e.to_string()))?;
    Ok(format!("SHA256:{}", STANDARD.encode(digest(&SHA256, der.as_ref()))))
}

pub fn sign_rs256(key: &RsaPrivateKey, header: &serde_json::Value, claims: &serde_json::Value) -> Result<String, AuthError> {
    let h = URL_SAFE_NO_PAD.encode(serde_json::to_vec(header).map_err(|e| AuthError::Invalid(e.to_string()))?);
    let c = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).map_err(|e| AuthError::Invalid(e.to_string()))?);
    let input = format!("{h}.{c}");
    let mut sig = vec![0; key.0.public_modulus_len()];
    key.0
        .sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut sig)
        .map_err(|_| AuthError::Invalid("could not sign with the private key".into()))?;
    Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig)))
}

/// Decode (without verifying) the payload of a JWT, e.g. an OIDC ID token
/// received directly from the token endpoint over TLS. Used for display only.
pub fn decode_payload(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::encoding::Pkcs8V1Der;
    use aws_lc_rs::rsa::KeySize;
    use aws_lc_rs::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    use pkcs8::der::pem::LineEnding;

    fn pkcs8_pem(key: &RsaKeyPair) -> String {
        let der: Pkcs8V1Der = key.as_der().unwrap();
        pem::encode_string("PRIVATE KEY", LineEnding::LF, der.as_ref()).unwrap()
    }

    #[test]
    fn sign_and_verify() {
        let key = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
        let parsed = parse_private_key(&pkcs8_pem(&key), None).unwrap();
        let jwt = sign_rs256(&parsed, &serde_json::json!({"alg": "RS256", "typ": "JWT"}), &serde_json::json!({"sub": "u"})).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let sig = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public_key().as_ref()).verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
        assert_eq!(decode_payload(&jwt).unwrap()["sub"], "u");
        assert!(parse_private_key("garbage", None).is_err());
    }

    /// A known key (openssl genrsa 2048), as PKCS#1, PKCS#8 and encrypted
    /// PKCS#8: same fingerprint, the value `openssl rsa -pubout -outform DER
    /// | openssl dgst -sha256 -binary | base64` prints (Snowflake's
    /// RSA_PUBLIC_KEY_FP).
    #[test]
    fn reads_every_pem_form_with_snowflakes_fingerprint() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let read = |n: &str| std::fs::read_to_string(dir.join(n)).unwrap();
        let want = read("rsa.fingerprint").trim().to_string();
        for (file, pass) in [("rsa-pkcs1.pem", None), ("rsa-pkcs8.pem", None), ("rsa-pkcs8-encrypted.pem", Some("databrain"))] {
            let key = parse_private_key(&read(file), pass).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(public_key_fingerprint(&key).unwrap(), want, "{file}");
        }
        let encrypted = read("rsa-pkcs8-encrypted.pem");
        assert!(parse_private_key(&encrypted, None).unwrap_err().to_string().contains("passphrase"));
        assert!(parse_private_key(&encrypted, Some("wrong")).unwrap_err().to_string().contains("decrypt"));
    }
}

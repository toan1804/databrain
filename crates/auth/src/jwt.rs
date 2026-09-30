//! Minimal RS256 JWT signing (key-pair auth, Google service accounts) and
//! PEM private-key parsing. Hand-rolled to avoid `jsonwebtoken`'s MSRV.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePublicKey};
use rsa::sha2::{Digest, Sha256};
use rsa::signature::{SignatureEncoding, Signer};

use crate::AuthError;

/// Parse an RSA private key from PEM: PKCS#8 (optionally encrypted) or PKCS#1.
pub fn parse_private_key(pem: &str, passphrase: Option<&str>) -> Result<RsaPrivateKey, AuthError> {
    let pem = pem.trim();
    if pem.contains("ENCRYPTED PRIVATE KEY") {
        let pass = passphrase.filter(|p| !p.is_empty()).ok_or_else(|| {
            AuthError::Invalid("the private key is encrypted; enter its passphrase".into())
        })?;
        return RsaPrivateKey::from_pkcs8_encrypted_pem(pem, pass.as_bytes())
            .map_err(|e| AuthError::Invalid(format!("cannot decrypt private key: {e}")));
    }
    RsaPrivateKey::from_pkcs8_pem(pem)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
        .map_err(|e| {
            AuthError::Invalid(format!(
                "invalid RSA private key (PEM PKCS#8 or PKCS#1 expected): {e}"
            ))
        })
}

/// `SHA256:<base64>` fingerprint of the public key (Snowflake format).
pub fn public_key_fingerprint(key: &RsaPrivateKey) -> Result<String, AuthError> {
    let der = key
        .to_public_key()
        .to_public_key_der()
        .map_err(|e| AuthError::Invalid(e.to_string()))?;
    Ok(format!(
        "SHA256:{}",
        STANDARD.encode(Sha256::digest(der.as_bytes()))
    ))
}

pub fn sign_rs256(
    key: &RsaPrivateKey,
    header: &serde_json::Value,
    claims: &serde_json::Value,
) -> Result<String, AuthError> {
    let h = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(header).map_err(|e| AuthError::Invalid(e.to_string()))?);
    let c = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).map_err(|e| AuthError::Invalid(e.to_string()))?);
    let input = format!("{h}.{c}");
    let signer = SigningKey::<Sha256>::new(key.clone());
    let sig = signer.sign(input.as_bytes());
    Ok(format!(
        "{input}.{}",
        URL_SAFE_NO_PAD.encode(sig.to_bytes())
    ))
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
    use rsa::pkcs1v15::VerifyingKey;
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    use rsa::signature::Verifier;

    #[test]
    fn sign_and_verify() {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem = key.to_pkcs8_pem(LineEnding::LF).unwrap();
        let parsed = parse_private_key(&pem, None).unwrap();
        let jwt = sign_rs256(
            &parsed,
            &serde_json::json!({"alg": "RS256", "typ": "JWT"}),
            &serde_json::json!({"sub": "u"}),
        )
        .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let sig = rsa::pkcs1v15::Signature::try_from(
            URL_SAFE_NO_PAD.decode(parts[2]).unwrap().as_slice(),
        )
        .unwrap();
        VerifyingKey::<Sha256>::new(key.to_public_key())
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
        assert_eq!(decode_payload(&jwt).unwrap()["sub"], "u");
        assert!(
            public_key_fingerprint(&parsed)
                .unwrap()
                .starts_with("SHA256:")
        );
        assert!(parse_private_key("garbage", None).is_err());
    }
}

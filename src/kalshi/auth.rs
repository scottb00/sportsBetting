use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rsa::pss::SigningKey;
use rsa::signature::SignatureEncoding;
use rsa::signature::RandomizedSigner;
use rsa::RsaPrivateKey;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

/// Kalshi API authentication using RSA-PSS request signing.
///
/// Each request requires three headers:
/// - KALSHI-ACCESS-KEY: the API key ID
/// - KALSHI-ACCESS-TIMESTAMP: request timestamp in milliseconds
/// - KALSHI-ACCESS-SIGNATURE: base64(sign(timestamp + method + path))
#[derive(Clone)]
pub struct KalshiAuth {
    key_id: String,
    signing_key: SigningKey<Sha256>,
}

impl KalshiAuth {
    /// Create auth from a key ID and PEM-encoded private key string.
    pub fn new(key_id: String, private_key_pem: &str) -> Result<Self> {
        let private_key = rsa::pkcs8::DecodePrivateKey::from_pkcs8_pem(private_key_pem)
            .or_else(|_| {
                let pk: RsaPrivateKey =
                    rsa::pkcs1::DecodeRsaPrivateKey::from_pkcs1_pem(private_key_pem)
                        .context("Failed to parse private key (tried PKCS#8 and PKCS#1)")?;
                Ok::<RsaPrivateKey, anyhow::Error>(pk)
            })
            .context("Failed to parse RSA private key")?;

        let signing_key = SigningKey::<Sha256>::new(private_key);

        Ok(Self { key_id, signing_key })
    }

    /// Create auth from a key ID and path to a PEM private key file.
    pub fn from_file(key_id: String, private_key_path: &str) -> Result<Self> {
        let pem = std::fs::read_to_string(private_key_path)
            .with_context(|| format!("Failed to read private key from {}", private_key_path))?;
        Self::new(key_id, &pem)
    }

    /// Generate the auth headers for a given HTTP method and path.
    pub fn sign_request(&self, method: &str, path: &str) -> Result<AuthHeaders> {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("System time before UNIX epoch")?
            .as_millis();

        let timestamp_str = timestamp_ms.to_string();

        // Signing string: "{timestamp_ms}{METHOD}{path_without_query}"
        let path_without_query = path.split('?').next().unwrap_or(path);
        let signing_string = format!("{}{}{}", timestamp_str, method.to_uppercase(), path_without_query);

        let mut rng = rand::thread_rng();
        let signature = self.signing_key.sign_with_rng(&mut rng, signing_string.as_bytes());
        let signature_b64 = BASE64.encode(signature.to_bytes());

        Ok(AuthHeaders {
            key_id: self.key_id.clone(),
            timestamp: timestamp_str,
            signature: signature_b64,
        })
    }

    /// Generate auth headers for WebSocket connection.
    pub fn sign_websocket(&self) -> Result<AuthHeaders> {
        self.sign_request("GET", "/trade-api/ws/v2")
    }
}

#[derive(Debug, Clone)]
pub struct AuthHeaders {
    pub key_id: String,
    pub timestamp: String,
    pub signature: String,
}

impl AuthHeaders {
    pub fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .header("KALSHI-ACCESS-KEY", &self.key_id)
            .header("KALSHI-ACCESS-TIMESTAMP", &self.timestamp)
            .header("KALSHI-ACCESS-SIGNATURE", &self.signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_request_produces_headers() {
        let mut rng = rand::thread_rng();
        let private_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem = rsa::pkcs8::EncodePrivateKey::to_pkcs8_pem(&private_key, rsa::pkcs8::LineEnding::LF)
            .unwrap();

        let auth = KalshiAuth::new("test-key-id".to_string(), pem.as_ref()).unwrap();
        let headers = auth.sign_request("GET", "/trade-api/v2/portfolio/balance").unwrap();

        assert_eq!(headers.key_id, "test-key-id");
        assert!(!headers.timestamp.is_empty());
        assert!(!headers.signature.is_empty());
        BASE64.decode(&headers.signature).unwrap();
    }

    #[test]
    fn test_sign_websocket() {
        let mut rng = rand::thread_rng();
        let private_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem = rsa::pkcs8::EncodePrivateKey::to_pkcs8_pem(&private_key, rsa::pkcs8::LineEnding::LF)
            .unwrap();

        let auth = KalshiAuth::new("test-key-id".to_string(), pem.as_ref()).unwrap();
        let headers = auth.sign_websocket().unwrap();

        assert_eq!(headers.key_id, "test-key-id");
        assert!(!headers.signature.is_empty());
    }
}

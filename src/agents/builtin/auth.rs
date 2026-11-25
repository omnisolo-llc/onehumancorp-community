// SPDX-License-Identifier: Apache-2.0
// Copyright (c) OmniSolo LLC.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::env;
use std::sync::Arc;

pub use ::ohc_core::extensions::{FailClosedPeerVerifier, PeerIdentity, PeerIdentityVerifier};

type HmacSha256 = Hmac<Sha256>;

/// Authentication mode for the built-in agent gRPC service.
#[derive(Clone)]
pub enum AuthMode {
    /// No authentication (unit-test binaries only).
    Disabled,
    /// Pre-shared HMAC-SHA256 bearer token (open-source single-tenant default).
    Token {
        token_hash: Vec<u8>,
        verification_key: Vec<u8>,
    },
    /// Runtime-injected peer identity verifier (for example, external mTLS peer verification).
    PeerVerifier(Arc<dyn PeerIdentityVerifier>),
}

impl AuthMode {
    /// Constructs a fail-closed peer authentication mode that rejects requests
    /// until an edition-specific [`PeerIdentityVerifier`] is injected.
    pub fn fail_closed_peer(reason: impl Into<String>) -> Self {
        Self::PeerVerifier(Arc::new(FailClosedPeerVerifier::new(reason)))
    }
}

impl std::fmt::Debug for AuthMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("AuthMode::Disabled"),
            Self::Token { .. } => formatter.write_str("AuthMode::Token { <redacted> }"),
            Self::PeerVerifier(verifier) => formatter
                .debug_tuple("AuthMode::PeerVerifier")
                .field(verifier)
                .finish(),
        }
    }
}

/// Build an [`AuthMode`] from environment variables for local single-tenant operation.
///
///   OMNISOLO_AGENT_AUTH_DISABLED=true   – skip auth in unit-test binaries only
///   OMNISOLO_AGENT_TOKEN                – enables HMAC-SHA256 bearer token mode
///   OMNISOLO_AGENT_AUTH_KEY             – >= 32-byte HMAC key required with OMNISOLO_AGENT_TOKEN
pub fn auth_mode_from_env() -> Result<AuthMode, String> {
    auth_mode_from_env_with_verifier(None)
}

/// Build an [`AuthMode`] from environment variables, falling back to an injected
/// [`PeerIdentityVerifier`] when token auth is not configured.
pub fn auth_mode_from_env_with_verifier(
    peer_verifier: Option<Arc<dyn PeerIdentityVerifier>>,
) -> Result<AuthMode, String> {
    let auth_disabled = env::var("OMNISOLO_AGENT_AUTH_DISABLED")
        .is_ok_and(|value| value.trim().eq_ignore_ascii_case("true"));
    if auth_disabled {
        #[cfg(test)]
        {
            let environment = env::var("OMNISOLO_ENV").unwrap_or_default();
            if matches!(
                environment.trim().to_ascii_lowercase().as_str(),
                "development" | "test"
            ) {
                return Ok(AuthMode::Disabled);
            }
        }
        return Err(
            "OMNISOLO_AGENT_AUTH_DISABLED is not allowed in production binaries".to_string(),
        );
    }

    if let Ok(token) = env::var("OMNISOLO_AGENT_TOKEN")
        && !token.trim().is_empty()
    {
        let key = env::var("OMNISOLO_AGENT_AUTH_KEY")
            .map_err(|_| "OMNISOLO_AGENT_AUTH_KEY is required in token mode".to_string())?;
        if key.trim().is_empty() || key.len() < 32 {
            return Err("OMNISOLO_AGENT_AUTH_KEY must contain at least 32 bytes".to_string());
        }
        let verification_key = key.into_bytes();
        let token_hash = hmac_token(&token, &verification_key);
        return Ok(AuthMode::Token {
            token_hash,
            verification_key,
        });
    }

    if let Some(verifier) = peer_verifier {
        return Ok(AuthMode::PeerVerifier(verifier));
    }

    Err("configure OMNISOLO_AGENT_TOKEN or inject a PeerIdentityVerifier".to_string())
}

/// Check a bearer token against an expected HMAC hash.
/// Returns true if the token matches.
pub fn check_token(provided: &str, expected_hash: &[u8], key: &[u8]) -> bool {
    let Ok(mut mac) = HmacSha256::new_from_slice(key) else {
        return false;
    };
    mac.update(provided.as_bytes());
    mac.verify_slice(expected_hash).is_ok()
}

/// Compute HMAC-SHA256 of the token using the application key.
pub fn hmac_token(tok: &str, key: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any size");
    mac.update(tok.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Debug)]
    struct HeaderPeerVerifier {
        expected_principal: String,
    }

    impl PeerIdentityVerifier for HeaderPeerVerifier {
        fn verify_peer(&self, req: &tonic::Request<()>) -> Result<PeerIdentity, tonic::Status> {
            let principal = req
                .metadata()
                .get("x-peer-principal")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| tonic::Status::unauthenticated("missing x-peer-principal"))?;
            if principal != self.expected_principal {
                return Err(tonic::Status::permission_denied("unexpected peer"));
            }
            Ok(PeerIdentity {
                principal: principal.to_string(),
                workspace_id: Some("local-workspace".to_string()),
                auth_scheme: "injected-peer".to_string(),
            })
        }
    }

    #[test]
    fn test_token_match() {
        let key = b"0123456789abcdef0123456789abcdef";
        let hash = hmac_token("my-secret", key);
        assert!(check_token("my-secret", &hash, key));
        assert!(!check_token("wrong-secret", &hash, key));
    }

    #[test]
    fn auth_mode_requires_complete_configuration_or_injected_verifier() {
        let _lock = ENV_LOCK.lock().unwrap();
        let variables = [
            "OMNISOLO_AGENT_TOKEN",
            "OMNISOLO_AGENT_AUTH_KEY",
            "OMNISOLO_AGENT_AUTH_DISABLED",
            "OMNISOLO_ENV",
        ];

        temp_env::with_vars(variables.map(|name| (name, None::<&str>)), || {
            assert!(auth_mode_from_env().is_err());

            let injected: Arc<dyn PeerIdentityVerifier> = Arc::new(HeaderPeerVerifier {
                expected_principal: "agent-1".to_string(),
            });
            assert!(matches!(
                auth_mode_from_env_with_verifier(Some(injected)).unwrap(),
                AuthMode::PeerVerifier(_)
            ));
        });

        temp_env::with_vars(
            [
                ("OMNISOLO_AGENT_TOKEN", Some("secret-token")),
                ("OMNISOLO_AGENT_AUTH_KEY", None),
                ("OMNISOLO_AGENT_AUTH_DISABLED", None),
                ("OMNISOLO_ENV", None),
            ],
            || assert!(auth_mode_from_env().is_err()),
        );

        temp_env::with_vars(
            [
                ("OMNISOLO_AGENT_TOKEN", Some("secret-token")),
                (
                    "OMNISOLO_AGENT_AUTH_KEY",
                    Some("0123456789abcdef0123456789abcdef"),
                ),
                ("OMNISOLO_AGENT_AUTH_DISABLED", None),
                ("OMNISOLO_ENV", None),
            ],
            || {
                assert!(matches!(
                    auth_mode_from_env().unwrap(),
                    AuthMode::Token { .. }
                ))
            },
        );

        for environment in ["development", "test"] {
            temp_env::with_vars(
                [
                    ("OMNISOLO_AGENT_TOKEN", None),
                    ("OMNISOLO_AGENT_AUTH_KEY", None),
                    ("OMNISOLO_AGENT_AUTH_DISABLED", Some("true")),
                    ("OMNISOLO_ENV", Some(environment)),
                ],
                || assert!(matches!(auth_mode_from_env().unwrap(), AuthMode::Disabled)),
            );
        }

        temp_env::with_vars(
            [
                ("OMNISOLO_AGENT_TOKEN", None),
                ("OMNISOLO_AGENT_AUTH_KEY", None),
                ("OMNISOLO_AGENT_AUTH_DISABLED", Some("true")),
                ("OMNISOLO_ENV", Some("production")),
            ],
            || assert!(auth_mode_from_env().is_err()),
        );
    }

    #[test]
    fn fail_closed_peer_mode_rejects_requests() {
        let mode = AuthMode::fail_closed_peer("mTLS verifier not installed");
        match mode {
            AuthMode::PeerVerifier(verifier) => {
                let err = verifier.verify_peer(&tonic::Request::new(())).unwrap_err();
                assert_eq!(err.code(), tonic::Code::Unauthenticated);
            }
            _ => panic!("expected PeerVerifier mode"),
        }
    }
}

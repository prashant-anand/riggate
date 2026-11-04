use crate::config::ClientConfig;
use anyhow::{bail, Context, Result};
use axum::{
    extract::State,
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::{collections::HashSet, net::SocketAddr, sync::Arc};
use subtle::ConstantTimeEq;

#[derive(Clone, Debug)]
pub struct ClientIdentity {
    pub id: String,
    pub name: String,
    pub allowed_agents: Option<Arc<HashSet<String>>>,
}

struct TokenEntry {
    token: String,
    identity: ClientIdentity,
}

#[derive(Clone, Default)]
pub struct ClientRegistry {
    entries: Arc<Vec<TokenEntry>>,
    authentication_enabled: bool,
}

impl ClientRegistry {
    #[cfg(test)]
    pub(crate) fn test_from_tokens(entries: Vec<(String, ClientIdentity)>) -> Self {
        Self {
            entries: Arc::new(
                entries
                    .into_iter()
                    .map(|(token, identity)| TokenEntry { token, identity })
                    .collect(),
            ),
            authentication_enabled: true,
        }
    }

    pub fn from_config(clients: &[ClientConfig], legacy_token: Option<String>) -> Result<Self> {
        let mut entries = Vec::new();
        for client in clients {
            let token = std::env::var(&client.token_env)
                .with_context(|| format!("reading client token from {}", client.token_env))?;
            if token.is_empty() {
                bail!(
                    "client token environment variable {} is empty",
                    client.token_env
                );
            }
            entries.push(TokenEntry {
                token,
                identity: ClientIdentity {
                    id: client.id.clone(),
                    name: client.name.clone().unwrap_or_else(|| client.id.clone()),
                    allowed_agents: client
                        .allowed_agents
                        .as_ref()
                        .map(|names| Arc::new(names.iter().cloned().collect())),
                },
            });
        }
        if let Some(token) = legacy_token.filter(|token| !token.is_empty()) {
            if entries.iter().any(|entry| entry.identity.id == "default") {
                bail!("client id 'default' conflicts with the implicit RIGGATE_API_KEY client");
            }
            entries.push(TokenEntry {
                token,
                identity: ClientIdentity {
                    id: "default".into(),
                    name: "default".into(),
                    allowed_agents: None,
                },
            });
        }
        Ok(Self {
            authentication_enabled: !entries.is_empty(),
            entries: Arc::new(entries),
        })
    }

    pub fn has_credentials(&self) -> bool {
        self.authentication_enabled
    }

    fn resolve(&self, authorization: Option<&str>) -> Option<ClientIdentity> {
        let provided = authorization
            .and_then(|header| header.strip_prefix("Bearer "))
            .unwrap_or("");
        let mut matched = None;
        for entry in self.entries.iter() {
            let equal = entry.token.len() == provided.len()
                && bool::from(entry.token.as_bytes().ct_eq(provided.as_bytes()));
            if equal {
                matched = Some(entry.identity.clone());
            }
        }
        matched
    }
}

pub async fn authorize(
    State(registry): State<ClientRegistry>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    if !registry.authentication_enabled {
        request.extensions_mut().insert(ClientIdentity {
            id: "anonymous".into(),
            name: "anonymous".into(),
            allowed_agents: None,
        });
        return next.run(request).await;
    }
    let identity = registry.resolve(
        request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    );
    if let Some(identity) = identity {
        request.extensions_mut().insert(identity);
        next.run(request).await
    } else {
        let client = request
            .extensions()
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|connect| connect.0.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        tracing::warn!(client_address = %client, "unauthorized API request");
        (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({"error": {"message": "missing or invalid bearer token", "type": "authentication_error"}})),
        )
            .into_response()
    }
}

pub fn bind_requires_auth(bind: &str) -> bool {
    bind.parse::<std::net::SocketAddr>()
        .map(|address| !address.ip().is_loopback())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_client_token_is_rejected_and_known_one_is_attributed() {
        let first = ClientIdentity {
            id: "a".into(),
            name: "A".into(),
            allowed_agents: None,
        };
        let second = ClientIdentity {
            id: "b".into(),
            name: "B".into(),
            allowed_agents: None,
        };
        let registry = ClientRegistry {
            entries: Arc::new(vec![
                TokenEntry {
                    token: "one".into(),
                    identity: first,
                },
                TokenEntry {
                    token: "two".into(),
                    identity: second,
                },
            ]),
            authentication_enabled: true,
        };
        assert_eq!(registry.resolve(Some("Bearer two")).unwrap().id, "b");
        assert!(registry.resolve(Some("Bearer invalid")).is_none());
    }

    #[test]
    fn requires_auth_when_binding_outside_loopback() {
        assert!(!bind_requires_auth("127.0.0.1:8080"));
        assert!(!bind_requires_auth("[::1]:8080"));
        assert!(bind_requires_auth("0.0.0.0:8080"));
        assert!(bind_requires_auth("[::]:8080"));
        assert!(bind_requires_auth("not a socket address"));
    }
}

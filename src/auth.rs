use axum::{
    extract::State,
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use subtle::ConstantTimeEq;

pub async fn authorize(
    State(api_key): State<Option<String>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let authorized = api_key.as_deref().is_none_or(|key| {
        request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|header| api_key_matches(Some(key), Some(header)))
    });

    if authorized {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({"error": {"message": "missing or invalid bearer token", "type": "authentication_error"}})),
        )
            .into_response()
    }
}

fn api_key_matches(expected: Option<&str>, authorization: Option<&str>) -> bool {
    let (Some(expected), Some(authorization)) = (expected, authorization) else {
        return false;
    };
    let Some(provided) = authorization.strip_prefix("Bearer ") else {
        return false;
    };
    expected.len() == provided.len() && bool::from(expected.as_bytes().ct_eq(provided.as_bytes()))
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
    fn accepts_only_the_configured_bearer_token() {
        assert!(api_key_matches(Some("secret"), Some("Bearer secret")));
        assert!(!api_key_matches(Some("secret"), Some("Bearer wrong")));
        assert!(!api_key_matches(Some("secret"), Some("secret")));
        assert!(!api_key_matches(Some("secret"), None));
        assert!(!api_key_matches(None, Some("Bearer secret")));
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

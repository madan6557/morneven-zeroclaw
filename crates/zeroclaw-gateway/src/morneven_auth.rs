use axum::{
    Json,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    env,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type HmacSha256 = Hmac<Sha256>;

const SESSION_PREFIX: &str = "mv1";
const DEFAULT_SESSION_TTL_SECONDS: i64 = 4 * 60 * 60;

#[derive(Debug, Deserialize)]
pub struct MornevenLoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MornevenWebUser {
    pub id: String,
    pub username: String,
    pub role: String,
    pub level: i64,
    pub track: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MornevenSessionClaims {
    sub: String,
    username: String,
    role: String,
    level: i64,
    track: String,
    iat: i64,
    exp: i64,
}

#[derive(Debug, Deserialize)]
struct BackendEnvelope<T> {
    success: bool,
    message: Option<String>,
    #[serde(rename = "errorCode")]
    error_code: Option<String>,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct BackendLoginData {
    token: String,
}

#[derive(Debug, Deserialize)]
struct BackendAccessData {
    #[serde(rename = "canAccessBotManager")]
    can_access_bot_manager: bool,
    user: MornevenWebUser,
}

fn response(status: StatusCode, payload: Value) -> Response {
    (status, Json(payload)).into_response()
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    response(
        status,
        json!({
            "ok": false,
            "error": message.into()
        }),
    )
}

fn auth_error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({
            "error": message.into()
        })),
    )
}

pub fn web_auth_enabled() -> bool {
    env::var("MORNEVEN_WEB_AUTH_ENABLED")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn backend_base_url() -> Result<String, String> {
    let raw = env::var("MORNEVEN_BACKEND_INTERNAL_URL")
        .map_err(|_| "MORNEVEN_BACKEND_INTERNAL_URL is not configured".to_string())?;
    let trimmed = raw.trim().trim_end_matches('/').to_string();
    if trimmed.is_empty() {
        return Err("MORNEVEN_BACKEND_INTERNAL_URL is empty".to_string());
    }
    Ok(trimmed)
}

fn backend_url(path: &str) -> Result<String, String> {
    let base = backend_base_url()?;
    let path = path.trim_start_matches('/');
    if base.ends_with("/api") && path.starts_with("api/") {
        Ok(format!("{}/{}", base, path.trim_start_matches("api/")))
    } else if base.ends_with("/v1") && path.starts_with("v1/") {
        Ok(format!("{}/{}", base, path.trim_start_matches("v1/")))
    } else {
        Ok(format!("{}/{}", base, path))
    }
}

fn session_secret() -> Result<String, String> {
    let secret = env::var("MORNEVEN_WEB_SESSION_SECRET")
        .map_err(|_| "MORNEVEN_WEB_SESSION_SECRET is not configured".to_string())?;
    if secret.trim().len() < 32 {
        return Err("MORNEVEN_WEB_SESSION_SECRET must be at least 32 characters".to_string());
    }
    Ok(secret)
}

fn session_ttl_seconds() -> i64 {
    env::var("MORNEVEN_WEB_SESSION_TTL_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_SESSION_TTL_SECONDS)
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs() as i64
}

fn sign_payload(payload: &str) -> Result<String, String> {
    let secret = session_secret()?;
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| "Invalid session secret".to_string())?;
    mac.update(payload.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn issue_session_token(user: &MornevenWebUser) -> Result<(String, String), String> {
    let issued_at = now_unix_seconds();
    let expires_at = issued_at + session_ttl_seconds();
    let claims = MornevenSessionClaims {
        sub: user.id.clone(),
        username: user.username.clone(),
        role: user.role.clone(),
        level: user.level,
        track: user.track.clone(),
        iat: issued_at,
        exp: expires_at,
    };
    let payload_json = serde_json::to_vec(&claims).map_err(|error| error.to_string())?;
    let payload = hex::encode(payload_json);
    let signature = sign_payload(&payload)?;
    let expires_at_iso = chrono::DateTime::<chrono::Utc>::from_timestamp(expires_at, 0)
        .map(|value| value.to_rfc3339())
        .unwrap_or_default();
    Ok((format!("{SESSION_PREFIX}.{payload}.{signature}"), expires_at_iso))
}

pub fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
        .filter(|token| !token.trim().is_empty())
}

pub fn validate_session_token(token: &str) -> Result<MornevenWebUser, String> {
    let mut parts = token.split('.');
    let prefix = parts.next().unwrap_or_default();
    let payload = parts.next().unwrap_or_default();
    let signature = parts.next().unwrap_or_default();
    if prefix != SESSION_PREFIX || payload.is_empty() || signature.is_empty() || parts.next().is_some() {
        return Err("Invalid Morneven session token".to_string());
    }

    let provided = hex::decode(signature).map_err(|_| "Invalid Morneven session signature".to_string())?;
    let mut mac = HmacSha256::new_from_slice(session_secret()?.as_bytes())
        .map_err(|_| "Invalid session secret".to_string())?;
    mac.update(payload.as_bytes());
    mac.verify_slice(&provided)
        .map_err(|_| "Invalid Morneven session signature".to_string())?;

    let payload_bytes = hex::decode(payload).map_err(|_| "Invalid Morneven session payload".to_string())?;
    let claims: MornevenSessionClaims =
        serde_json::from_slice(&payload_bytes).map_err(|_| "Invalid Morneven session payload".to_string())?;
    if claims.exp <= now_unix_seconds() {
        return Err("Morneven session expired".to_string());
    }
    Ok(MornevenWebUser {
        id: claims.sub,
        username: claims.username,
        role: claims.role,
        level: claims.level,
        track: claims.track,
    })
}

pub fn require_web_session(headers: &HeaderMap) -> Result<MornevenWebUser, (StatusCode, Json<Value>)> {
    let token = extract_bearer_token(headers)
        .ok_or_else(|| auth_error(StatusCode::UNAUTHORIZED, "Missing Morneven session"))?;
    validate_session_token(token).map_err(|error| auth_error(StatusCode::UNAUTHORIZED, error))
}

pub async fn handle_login(Json(body): Json<MornevenLoginRequest>) -> Response {
    if !web_auth_enabled() {
        return error_response(StatusCode::NOT_FOUND, "Morneven WebUI auth is not enabled");
    }

    let login_url = match backend_url("/api/auth/login") {
        Ok(value) => value,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    let access_url = match backend_url("/api/bot-manager/access") {
        Ok(value) => value,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
    {
        Ok(value) => value,
        Err(error) => return error_response(StatusCode::BAD_GATEWAY, error.to_string()),
    };

    let login_response = match client
        .post(login_url)
        .json(&json!({
            "email": body.email,
            "password": body.password
        }))
        .send()
        .await
    {
        Ok(value) => value,
        Err(error) => return error_response(StatusCode::BAD_GATEWAY, format!("Morneven login request failed: {error}")),
    };
    let login_status = login_response.status();
    let login_payload = match login_response.json::<BackendEnvelope<BackendLoginData>>().await {
        Ok(value) => value,
        Err(error) => return error_response(StatusCode::BAD_GATEWAY, format!("Invalid Morneven login response: {error}")),
    };
    if !login_status.is_success() || !login_payload.success {
        return error_response(
            StatusCode::from_u16(login_status.as_u16()).unwrap_or(StatusCode::UNAUTHORIZED),
            login_payload
                .message
                .or(login_payload.error_code)
                .unwrap_or_else(|| "Morneven login failed".to_string()),
        );
    }
    let Some(login_data) = login_payload.data else {
        return error_response(StatusCode::BAD_GATEWAY, "Morneven login response did not include a token");
    };

    let access_response = match client
        .get(access_url)
        .bearer_auth(&login_data.token)
        .send()
        .await
    {
        Ok(value) => value,
        Err(error) => return error_response(StatusCode::BAD_GATEWAY, format!("Morneven access check failed: {error}")),
    };
    let access_status = access_response.status();
    let access_payload = match access_response.json::<BackendEnvelope<BackendAccessData>>().await {
        Ok(value) => value,
        Err(error) => return error_response(StatusCode::BAD_GATEWAY, format!("Invalid Morneven access response: {error}")),
    };
    if !access_status.is_success() || !access_payload.success {
        return error_response(
            StatusCode::from_u16(access_status.as_u16()).unwrap_or(StatusCode::FORBIDDEN),
            access_payload
                .message
                .or(access_payload.error_code)
                .unwrap_or_else(|| "Bot Manager access denied".to_string()),
        );
    }
    let Some(access_data) = access_payload.data else {
        return error_response(StatusCode::BAD_GATEWAY, "Morneven access response was empty");
    };
    if !access_data.can_access_bot_manager {
        return error_response(StatusCode::FORBIDDEN, "Bot Manager access denied");
    }
    let (token, expires_at) = match issue_session_token(&access_data.user) {
        Ok(value) => value,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };

    response(
        StatusCode::OK,
        json!({
            "ok": true,
            "authMode": "morneven",
            "token": token,
            "expiresAt": expires_at,
            "user": access_data.user
        }),
    )
}

pub async fn handle_session(headers: HeaderMap) -> Response {
    if !web_auth_enabled() {
        return error_response(StatusCode::NOT_FOUND, "Morneven WebUI auth is not enabled");
    }
    match require_web_session(&headers) {
        Ok(user) => response(
            StatusCode::OK,
            json!({
                "ok": true,
                "authMode": "morneven",
                "user": user
            }),
        ),
        Err((status, payload)) => (status, payload).into_response(),
    }
}

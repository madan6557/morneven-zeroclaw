use axum::{
    Json,
    extract::{Path as AxumPath, Query},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    env, fs, io,
    path::{Component, Path, PathBuf},
    time::Duration,
};

const MORNEVEN_TOKEN_HEADER: &str = "x-morneven-reload-token";
const BOT_MANAGER_TOKEN_HEADER: &str = "x-bot-manager-sync-token";
const MAX_WORKSPACE_SYNC_BYTES: u64 = 500_000;
const USAGE_EVENT_LIMIT: usize = 5_000;
const DEFAULT_GATEWAY_BASE_PORT: u16 = 18_080;

#[derive(Debug, Deserialize)]
pub struct ReloadRequest {
    #[serde(rename = "restartGateway")]
    restart_gateway: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ProviderUsageQuery {
    from: Option<String>,
    to: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
struct DesiredRuntimeState {
    identity_id: String,
    state: String,
    started_at: Option<String>,
    stopped_at: Option<String>,
    restart_count: u64,
    last_action_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
struct DesiredGatewayState {
    global: String,
    updated_at: Option<String>,
    runtimes: BTreeMap<String, DesiredRuntimeState>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestFile {
    #[serde(rename = "contentHash")]
    content_hash: String,
    size: u64,
    #[serde(rename = "syncedAt")]
    synced_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeManifest {
    version: u8,
    #[serde(rename = "syncedAt")]
    synced_at: Option<String>,
    identity: Value,
    files: BTreeMap<String, ManifestFile>,
}

fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

fn runtime_root() -> PathBuf {
    if let Ok(path) = env::var("MORNEVEN_ZEROCLAW_ROOT") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    if let Ok(path) = env::var("ZEROCLAW_MORNEVEN_ROOT") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".zeroclaw").join("morneven"))
        .unwrap_or_else(|| PathBuf::from(".zeroclaw").join("morneven"))
}

fn runtimes_root() -> PathBuf {
    runtime_root().join("runtimes")
}

fn runtime_state_path() -> PathBuf {
    runtime_root().join("runtime-state.json")
}

fn desired_state_path() -> PathBuf {
    runtime_root().join("gateway-desired-state.json")
}

fn log_path() -> PathBuf {
    runtime_root().join("morneven.log")
}

fn root_usage_path() -> PathBuf {
    runtime_root().join("provider-usage.jsonl")
}

fn response(status: StatusCode, payload: Value) -> Response {
    (status, Json(payload)).into_response()
}

fn require_morneven_token(headers: &HeaderMap) -> Result<(), Response> {
    let expected = env::var("MORNEVEN_RELOAD_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            env::var("NANOBOT_MORNEVEN_RELOAD_TOKEN")
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
    let Some(expected) = expected else {
        return Err(response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"ok": false, "error": "Morneven reload token is not configured"}),
        ));
    };
    let provided = headers
        .get(MORNEVEN_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if provided != expected {
        return Err(response(
            StatusCode::FORBIDDEN,
            json!({"ok": false, "error": "Invalid Morneven reload token"}),
        ));
    }
    Ok(())
}

fn append_log(message: impl AsRef<str>) {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let line = format!("[Morneven] {} {}\n", now_iso(), message.as_ref());
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
}

fn read_recent_logs(limit: usize) -> Vec<String> {
    let Ok(content) = fs::read_to_string(log_path()) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = content.lines().map(ToOwned::to_owned).collect();
    if lines.len() > limit {
        lines.drain(0..lines.len() - limit);
    }
    lines
}

fn json_read(path: &Path) -> Option<Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<Value>(&content).ok())
}

fn json_bytes<T: Serialize + ?Sized>(value: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(value).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn json_write(path: &Path, value: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json_bytes(value)?)
}

fn load_runtime_state() -> Value {
    json_read(&runtime_state_path()).unwrap_or_else(|| {
        json!({
            "syncedAt": null,
            "mode": "single-active-personality",
            "identity": null,
            "mainIdentity": null,
            "runtimeCount": 0,
            "runtimes": [],
            "fileCount": 0,
            "files": []
        })
    })
}

fn load_desired_state() -> DesiredGatewayState {
    fs::read_to_string(desired_state_path())
        .ok()
        .and_then(|content| serde_json::from_str::<DesiredGatewayState>(&content).ok())
        .unwrap_or_else(|| DesiredGatewayState {
            global: "stopped".to_string(),
            updated_at: None,
            runtimes: BTreeMap::new(),
        })
}

fn save_desired_state(state: &DesiredGatewayState) -> io::Result<()> {
    if let Some(parent) = desired_state_path().parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(desired_state_path(), json_bytes(state)?)
}

fn value_object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn normalize_slug(raw: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "identity".to_string()
    } else {
        trimmed
    }
}

fn runtime_slug(identity: &Value) -> String {
    let raw = string_field(identity, "slug")
        .or_else(|| string_field(identity, "name"))
        .or_else(|| string_field(identity, "id"))
        .unwrap_or("identity");
    normalize_slug(raw)
}

fn runtime_dir_for_identity(identity: &Value) -> PathBuf {
    let slug = runtime_slug(identity);
    let raw_id = string_field(identity, "id").unwrap_or(&slug);
    let safe_id = normalize_slug(raw_id);
    let suffix: String = safe_id.chars().take(8).collect();
    runtimes_root().join(format!("{slug}-{suffix}"))
}

fn normalize_runtime_path(raw: &str) -> io::Result<String> {
    let normalized = raw.trim().replace('\\', "/").trim_start_matches('/').to_string();
    if normalized.is_empty() || normalized.len() > 240 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid runtime file path"));
    }
    if normalized.chars().any(|ch| {
        !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/'))
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Runtime file path contains unsupported characters",
        ));
    }
    let path = Path::new(&normalized);
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) | Component::CurDir
        )
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Runtime file path cannot traverse directories",
        ));
    }
    Ok(normalized)
}

fn resolve_workspace_file(workspace: &Path, relative_path: &str) -> io::Result<PathBuf> {
    let normalized = normalize_runtime_path(relative_path)?;
    let mut target = PathBuf::from(workspace);
    for segment in normalized.split('/') {
        target.push(segment);
    }
    Ok(target)
}

fn content_hash(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    hex::encode(hasher.finalize())
}

fn infer_workspace_kind(relative_path: &str) -> &'static str {
    let normalized = relative_path.to_ascii_lowercase();
    let filename = normalized.rsplit('/').next().unwrap_or(&normalized);
    if matches!(filename, "agents.md" | "soul.md" | "lore.md") {
        "identity"
    } else if filename == "memory.md" || normalized.starts_with("memory/") {
        "memory"
    } else if normalized.starts_with("cron/") {
        "cron"
    } else if normalized.starts_with("sessions/") {
        "session"
    } else if normalized.starts_with("skills/") {
        "skill"
    } else if filename == "tools.md" || normalized.starts_with("tools/") {
        "tool"
    } else if filename == "user.md" {
        "user"
    } else if filename == "heartbeat.md" {
        "system"
    } else {
        "other"
    }
}

fn load_manifest(path: &Path) -> RuntimeManifest {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<RuntimeManifest>(&content).ok())
        .unwrap_or(RuntimeManifest {
            version: 1,
            synced_at: None,
            identity: json!({}),
            files: BTreeMap::new(),
        })
}

fn write_manifest(path: &Path, files: BTreeMap<String, ManifestFile>, identity: Value) -> io::Result<()> {
    let manifest = RuntimeManifest {
        version: 1,
        synced_at: Some(now_iso()),
        identity,
        files,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json_bytes(&manifest)?)
}

fn credential_provider(entry: &Value) -> Option<String> {
    let credentials = entry.get("credentials")?.as_object()?;
    credentials
        .iter()
        .find(|(_, credential)| credential.as_object().is_some())
        .map(|(provider, _)| provider.to_string())
}

fn credential_model(entry: &Value, provider: &str) -> Option<String> {
    entry.get("credentials")
        .and_then(|credentials| credentials.get(provider))
        .and_then(|credential| {
            string_field(credential, "modelId")
                .or_else(|| string_field(credential, "model_id"))
                .map(ToOwned::to_owned)
        })
}

fn telegram_config(entry: &Value) -> Option<&Value> {
    entry.get("channels")
        .and_then(|channels| channels.get("telegram"))
        .filter(|telegram| bool_field(telegram, "enabled"))
}

fn telegram_token_fingerprint(entry: &Value) -> Option<String> {
    let token = telegram_config(entry)
        .and_then(|telegram| {
            string_field(telegram, "token")
                .or_else(|| string_field(telegram, "botToken"))
                .or_else(|| string_field(telegram, "bot_token"))
        })?;
    Some(content_hash(token.as_bytes()).chars().take(12).collect())
}

fn auto_dream_enabled(entry: &Value) -> Option<bool> {
    let value = entry
        .get("settings")
        .and_then(|settings| settings.get("autoDream"))
        .and_then(|auto_dream| auto_dream.get("enabled"))?;
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) => Some(!matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )),
        _ => None,
    }
}

fn topic_registry_from_entry(entry: &Value) -> Value {
    entry.get("channels")
        .and_then(|channels| channels.get("telegram"))
        .and_then(|telegram| telegram.get("topicRegistry"))
        .filter(|registry| registry.get("groups").and_then(Value::as_array).is_some())
        .cloned()
        .unwrap_or_else(|| json!({"groups": []}))
}

fn runtime_entries_from_bundle(bundle: &Value) -> io::Result<Vec<Value>> {
    if let Some(entries) = bundle.get("identities").and_then(Value::as_array) {
        let filtered: Vec<Value> = entries
            .iter()
            .filter(|entry| entry.get("identity").and_then(Value::as_object).is_some())
            .cloned()
            .collect();
        if !filtered.is_empty() {
            return Ok(filtered);
        }
    }
    let identity = bundle
        .get("activeIdentity")
        .or_else(|| bundle.get("mainIdentity"))
        .filter(|identity| identity.as_object().is_some())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Runtime bundle does not contain an active identity"))?;
    Ok(vec![json!({
        "identity": identity,
        "credentials": bundle.get("credentials").cloned().unwrap_or_else(|| json!({})),
        "channels": bundle.get("channels").cloned().unwrap_or_else(|| json!({})),
        "settings": bundle.get("settings").cloned().unwrap_or_else(|| json!({})),
        "files": bundle.get("files").cloned().unwrap_or_else(|| json!([]))
    })])
}

fn write_runtime_config(
    config_path: &Path,
    entry: &Value,
    general_config: &Value,
    workspace_path: &Path,
    gateway_port: u16,
) -> io::Result<()> {
    let provider = credential_provider(entry);
    let model = provider
        .as_deref()
        .and_then(|provider| credential_model(entry, provider));
    let config = json!({
        "providers": entry.get("credentials").cloned().unwrap_or_else(|| json!({})),
        "channels": entry.get("channels").cloned().unwrap_or_else(|| json!({})),
        "settings": entry.get("settings").cloned().unwrap_or_else(|| json!({})),
        "generalConfig": general_config,
        "agents": {
            "defaults": {
                "workspace": workspace_path.to_string_lossy(),
                "provider": provider.unwrap_or_else(|| "auto".to_string()),
                "model": model.unwrap_or_default()
            }
        },
        "gateway": {
            "port": gateway_port
        }
    });
    json_write(config_path, &config)
}

fn materialize_runtime_entry(
    entry: &Value,
    general_config: &Value,
    main_identity_id: &str,
    gateway_port: u16,
) -> io::Result<Value> {
    let identity = entry.get("identity").cloned().unwrap_or_else(|| json!({}));
    let identity_id = string_field(&identity, "id").unwrap_or_default().to_string();
    let runtime_dir = runtime_dir_for_identity(&identity);
    let workspace_path = runtime_dir.join("workspace");
    let manifest_path = runtime_dir.join(".morneven-runtime-manifest.json");
    let config_path = runtime_dir.join("config.json");
    fs::create_dir_all(&workspace_path)?;
    let previous_manifest = load_manifest(&manifest_path);
    let mut written = BTreeMap::new();
    let mut written_paths = HashSet::new();

    let files = entry.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
    for file in files {
        let path = string_field(&file, "path").unwrap_or_default();
        if path.is_empty() {
            continue;
        }
        let relative_path = normalize_runtime_path(path)?;
        let content = file
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let target = resolve_workspace_file(&workspace_path, &relative_path)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, content.as_bytes())?;
        written_paths.insert(relative_path.clone());
        written.insert(
            relative_path,
            ManifestFile {
                content_hash: content_hash(content.as_bytes()),
                size: content.len() as u64,
                synced_at: now_iso(),
            },
        );
    }

    for previous_path in previous_manifest.files.keys() {
        if written_paths.contains(previous_path) {
            continue;
        }
        if let Ok(target) = resolve_workspace_file(&workspace_path, previous_path) {
            if target.is_file() {
                let _ = fs::remove_file(target);
            }
        }
    }

    write_runtime_config(&config_path, entry, general_config, &workspace_path, gateway_port)?;
    json_write(&runtime_dir.join("telegram-topics.json"), &topic_registry_from_entry(entry))?;
    write_manifest(
        &manifest_path,
        written,
        json!({
            "id": string_field(&identity, "id").unwrap_or_default(),
            "slug": string_field(&identity, "slug").unwrap_or_default(),
            "name": string_field(&identity, "name").unwrap_or_default(),
            "roleTitle": string_field(&identity, "roleTitle").unwrap_or_default()
        }),
    )?;

    let files: Vec<String> = written_paths.into_iter().collect();
    Ok(json!({
        "identityId": identity_id,
        "slug": string_field(&identity, "slug").unwrap_or_default(),
        "name": string_field(&identity, "name").unwrap_or_default(),
        "roleTitle": string_field(&identity, "roleTitle").unwrap_or_default(),
        "isMain": identity_id == main_identity_id || bool_field(&identity, "isMain"),
        "runtimePath": runtime_dir.to_string_lossy(),
        "workspacePath": workspace_path.to_string_lossy(),
        "configPath": config_path.to_string_lossy(),
        "telegramTopicsPath": runtime_dir.join("telegram-topics.json").to_string_lossy(),
        "usageEventsPath": runtime_dir.join("provider-usage.jsonl").to_string_lossy(),
        "gatewayPort": gateway_port,
        "telegramBotUsername": null,
        "telegramTokenFingerprint": telegram_token_fingerprint(entry),
        "telegramActiveBotUsernames": [],
        "autoDreamEnabled": auto_dream_enabled(entry),
        "fileCount": files.len(),
        "files": files,
        "provider": credential_provider(entry),
        "enabledChannels": enabled_channels(entry),
        "syncedAt": now_iso()
    }))
}

fn enabled_channels(entry: &Value) -> Vec<String> {
    entry
        .get("channels")
        .and_then(Value::as_object)
        .map(|channels| {
            channels
                .iter()
                .filter(|(_, config)| bool_field(config, "enabled"))
                .map(|(name, _)| name.to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn materialize_morneven_runtime(bundle: &Value) -> io::Result<Value> {
    fs::create_dir_all(runtime_root())?;
    fs::create_dir_all(runtimes_root())?;
    let entries = runtime_entries_from_bundle(bundle)?;
    let main_identity = bundle
        .get("mainIdentity")
        .or_else(|| bundle.get("activeIdentity"))
        .cloned()
        .unwrap_or_else(|| entries[0].get("identity").cloned().unwrap_or_else(|| json!({})));
    let main_identity_id = string_field(&main_identity, "id")
        .or_else(|| entries[0].get("identity").and_then(|identity| string_field(identity, "id")))
        .unwrap_or_default()
        .to_string();
    let general_config = bundle.get("generalConfig").cloned().unwrap_or_else(|| json!({}));
    let mode = string_field(bundle, "mode")
        .unwrap_or("single-active-personality")
        .to_string();
    let mut runtimes = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let gateway_port = DEFAULT_GATEWAY_BASE_PORT.saturating_add(index as u16);
        runtimes.push(materialize_runtime_entry(
            entry,
            &general_config,
            &main_identity_id,
            gateway_port,
        )?);
    }
    let main_runtime = runtimes
        .iter()
        .find(|runtime| bool_field(runtime, "isMain"))
        .cloned()
        .unwrap_or_else(|| runtimes[0].clone());
    let file_count: usize = runtimes
        .iter()
        .map(|runtime| runtime.get("fileCount").and_then(Value::as_u64).unwrap_or(0) as usize)
        .sum();
    let state = json!({
        "syncedAt": now_iso(),
        "mode": mode,
        "mainIdentity": {
            "id": string_field(&main_runtime, "identityId").unwrap_or_default(),
            "slug": string_field(&main_runtime, "slug").unwrap_or_default(),
            "name": string_field(&main_runtime, "name").unwrap_or_default(),
            "roleTitle": string_field(&main_runtime, "roleTitle").unwrap_or_default()
        },
        "identity": {
            "id": string_field(&main_runtime, "identityId").unwrap_or_default(),
            "slug": string_field(&main_runtime, "slug").unwrap_or_default(),
            "name": string_field(&main_runtime, "name").unwrap_or_default(),
            "roleTitle": string_field(&main_runtime, "roleTitle").unwrap_or_default()
        },
        "runtimeCount": runtimes.len(),
        "runtimes": runtimes,
        "fileCount": file_count,
        "files": main_runtime.get("files").cloned().unwrap_or_else(|| json!([]))
    });
    json_write(&runtime_state_path(), &state)?;
    ensure_desired_runtimes(&state)?;
    append_log(format!("runtime synced: {} runtime(s)", state["runtimeCount"]));
    Ok(state)
}

fn ensure_desired_runtimes(state: &Value) -> io::Result<()> {
    let mut desired = load_desired_state();
    if desired.global.is_empty() {
        desired.global = "stopped".to_string();
    }
    for runtime in state
        .get("runtimes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(identity_id) = string_field(runtime, "identityId") else {
            continue;
        };
        desired
            .runtimes
            .entry(identity_id.to_string())
            .or_insert_with(|| DesiredRuntimeState {
                identity_id: identity_id.to_string(),
                state: desired.global.clone(),
                started_at: None,
                stopped_at: None,
                restart_count: 0,
                last_action_at: None,
            });
    }
    desired.updated_at = Some(now_iso());
    save_desired_state(&desired)
}

fn normalize_service_url(raw: &str, default_port: Option<&str>) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let mut url = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    if let Some(port) = default_port {
        let without_scheme = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))
            .unwrap_or(&url);
        let host = without_scheme.split('/').next().unwrap_or_default();
        if host.ends_with(".railway.internal") && !host.contains(':') {
            let prefix_len = url.len() - without_scheme.len();
            let rest_start = prefix_len + host.len();
            let rest = url[rest_start..].to_string();
            url.truncate(rest_start);
            url.push(':');
            url.push_str(port);
            url.push_str(&rest);
        }
    }
    Some(url)
}

fn backend_base_urls() -> Vec<String> {
    let mut urls = Vec::new();
    for (key, default_port) in [
        ("MORNEVEN_BACKEND_INTERNAL_URL", Some("8080")),
        ("MORNEVEN_BACKEND_PUBLIC_URL", None),
    ] {
        if let Ok(value) = env::var(key) {
            if let Some(url) = normalize_service_url(&value, default_port) {
                if !urls.contains(&url) {
                    urls.push(url);
                }
            }
        }
    }
    urls
}

fn bot_manager_sync_token() -> Option<String> {
    env::var("MORNEVEN_BOT_MANAGER_SYNC_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            env::var("BOT_MANAGER_SYNC_TOKEN")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
}

fn bot_manager_bundle_url(base: &str) -> String {
    if base.ends_with("/api") {
        format!("{base}/bot-manager/runtime/bundle")
    } else if base.ends_with("/bot-manager") {
        format!("{base}/runtime/bundle")
    } else {
        format!("{base}/api/bot-manager/runtime/bundle")
    }
}

async fn fetch_morneven_runtime_bundle() -> Result<Value, String> {
    let token = bot_manager_sync_token()
        .ok_or_else(|| "MORNEVEN_BOT_MANAGER_SYNC_TOKEN is not configured".to_string())?;
    let bases = backend_base_urls();
    if bases.is_empty() {
        return Err("Morneven backend URL is not configured".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| error.to_string())?;
    let mut last_error = String::new();
    for base in bases {
        let endpoint = bot_manager_bundle_url(&base);
        let result = client
            .get(&endpoint)
            .header("accept", "application/json")
            .header(BOT_MANAGER_TOKEN_HEADER, &token)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {
                let payload = response
                    .json::<Value>()
                    .await
                    .map_err(|error| error.to_string())?;
                if payload.get("success").and_then(Value::as_bool) == Some(true) {
                    return Ok(payload.get("data").cloned().unwrap_or(Value::Null));
                }
                return Ok(payload);
            }
            Ok(response) => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                last_error = format!("{endpoint} responded with {status}: {text}");
            }
            Err(error) => {
                last_error = format!("{endpoint} failed: {error}");
            }
        }
    }
    Err(last_error)
}

fn set_runtime_action(identity_id: Option<&str>, action: &str) -> io::Result<DesiredGatewayState> {
    let mut desired = load_desired_state();
    let state = load_runtime_state();
    let now = now_iso();
    let target_state = match action {
        "start" => "running",
        "stop" => "stopped",
        "restart" => "running",
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid runtime action")),
    };
    if identity_id.is_none() {
        desired.global = target_state.to_string();
    }
    for runtime in state
        .get("runtimes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(runtime_identity_id) = string_field(runtime, "identityId") else {
            continue;
        };
        if identity_id.is_some_and(|id| id != runtime_identity_id) {
            continue;
        }
        let entry = desired
            .runtimes
            .entry(runtime_identity_id.to_string())
            .or_insert_with(|| DesiredRuntimeState {
                identity_id: runtime_identity_id.to_string(),
                state: "stopped".to_string(),
                started_at: None,
                stopped_at: None,
                restart_count: 0,
                last_action_at: None,
            });
        if action == "restart" {
            entry.restart_count = entry.restart_count.saturating_add(1);
        }
        entry.state = target_state.to_string();
        entry.last_action_at = Some(now.clone());
        if target_state == "running" {
            entry.started_at = Some(now.clone());
            entry.stopped_at = None;
        } else {
            entry.stopped_at = Some(now.clone());
        }
    }
    desired.updated_at = Some(now);
    save_desired_state(&desired)?;
    append_log(format!(
        "gateway action {action}{}",
        identity_id.map(|id| format!(" for {id}")).unwrap_or_default()
    ));
    Ok(desired)
}

fn runtime_status(runtime: &Value, desired: &DesiredGatewayState) -> Value {
    let identity_id = string_field(runtime, "identityId").unwrap_or_default();
    let desired_runtime = desired.runtimes.get(identity_id);
    let desired_state = desired_runtime
        .map(|entry| entry.state.as_str())
        .unwrap_or("stopped");
    json!({
        "state": desired_state,
        "identityId": identity_id,
        "name": string_field(runtime, "name").unwrap_or_default(),
        "pid": null,
        "uptime": null,
        "startedAt": desired_runtime.and_then(|entry| entry.started_at.clone()),
        "restart_count": desired_runtime.map(|entry| entry.restart_count).unwrap_or(0),
        "gatewayPort": runtime.get("gatewayPort").cloned().unwrap_or(Value::Null),
        "telegramBotUsername": runtime.get("telegramBotUsername").cloned().unwrap_or(Value::Null),
        "telegramTokenFingerprint": runtime.get("telegramTokenFingerprint").cloned().unwrap_or(Value::Null),
        "lastError": null,
        "lastExitCode": null,
        "desiredState": desired_state,
        "autoRestartEnabled": true,
        "lastUnplannedExitAt": null,
        "lastRestartAt": desired_runtime.and_then(|entry| entry.last_action_at.clone()),
        "lastLogLine": read_recent_logs(1).first().cloned(),
        "slug": string_field(runtime, "slug").unwrap_or_default(),
        "isMain": bool_field(runtime, "isMain"),
        "workspacePath": string_field(runtime, "workspacePath").unwrap_or_default(),
        "provider": runtime.get("provider").cloned().unwrap_or(Value::Null),
        "enabledChannels": runtime.get("enabledChannels").cloned().unwrap_or_else(|| json!([]))
    })
}

fn gateway_status() -> Value {
    let state = load_runtime_state();
    let desired = load_desired_state();
    let runtimes: Vec<Value> = state
        .get("runtimes")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(|runtime| runtime_status(runtime, &desired)).collect())
        .unwrap_or_default();
    let main = runtimes
        .iter()
        .find(|runtime| bool_field(runtime, "isMain"))
        .cloned()
        .or_else(|| runtimes.first().cloned());
    json!({
        "state": desired.global,
        "running": runtimes.iter().filter(|runtime| string_field(runtime, "state") == Some("running")).count(),
        "stopped": runtimes.iter().filter(|runtime| string_field(runtime, "state") != Some("running")).count(),
        "runtimeCount": runtimes.len(),
        "identityId": main.as_ref().and_then(|runtime| string_field(runtime, "identityId")).unwrap_or_default(),
        "name": main.as_ref().and_then(|runtime| string_field(runtime, "name")).unwrap_or_default(),
        "pid": main.as_ref().and_then(|runtime| runtime.get("pid")).cloned().unwrap_or(Value::Null),
        "uptime": main.as_ref().and_then(|runtime| runtime.get("uptime")).cloned().unwrap_or(Value::Null),
        "startedAt": main.as_ref().and_then(|runtime| runtime.get("startedAt")).cloned().unwrap_or(Value::Null),
        "runtimes": runtimes,
        "desiredState": desired.global,
        "autoRestartEnabled": true,
        "logs": read_recent_logs(50)
    })
}

fn read_workspace_text(path: &Path) -> io::Result<(String, fs::Metadata)> {
    let stat = fs::metadata(path)?;
    if stat.len() > MAX_WORKSPACE_SYNC_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "File exceeds workspace sync size limit",
        ));
    }
    let raw = fs::read(path)?;
    if raw.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Binary files are not supported",
        ));
    }
    let content = String::from_utf8(raw).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "File is not valid UTF-8")
    })?;
    Ok((content, stat))
}

fn workspace_changes_at(workspace_root: &Path, manifest_path: &Path) -> Value {
    let manifest = load_manifest(manifest_path);
    let mut changes = Vec::new();
    let mut skipped = Vec::new();
    if !workspace_root.exists() {
        return json!({
            "syncedAt": manifest.synced_at,
            "changedCount": 0,
            "changes": [],
            "skipped": []
        });
    }
    let mut stack = vec![workspace_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let relative_path = path
                .strip_prefix(workspace_root)
                .ok()
                .and_then(|path| path.to_str())
                .map(|path| path.replace('\\', "/"))
                .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string());
            match read_workspace_text(&path) {
                Ok((content, stat)) => {
                    let hash = content_hash(content.as_bytes());
                    let base_hash = manifest.files.get(&relative_path).map(|file| file.content_hash.clone());
                    if base_hash.as_deref() == Some(hash.as_str()) {
                        continue;
                    }
                    let updated_at = stat
                        .modified()
                        .ok()
                        .map(DateTime::<Utc>::from)
                        .map(|time| time.to_rfc3339());
                    changes.push(json!({
                        "path": relative_path,
                        "kind": infer_workspace_kind(&relative_path),
                        "content": content,
                        "contentHash": hash,
                        "baseHash": base_hash,
                        "size": stat.len(),
                        "updatedAt": updated_at
                    }));
                }
                Err(error) => {
                    skipped.push(json!({"path": relative_path, "reason": error.to_string()}));
                }
            }
        }
    }
    changes.sort_by(|a, b| string_field(a, "path").cmp(&string_field(b, "path")));
    skipped.sort_by(|a, b| string_field(a, "path").cmp(&string_field(b, "path")));
    json!({
        "syncedAt": manifest.synced_at,
        "changedCount": changes.len(),
        "changes": changes,
        "skipped": skipped
    })
}

fn config_secret_payload_for_runtime(runtime: &Value) -> Value {
    let config_path = string_field(runtime, "configPath").unwrap_or_default();
    let data = if config_path.is_empty() {
        json!({})
    } else {
        json_read(Path::new(config_path)).unwrap_or_else(|| json!({}))
    };
    json!({
        "identityId": string_field(runtime, "identityId").unwrap_or_default(),
        "identity": {
            "id": string_field(runtime, "identityId").unwrap_or_default(),
            "slug": string_field(runtime, "slug").unwrap_or_default(),
            "name": string_field(runtime, "name").unwrap_or_default()
        },
        "providers": data.get("providers").cloned().unwrap_or_else(|| json!({})),
        "channels": data.get("channels").cloned().unwrap_or_else(|| json!({})),
        "tools": data.get("tools").cloned().unwrap_or_else(|| json!({})),
        "agents": data.get("agents").cloned().unwrap_or_else(|| json!({}))
    })
}

fn telegram_topics_for_runtime(runtime: &Value) -> Value {
    let path = string_field(runtime, "telegramTopicsPath").unwrap_or_default();
    let topics = if path.is_empty() {
        json!({"groups": []})
    } else {
        json_read(Path::new(path)).unwrap_or_else(|| json!({"groups": []}))
    };
    json!({
        "identityId": string_field(runtime, "identityId").unwrap_or_default(),
        "identity": {
            "id": string_field(runtime, "identityId").unwrap_or_default(),
            "slug": string_field(runtime, "slug").unwrap_or_default(),
            "name": string_field(runtime, "name").unwrap_or_default()
        },
        "groups": topics.get("groups").cloned().unwrap_or_else(|| json!([]))
    })
}

fn parse_iso_datetime(value: Option<&str>) -> Option<DateTime<Utc>> {
    value.and_then(|value| {
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|time| time.with_timezone(&Utc))
    })
}

fn usage_event_in_range(
    event: &Value,
    start: Option<&DateTime<Utc>>,
    end: Option<&DateTime<Utc>>,
) -> bool {
    if let Some(recorded) = parse_iso_datetime(string_field(event, "recordedAt")) {
        if let Some(start) = start {
            if recorded < start.clone() {
                return false;
            }
        }
        if let Some(end) = end {
            if recorded >= end.clone() {
                return false;
            }
        }
    }
    true
}

fn load_usage_events(
    path: &Path,
    start: Option<&DateTime<Utc>>,
    end: Option<&DateTime<Utc>>,
) -> Vec<Value> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .rev()
        .take(USAGE_EVENT_LIMIT)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| usage_event_in_range(event, start, end))
        .collect()
}

pub async fn handle_status(headers: HeaderMap) -> Response {
    if let Err(error) = require_morneven_token(&headers) {
        return error;
    }
    response(
        StatusCode::OK,
        json!({
            "ok": true,
            "gateway": gateway_status(),
            "morneven": load_runtime_state(),
            "logs": read_recent_logs(50)
        }),
    )
}

pub async fn handle_reload(headers: HeaderMap, Json(body): Json<ReloadRequest>) -> Response {
    if let Err(error) = require_morneven_token(&headers) {
        return error;
    }
    match fetch_morneven_runtime_bundle().await {
        Ok(bundle) => match materialize_morneven_runtime(&bundle) {
            Ok(state) => {
                if body.restart_gateway.unwrap_or(false) {
                    let _ = set_runtime_action(None, "restart");
                }
                response(
                    StatusCode::OK,
                    json!({
                        "ok": true,
                        "result": {"synced": true, "state": state},
                        "gateway": gateway_status(),
                        "restarted": body.restart_gateway.unwrap_or(false)
                    }),
                )
            }
            Err(error) => response(
                StatusCode::BAD_GATEWAY,
                json!({"ok": false, "error": error.to_string()}),
            ),
        },
        Err(error) => response(StatusCode::BAD_GATEWAY, json!({"ok": false, "error": error})),
    }
}

pub async fn handle_config_secrets(headers: HeaderMap) -> Response {
    if let Err(error) = require_morneven_token(&headers) {
        return error;
    }
    let state = load_runtime_state();
    let runtimes: Vec<Value> = state
        .get("runtimes")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(config_secret_payload_for_runtime).collect())
        .unwrap_or_default();
    response(StatusCode::OK, json!({"ok": true, "runtimes": runtimes}))
}

pub async fn handle_workspace_changes(headers: HeaderMap) -> Response {
    if let Err(error) = require_morneven_token(&headers) {
        return error;
    }
    let state = load_runtime_state();
    let runtimes: Vec<Value> = state
        .get("runtimes")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|runtime| {
                    let workspace = PathBuf::from(string_field(runtime, "workspacePath").unwrap_or_default());
                    let manifest = PathBuf::from(string_field(runtime, "runtimePath").unwrap_or_default())
                        .join(".morneven-runtime-manifest.json");
                    let changes = workspace_changes_at(&workspace, &manifest);
                    let mut payload = value_object(&changes).cloned().unwrap_or_default();
                    payload.insert("identityId".to_string(), json!(string_field(runtime, "identityId").unwrap_or_default()));
                    payload.insert(
                        "identity".to_string(),
                        json!({
                            "id": string_field(runtime, "identityId").unwrap_or_default(),
                            "slug": string_field(runtime, "slug").unwrap_or_default(),
                            "name": string_field(runtime, "name").unwrap_or_default()
                        }),
                    );
                    Value::Object(payload)
                })
                .collect()
        })
        .unwrap_or_default();
    response(StatusCode::OK, json!({"ok": true, "runtimes": runtimes}))
}

pub async fn handle_telegram_topics(headers: HeaderMap) -> Response {
    if let Err(error) = require_morneven_token(&headers) {
        return error;
    }
    let state = load_runtime_state();
    let runtimes: Vec<Value> = state
        .get("runtimes")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(telegram_topics_for_runtime).collect())
        .unwrap_or_default();
    response(StatusCode::OK, json!({"ok": true, "runtimes": runtimes}))
}

pub async fn handle_provider_usage(
    headers: HeaderMap,
    Query(query): Query<ProviderUsageQuery>,
) -> Response {
    if let Err(error) = require_morneven_token(&headers) {
        return error;
    }
    let start = parse_iso_datetime(query.from.as_deref());
    let end = parse_iso_datetime(query.to.as_deref());
    let state = load_runtime_state();
    let mut events = Vec::new();
    for runtime in state
        .get("runtimes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(path) = string_field(runtime, "usageEventsPath") {
            events.extend(load_usage_events(Path::new(path), start.as_ref(), end.as_ref()));
        }
    }
    if events.is_empty() {
        events.extend(load_usage_events(&root_usage_path(), start.as_ref(), end.as_ref()));
    }
    if events.len() > USAGE_EVENT_LIMIT {
        events.drain(0..events.len() - USAGE_EVENT_LIMIT);
    }
    let count = events.len();
    response(
        StatusCode::OK,
        json!({
            "ok": true,
            "events": events,
            "count": count
        }),
    )
}

pub async fn handle_gateway_action(headers: HeaderMap, AxumPath(action): AxumPath<String>) -> Response {
    if let Err(error) = require_morneven_token(&headers) {
        return error;
    }
    if !matches!(action.as_str(), "start" | "stop" | "restart") {
        return response(
            StatusCode::NOT_FOUND,
            json!({"ok": false, "error": "Invalid runtime action"}),
        );
    }
    if matches!(action.as_str(), "start" | "restart") {
        if let Ok(bundle) = fetch_morneven_runtime_bundle().await {
            let _ = materialize_morneven_runtime(&bundle);
        }
    }
    match set_runtime_action(None, &action) {
        Ok(_) => response(
            StatusCode::OK,
            json!({
                "ok": true,
                "action": action,
                "gateway": gateway_status(),
                "morneven": load_runtime_state()
            }),
        ),
        Err(error) => response(
            StatusCode::BAD_GATEWAY,
            json!({"ok": false, "error": error.to_string()}),
        ),
    }
}

pub async fn handle_runtime_gateway_action(
    headers: HeaderMap,
    AxumPath((identity_id, action)): AxumPath<(String, String)>,
) -> Response {
    if let Err(error) = require_morneven_token(&headers) {
        return error;
    }
    if !matches!(action.as_str(), "start" | "stop" | "restart") {
        return response(
            StatusCode::NOT_FOUND,
            json!({"ok": false, "error": "Invalid runtime action"}),
        );
    }
    if matches!(action.as_str(), "start" | "restart") {
        if let Ok(bundle) = fetch_morneven_runtime_bundle().await {
            let _ = materialize_morneven_runtime(&bundle);
        }
    }
    match set_runtime_action(Some(&identity_id), &action) {
        Ok(_) => response(
            StatusCode::OK,
            json!({
                "ok": true,
                "action": action,
                "identityId": identity_id,
                "gateway": gateway_status(),
                "morneven": load_runtime_state()
            }),
        ),
        Err(error) => response(
            StatusCode::BAD_GATEWAY,
            json!({"ok": false, "error": error.to_string()}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_runtime_path_rejects_traversal() {
        assert!(normalize_runtime_path("../secret.txt").is_err());
        assert!(normalize_runtime_path("memory/history.jsonl").is_ok());
    }

    #[test]
    fn content_hash_is_stable() {
        assert_eq!(
            content_hash(b"morneven"),
            "94c02c15d942f312c181956cd1914cb8d75e520e44adc50eec0c7d2084e784e1"
        );
    }
}

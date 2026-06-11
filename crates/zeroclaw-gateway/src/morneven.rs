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
    process::{Child, Command, Stdio},
    sync::OnceLock,
    time::Duration,
};

const MORNEVEN_TOKEN_HEADER: &str = "x-morneven-reload-token";
const BOT_MANAGER_TOKEN_HEADER: &str = "x-bot-manager-sync-token";
const MAX_WORKSPACE_SYNC_BYTES: u64 = 500_000;
const USAGE_EVENT_LIMIT: usize = 5_000;
const DEFAULT_GATEWAY_BASE_PORT: u16 = 18_080;

static RUNTIME_PROCESSES: OnceLock<parking_lot::Mutex<BTreeMap<String, Child>>> = OnceLock::new();

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

#[derive(Debug, Deserialize, Default)]
pub struct WorkspaceChangesQuery {
    #[serde(rename = "includeAll")]
    include_all: Option<String>,
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

fn runtime_processes() -> &'static parking_lot::Mutex<BTreeMap<String, Child>> {
    RUNTIME_PROCESSES.get_or_init(|| parking_lot::Mutex::new(BTreeMap::new()))
}

fn response(status: StatusCode, payload: Value) -> Response {
    (status, Json(payload)).into_response()
}

fn require_morneven_token(headers: &HeaderMap) -> Result<(), Response> {
    if crate::morneven_auth::web_auth_enabled()
        && crate::morneven_auth::require_web_session(headers).is_ok()
    {
        return Ok(());
    }

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
    read_recent_file_lines(&log_path(), limit)
}

fn read_recent_file_lines(path: &Path, limit: usize) -> Vec<String> {
    let Ok(content) = fs::read_to_string(path) else {
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
    serde_json::to_vec_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn json_write(path: &Path, value: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json_bytes(value)?)
}

fn toml_quote(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn zero_provider_name(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "zhipu" | "bigmodel" => "glm".to_string(),
        "vllm" => "vllm".to_string(),
        "deepseek" => "deepseek".to_string(),
        "openrouter" => "openrouter".to_string(),
        "openai" => "openai".to_string(),
        "anthropic" => "anthropic".to_string(),
        "gemini" => "gemini".to_string(),
        "groq" => "groq".to_string(),
        "ollama" => "ollama".to_string(),
        "custom" => "custom".to_string(),
        other => normalize_slug(other),
    }
}

fn credential_value<'a>(credential: &'a Value, camel: &str, snake: &str) -> Option<&'a str> {
    string_field(credential, camel).or_else(|| string_field(credential, snake))
}

fn append_provider_toml(
    out: &mut String,
    entry: &Value,
    provider: &str,
    alias: &str,
) -> Option<String> {
    let credentials = entry.get("credentials")?.as_object()?;
    let credential = credentials.get(provider)?;
    let zero_provider = zero_provider_name(provider);
    let model = credential_value(credential, "modelId", "model_id").unwrap_or_default();
    let api_key = credential_value(credential, "apiKey", "api_key").unwrap_or_default();
    let api_base = credential_value(credential, "apiBase", "api_base").unwrap_or_default();
    out.push_str(&format!("[providers.models.{zero_provider}.{alias}]\n"));
    if zero_provider == "custom" || provider == "vllm" {
        out.push_str("kind = \"openai-compatible\"\n");
    }
    if !api_key.is_empty() {
        out.push_str(&format!("api_key = {}\n", toml_quote(api_key)));
    }
    if !api_base.is_empty() {
        out.push_str(&format!("uri = {}\n", toml_quote(api_base)));
    }
    if !model.is_empty() {
        out.push_str(&format!("model = {}\n", toml_quote(model)));
    }
    out.push('\n');
    Some(format!("{zero_provider}.{alias}"))
}

fn append_telegram_toml(out: &mut String, entry: &Value, alias: &str) -> Option<String> {
    let token = telegram_token_for_alias(entry, alias)?;
    out.push_str(&format!("[channels.telegram.{alias}]\n"));
    out.push_str("enabled = true\n");
    out.push_str(&format!("bot_token = {}\n", toml_quote(token)));
    out.push_str("mention_only = true\n");
    out.push_str("ack_reactions = true\n\n");
    let allowed_peers = telegram_allowed_peers_for_alias(entry, alias);
    if !allowed_peers.is_empty() {
        out.push_str(&format!("[peer_groups.telegram_{alias}]\n"));
        out.push_str(&format!(
            "channel = {}\n",
            toml_quote(&format!("telegram.{alias}"))
        ));
        out.push_str(&format!(
            "external_peers = [{}]\n\n",
            allowed_peers
                .iter()
                .map(|peer| toml_quote(peer))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Some(format!("telegram.{alias}"))
}

fn write_zeroclaw_toml_config(
    config_path: &Path,
    entry: &Value,
    workspace_path: &Path,
    gateway_port: u16,
) -> io::Result<()> {
    let identity = entry.get("identity").cloned().unwrap_or_else(|| json!({}));
    let agent_alias = runtime_slug(&identity);
    let provider = credential_provider(entry).unwrap_or_else(|| "custom".to_string());
    let provider_ref = append_provider_toml_to_string(entry, &provider, "default");
    let channel_ref = append_telegram_toml_to_string(entry, "default");
    let mut out = String::new();

    out.push_str(&format!(
        "schema_version = {}\n",
        zeroclaw_config::migration::CURRENT_SCHEMA_VERSION
    ));
    out.push_str("quickstart_completed = true\n\n");
    out.push_str("[gateway]\n");
    out.push_str("host = \"127.0.0.1\"\n");
    out.push_str(&format!("port = {gateway_port}\n"));
    out.push_str("require_pairing = false\n\n");
    out.push_str("[runtime]\n");
    out.push_str("reasoning_enabled = false\n\n");
    append_morneven_risk_profile_toml(&mut out);
    out.push_str("[runtime_profiles.default]\n");
    out.push_str("agentic = true\n\n");

    if let Some((_, provider_toml)) = &provider_ref {
        out.push_str(provider_toml);
    }
    if let Some((_, channel_toml)) = &channel_ref {
        out.push_str(channel_toml);
    }
    let cron_aliases = append_morneven_cron_toml(&mut out, entry);

    out.push_str(&format!("[agents.{agent_alias}]\n"));
    out.push_str("enabled = true\n");
    if let Some((reference, _)) = &provider_ref {
        out.push_str(&format!("model_provider = {}\n", toml_quote(reference)));
    }
    out.push_str("risk_profile = \"default\"\n");
    out.push_str("runtime_profile = \"default\"\n");
    if let Some((reference, _)) = &channel_ref {
        out.push_str(&format!("channels = [{}]\n", toml_quote(reference)));
    }
    if !cron_aliases.is_empty() {
        out.push_str(&format!(
            "cron_jobs = {}\n",
            toml_string_array(&cron_aliases)
        ));
    }
    out.push('\n');
    out.push_str(&format!("[agents.{agent_alias}.workspace]\n"));
    out.push_str(&format!(
        "path = {}\n",
        toml_quote(workspace_path.to_string_lossy().as_ref())
    ));
    out.push('\n');
    out.push_str("[runtime_profiles.default.thinking]\n");
    out.push_str("default_level = \"off\"\n");
    out.push_str("native_thinking = false\n");

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(config_path, out)
}

fn append_provider_toml_to_string(
    entry: &Value,
    provider: &str,
    alias: &str,
) -> Option<(String, String)> {
    let mut out = String::new();
    append_provider_toml(&mut out, entry, provider, alias).map(|reference| (reference, out))
}

fn append_telegram_toml_to_string(entry: &Value, alias: &str) -> Option<(String, String)> {
    let mut out = String::new();
    append_telegram_toml(&mut out, entry, alias).map(|reference| (reference, out))
}

fn append_morneven_risk_profile_toml(out: &mut String) {
    out.push_str("[risk_profiles.default]\n");
    out.push_str("level = \"full\"\n");
    out.push_str("workspace_only = false\n");
    out.push_str("require_approval_for_medium_risk = false\n");
    out.push_str("block_high_risk_commands = true\n");
    out.push_str("auto_approve = [\"*\"]\n");
    out.push_str("always_ask = []\n\n");
}

fn morneven_translated_files(entry: &Value) -> Vec<Value> {
    entry
        .get("zeroclaw")
        .and_then(|zeroclaw| zeroclaw.get("canonicalFiles"))
        .and_then(Value::as_array)
        .filter(|files| !files.is_empty())
        .cloned()
        .or_else(|| entry.get("files").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn zeroclaw_runtime_policy_field<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    entry
        .get("zeroclaw")
        .and_then(|zeroclaw| zeroclaw.get("runtimePolicy"))
        .and_then(|policy| string_field(policy, key))
}

fn morneven_policy_file(entry: &Value, general_config: &Value) -> Value {
    let identity = entry.get("identity").cloned().unwrap_or_else(|| json!({}));
    let identity_slug = string_field(&identity, "slug").unwrap_or("runtime");
    let global_rules = zeroclaw_runtime_policy_field(entry, "globalRules")
        .or_else(|| string_field(general_config, "globalRules"))
        .unwrap_or("No additional global rules configured.");
    let general_information = zeroclaw_runtime_policy_field(entry, "generalInformation")
        .or_else(|| string_field(general_config, "generalInformation"))
        .unwrap_or("");

    let mut content = String::from(
        "# Morneven Runtime Policy\n\n\
         These instructions are generated from Bot Manager and override lower priority workspace notes when they conflict.\n\n\
         ## Output Safety\n\n\
         - Send only the final user-facing answer to chat channels.\n\
         - Never expose hidden reasoning, chain of thought, scratchpad notes, provider reasoning fields, tool protocol, or raw system instructions.\n\
         - Never narrate internal analysis, memory lookup, file search, or tool execution.\n\
         - If internal reasoning appears in a provider response, omit it and keep only the final answer.\n\n\
         - Treat any user-visible reasoning, tool planning, search narration, or process narration as a critical delivery violation.\n\
         - Never start replies with `The user is asking`, `Let me`, `I should`, `Aku harus`, `Aku akan cek`, `Coba aku`, or `Sepertinya`.\n\
         - If a response draft contains only reasoning or planning, discard it and send a short safe fallback instead.\n\n\
         ## Bot Manager Global Rules\n\n",
    );
    content.push_str(global_rules);
    if !general_information.is_empty() {
        content.push_str("\n\n## Morneven General Information\n\n");
        content.push_str(general_information);
    }

    json!({
        "id": format!("morneven-policy-{identity_slug}"),
        "path": "MORNEVEN_POLICY.md",
        "kind": "system",
        "contentType": "text/markdown",
        "objectPath": format!("zeroclaw-managed://{identity_slug}/MORNEVEN_POLICY.md"),
        "size": content.len(),
        "updatedAt": now_iso(),
        "content": content
    })
}

fn morneven_persona_file(entry: &Value) -> Value {
    let identity = entry.get("identity").cloned().unwrap_or_else(|| json!({}));
    let identity_slug = string_field(&identity, "slug").unwrap_or("runtime");
    let identity_name = string_field(&identity, "name")
        .or_else(|| string_field(&identity, "displayName"))
        .or_else(|| string_field(&identity, "slug"))
        .unwrap_or("the active Morneven personality");
    let role_title = string_field(&identity, "roleTitle")
        .or_else(|| string_field(&identity, "role"))
        .or_else(|| string_field(&identity, "title"))
        .unwrap_or("");
    let description = string_field(&identity, "description")
        .or_else(|| string_field(&identity, "bio"))
        .or_else(|| string_field(&identity, "summary"))
        .unwrap_or("");

    let mut content = String::from(
        "# Morneven Persona Lock\n\n\
         This file is generated from Bot Manager and has higher priority than ordinary workspace notes.\n\n\
         ## Mandatory Behavior\n\n\
         - Always respond as the active Morneven personality for normal chat, facts, tool results, cron output, and follow-up replies.\n\
         - Treat `AGENTS.md`, `SOUL.md`, `IDENTITY.md`, `USER.md`, and `MEMORY.md` as mandatory persona instructions, not optional reference material.\n\
         - Keep the personality voice, relationship dynamic, lore, habits, emotional texture, and configured language style while still answering the user's actual request.\n\
         - Never switch into generic assistant mode unless the user explicitly asks for system debugging, code implementation, or operational diagnostics.\n\
         - Do not say that you are roleplaying, following a persona lock, reading memory, checking files, or using tools.\n\
         - If a fact or tool result is needed, keep the factual content accurate and phrase the final answer in character.\n\
         - Use Indonesian by default when no language is requested. If the user asks for English or another language, answer in that language while staying in character.\n\
         - Only send the final user-facing message. Do not expose internal reasoning, analysis, scratchpad, or tool protocol.\n\n\
         - User-visible reasoning, tool planning, search narration, or process narration is a critical delivery violation.\n\
         - Never include phrases such as `The user is asking`, `Let me`, `I should`, `Aku harus`, `Aku akan cek`, `Coba aku`, or `Sepertinya` as internal process narration.\n\n\
         ## Active Personality\n\n",
    );
    content.push_str(&format!("- Name: {identity_name}\n"));
    if !role_title.is_empty() {
        content.push_str(&format!("- Role: {role_title}\n"));
    }
    if !description.is_empty() {
        content.push_str(&format!("- Description: {description}\n"));
    }
    content.push_str("\nThe personality files injected below define the full character. Follow them before ordinary workspace notes when they conflict.\n");

    json!({
        "id": format!("morneven-persona-{identity_slug}"),
        "path": "MORNEVEN_PERSONA.md",
        "kind": "system",
        "contentType": "text/markdown",
        "objectPath": format!("zeroclaw-managed://{identity_slug}/MORNEVEN_PERSONA.md"),
        "size": content.len(),
        "updatedAt": now_iso(),
        "content": content
    })
}

fn morneven_cron_schedule_label(schedule: &Value) -> String {
    let kind = string_field(schedule, "kind").unwrap_or("cron");
    match kind {
        "every" => schedule
            .get("every_ms")
            .or_else(|| schedule.get("everyMs"))
            .and_then(Value::as_u64)
            .map(|value| format!("every {value} ms"))
            .unwrap_or_else(|| "every interval".to_string()),
        "at" => string_field(schedule, "at")
            .map(|value| format!("at {value}"))
            .unwrap_or_else(|| "at one-shot time".to_string()),
        _ => {
            let expr = string_field(schedule, "expr")
                .or_else(|| string_field(schedule, "expression"))
                .unwrap_or("* * * * *");
            let tz = string_field(schedule, "tz")
                .or_else(|| string_field(schedule, "timezone"))
                .unwrap_or("runtime timezone");
            format!("{expr} ({tz})")
        }
    }
}

fn morneven_cron_delivery_label(job: &Value) -> String {
    let Some(delivery) = job.get("delivery").filter(|value| value.is_object()) else {
        return "no delivery configured".to_string();
    };
    let channel = string_field(delivery, "channel").unwrap_or("channel not set");
    let to = string_field(delivery, "to").unwrap_or("recipient not set");
    let thread = string_field(delivery, "threadId")
        .or_else(|| string_field(delivery, "thread_id"))
        .map(|value| format!(" thread {value}"))
        .unwrap_or_default();
    format!("{channel} to {to}{thread}")
}

fn morneven_cron_file(entry: &Value) -> Option<Value> {
    let jobs = morneven_cron_jobs(entry);
    if jobs.is_empty() {
        return None;
    }
    let identity = entry.get("identity").cloned().unwrap_or_else(|| json!({}));
    let identity_slug = string_field(&identity, "slug").unwrap_or("runtime");
    let mut content = String::from(
        "# Morneven Cron Jobs\n\n\
         This read-only summary is generated from Bot Manager cron data. Use it when the user asks what scheduled jobs or routines exist. For live status, use cron_list when available.\n\n",
    );
    for job in jobs {
        let id = string_field(&job, "id")
            .or_else(|| string_field(&job, "alias"))
            .unwrap_or("cron-job");
        let name = string_field(&job, "name").unwrap_or(id);
        let enabled = job.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let job_type = string_field(&job, "jobType")
            .or_else(|| string_field(&job, "job_type"))
            .unwrap_or("agent");
        let schedule_label = job
            .get("schedule")
            .map(morneven_cron_schedule_label)
            .unwrap_or_else(|| "schedule not set".to_string());
        let prompt = string_field(&job, "prompt")
            .or_else(|| string_field(&job, "command"))
            .unwrap_or("");
        let delivery = morneven_cron_delivery_label(&job);
        content.push_str(&format!(
            "## {name}\n\n- ID: `{id}`\n- Enabled: {enabled}\n- Type: {job_type}\n- Schedule: `{schedule_label}`\n- Delivery: {delivery}\n"
        ));
        if !prompt.is_empty() {
            content.push_str(&format!("- Task: {prompt}\n"));
        }
        if let Some(source_path) = string_field(&job, "sourcePath") {
            content.push_str(&format!("- Source: `{source_path}`\n"));
        }
        content.push('\n');
    }

    Some(json!({
        "id": format!("morneven-cron-{identity_slug}"),
        "path": "MORNEVEN_CRON.md",
        "kind": "cron",
        "contentType": "text/markdown",
        "objectPath": format!("zeroclaw-managed://{identity_slug}/MORNEVEN_CRON.md"),
        "size": content.len(),
        "updatedAt": now_iso(),
        "content": content
    }))
}

fn morneven_topic_id_text(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() || matches!(trimmed.to_ascii_lowercase().as_str(), "0" | "1" | "main") {
        "main".to_string()
    } else {
        trimmed.to_string()
    }
}

fn morneven_topic_id(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => morneven_topic_id_text(text),
        Some(Value::Number(number)) => morneven_topic_id_text(&number.to_string()),
        _ => "main".to_string(),
    }
}

fn morneven_lock_group<'a>(lock: &'a Value, chat_id: &str) -> Option<&'a Value> {
    lock.get("groups")
        .and_then(Value::as_array)?
        .iter()
        .find(|group| {
            group
                .get("chatId")
                .or_else(|| group.get("chat_id"))
                .map(|value| match value {
                    Value::String(text) => text.trim().to_string(),
                    Value::Number(number) => number.to_string(),
                    _ => String::new(),
                })
                .as_deref()
                == Some(chat_id)
        })
}

fn morneven_topic_allowed(lock_group: Option<&Value>, topic_id: &str) -> Option<bool> {
    let group = lock_group?;
    if topic_id == "main" {
        return group.get("allowMainTopic").and_then(Value::as_bool);
    }
    Some(
        group
            .get("allowedTopicIds")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .any(|item| morneven_topic_id(Some(item)) == topic_id)
            })
            .unwrap_or(false),
    )
}

fn morneven_telegram_topics_markdown(registry: &Value) -> String {
    let mut content = String::from(
        "# Morneven Telegram Topics\n\n\
         This read-only summary is generated from Bot Manager Telegram topic registry and topic lock data. Use it when the user asks about Telegram groups, topics, topic IDs, primary topics, or where scheduled/outbound messages should be sent.\n\n",
    );
    let lock = registry.get("topicLock").or_else(|| {
        if registry.get("enabled").is_some() || registry.get("groups").is_some() {
            Some(registry)
        } else {
            None
        }
    });
    let groups = registry
        .get("groups")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if groups.is_empty() {
        content.push_str("No Telegram groups or topics have been registered yet.\n");
        return content;
    }
    for group in groups {
        let chat_id = string_field(&group, "chatId")
            .or_else(|| string_field(&group, "chat_id"))
            .unwrap_or("unknown");
        let title = string_field(&group, "title").unwrap_or("Untitled group");
        let is_forum = group
            .get("isForum")
            .or_else(|| group.get("is_forum"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let lock_group = lock.and_then(|lock| morneven_lock_group(lock, chat_id));
        let allow_main = morneven_topic_allowed(lock_group, "main").unwrap_or(true);
        let primary = lock_group
            .and_then(|group| {
                group
                    .get("primaryTopicId")
                    .or_else(|| group.get("primary_topic_id"))
            })
            .map(|value| morneven_topic_id(Some(value)))
            .unwrap_or_else(|| "main".to_string());

        content.push_str(&format!(
            "## {title}\n\n- Chat ID: `{chat_id}`\n- Forum group: {is_forum}\n- Main topic allowed: {allow_main}\n- Primary topic ID: `{primary}`\n\n"
        ));

        let topics = group
            .get("topics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if topics.is_empty() {
            content.push_str("- Topics: none registered yet.\n\n");
            continue;
        }
        content.push_str("| Topic ID | Title | Allowed | Primary | Source |\n");
        content.push_str("| --- | --- | --- | --- | --- |\n");
        for topic in topics {
            let topic_id = morneven_topic_id(
                topic
                    .get("messageThreadId")
                    .or_else(|| topic.get("message_thread_id")),
            );
            let topic_title = string_field(&topic, "title").unwrap_or(if topic_id == "main" {
                "Main topic"
            } else {
                "Untitled topic"
            });
            let source = string_field(&topic, "source").unwrap_or("unknown");
            let allowed = morneven_topic_allowed(lock_group, &topic_id)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let is_primary = topic_id == primary;
            content.push_str(&format!(
                "| `{topic_id}` | {topic_title} | {allowed} | {is_primary} | {source} |\n"
            ));
        }
        content.push('\n');
    }
    content
}

fn morneven_telegram_topics_file(entry: &Value) -> Option<Value> {
    let registry = topic_registry_from_entry(entry);
    let has_groups = registry
        .get("groups")
        .and_then(Value::as_array)
        .is_some_and(|groups| !groups.is_empty());
    let has_lock = registry.get("topicLock").is_some();
    if !has_groups && !has_lock {
        return None;
    }
    let identity = entry.get("identity").cloned().unwrap_or_else(|| json!({}));
    let identity_slug = string_field(&identity, "slug").unwrap_or("runtime");
    let content = morneven_telegram_topics_markdown(&registry);
    Some(json!({
        "id": format!("morneven-telegram-topics-{identity_slug}"),
        "path": "MORNEVEN_TELEGRAM_TOPICS.md",
        "kind": "telegram-topics",
        "contentType": "text/markdown",
        "objectPath": format!("zeroclaw-managed://{identity_slug}/MORNEVEN_TELEGRAM_TOPICS.md"),
        "size": content.len(),
        "updatedAt": now_iso(),
        "content": content
    }))
}

fn nanobot_legacy_root_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for key in ["MORNEVEN_NANOBOT_LEGACY_ROOT", "NANOBOT_LEGACY_ROOT"] {
        if let Ok(path) = env::var(key) {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                roots.push(PathBuf::from(trimmed));
            }
        }
    }
    if let Some(parent) = runtime_root().parent() {
        roots.push(parent.join("legacy").join("nanobot"));
    }
    roots.push(PathBuf::from("/data/.nanobot"));

    let mut unique = Vec::new();
    for root in roots {
        if !unique.iter().any(|existing| existing == &root) {
            unique.push(root);
        }
    }
    unique
}

fn legacy_nanobot_runtime_dirs(identity: &Value) -> Vec<PathBuf> {
    let slug = runtime_slug(identity);
    let safe_id = normalize_slug(string_field(identity, "id").unwrap_or(&slug));
    let suffix: String = safe_id.chars().take(8).collect();
    let runtime_dir_name = format!("{slug}-{suffix}");
    let mut dirs = Vec::new();
    for root in nanobot_legacy_root_candidates() {
        if root.join("workspace").is_dir() {
            dirs.push(root);
            continue;
        }
        let runtimes_root = if root.file_name().and_then(|name| name.to_str()) == Some("runtimes") {
            root
        } else {
            root.join("runtimes")
        };
        dirs.push(runtimes_root.join(&runtime_dir_name));
    }
    dirs
}

fn canonicalize_legacy_nanobot_path(relative_path: &str) -> String {
    let normalized = relative_path
        .trim()
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_string();
    let lower = normalized.to_ascii_lowercase();
    if lower.starts_with("sessions/") {
        return format!("legacy/nanobot/{normalized}");
    }
    match lower.as_str() {
        "agents.md" => "AGENTS.md".to_string(),
        "soul.md" => "SOUL.md".to_string(),
        "identity.md" => "IDENTITY.md".to_string(),
        "user.md" => "USER.md".to_string(),
        "tools.md" => "TOOLS.md".to_string(),
        "heartbeat.md" => "HEARTBEAT.md".to_string(),
        "bootstrap.md" => "BOOTSTRAP.md".to_string(),
        "memory.md" => "MEMORY.md".to_string(),
        "lore.md" => "LORE.md".to_string(),
        _ => normalized,
    }
}

fn legacy_nanobot_workspace_files(identity: &Value) -> Vec<Value> {
    let mut files = BTreeMap::new();
    for runtime_dir in legacy_nanobot_runtime_dirs(identity) {
        let workspace_path = runtime_dir.join("workspace");
        if !workspace_path.is_dir() {
            continue;
        }
        let mut stack = vec![workspace_path.clone()];
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
                let Some(relative_path) = path
                    .strip_prefix(&workspace_path)
                    .ok()
                    .and_then(|path| path.to_str())
                    .map(|path| path.replace('\\', "/"))
                else {
                    continue;
                };
                let Ok(relative_path) = normalize_runtime_path(&relative_path) else {
                    continue;
                };
                let runtime_path = canonicalize_legacy_nanobot_path(&relative_path);
                let Ok(runtime_path) = normalize_runtime_path(&runtime_path) else {
                    continue;
                };
                let Ok((content, stat)) = read_workspace_text(&path) else {
                    continue;
                };
                let updated_at = stat
                    .modified()
                    .ok()
                    .map(DateTime::<Utc>::from)
                    .map(|time| time.to_rfc3339())
                    .unwrap_or_else(now_iso);
                let content_type =
                    if runtime_path.ends_with(".json") || runtime_path.ends_with(".jsonl") {
                        "application/json"
                    } else {
                        "text/markdown"
                    };
                let kind = infer_workspace_kind(&runtime_path);
                let object_path = format!("legacy-nanobot://{}", relative_path);
                let file_id = format!("legacy-nanobot-{}", content_hash(relative_path.as_bytes()));
                files.insert(
                    runtime_path.to_ascii_lowercase(),
                    json!({
                        "id": file_id,
                        "path": runtime_path,
                        "kind": kind,
                        "contentType": content_type,
                        "objectPath": object_path,
                        "sourcePath": relative_path,
                        "size": content.len(),
                        "updatedAt": updated_at,
                        "content": content
                    }),
                );
            }
        }
    }
    files.into_values().collect()
}

fn is_generated_zeroclaw_file(file: &Value) -> bool {
    string_field(file, "id").is_some_and(|id| id.starts_with("zeroclaw-managed-"))
        || string_field(file, "objectPath")
            .is_some_and(|path| path.starts_with("zeroclaw-managed://"))
}

fn merge_runtime_materialization_files(
    legacy_files: Vec<Value>,
    bundle_files: Vec<Value>,
) -> Vec<Value> {
    let mut files = BTreeMap::new();
    for file in legacy_files {
        if let Some(path) = string_field(&file, "path") {
            files.insert(path.to_ascii_lowercase(), file);
        }
    }
    for file in bundle_files {
        let Some(path) = string_field(&file, "path") else {
            continue;
        };
        let key = path.to_ascii_lowercase();
        if files.contains_key(&key) && is_generated_zeroclaw_file(&file) {
            continue;
        }
        files.insert(key, file);
    }
    files.into_values().collect()
}

fn runtime_files_for_materialization(
    entry: &Value,
    identity: &Value,
    general_config: &Value,
) -> Vec<Value> {
    let mut bundle_files = morneven_translated_files(entry);
    let has_policy = bundle_files.iter().any(|file| {
        string_field(file, "path")
            .is_some_and(|path| path.eq_ignore_ascii_case("MORNEVEN_POLICY.md"))
    });
    if !has_policy {
        bundle_files.push(morneven_policy_file(entry, general_config));
    }
    let has_persona = bundle_files.iter().any(|file| {
        string_field(file, "path")
            .is_some_and(|path| path.eq_ignore_ascii_case("MORNEVEN_PERSONA.md"))
    });
    if !has_persona {
        bundle_files.push(morneven_persona_file(entry));
    }
    let has_cron_summary = bundle_files.iter().any(|file| {
        string_field(file, "path").is_some_and(|path| path.eq_ignore_ascii_case("MORNEVEN_CRON.md"))
    });
    if !has_cron_summary && let Some(file) = morneven_cron_file(entry) {
        bundle_files.push(file);
    }
    let has_topic_summary = bundle_files.iter().any(|file| {
        string_field(file, "path")
            .is_some_and(|path| path.eq_ignore_ascii_case("MORNEVEN_TELEGRAM_TOPICS.md"))
    });
    if !has_topic_summary && let Some(file) = morneven_telegram_topics_file(entry) {
        bundle_files.push(file);
    }

    merge_runtime_materialization_files(legacy_nanobot_workspace_files(identity), bundle_files)
}

fn morneven_cron_jobs(entry: &Value) -> Vec<Value> {
    entry
        .get("zeroclaw")
        .and_then(|zeroclaw| zeroclaw.get("cron"))
        .and_then(|cron| cron.get("jobs"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn toml_string_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| toml_quote(value))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn append_optional_toml_string(out: &mut String, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        out.push_str(&format!("{key} = {}\n", toml_quote(value)));
    }
}

fn append_morneven_cron_toml(out: &mut String, entry: &Value) -> Vec<String> {
    let mut aliases = Vec::new();
    for job in morneven_cron_jobs(entry) {
        let Some(schedule) = job.get("schedule").filter(|value| value.is_object()) else {
            continue;
        };
        let raw_id = string_field(&job, "id")
            .or_else(|| string_field(&job, "alias"))
            .or_else(|| string_field(&job, "name"))
            .unwrap_or("morneven-cron");
        let alias = normalize_slug(raw_id);
        let job_type = string_field(&job, "jobType")
            .or_else(|| string_field(&job, "job_type"))
            .unwrap_or("agent");
        let command = string_field(&job, "command");
        let prompt = string_field(&job, "prompt");
        if job_type == "shell" && command.is_none() {
            continue;
        }
        if job_type != "shell" && prompt.is_none() {
            continue;
        }

        let schedule_kind = string_field(schedule, "kind").unwrap_or("cron");
        let schedule_is_valid = match schedule_kind {
            "every" => schedule
                .get("every_ms")
                .or_else(|| schedule.get("everyMs"))
                .and_then(Value::as_u64)
                .is_some(),
            "at" => string_field(schedule, "at").is_some(),
            _ => string_field(schedule, "expr")
                .or_else(|| string_field(schedule, "expression"))
                .is_some(),
        };
        if !schedule_is_valid {
            continue;
        }

        out.push_str(&format!("[cron.{alias}]\n"));
        append_optional_toml_string(out, "name", string_field(&job, "name"));
        out.push_str(&format!("job_type = {}\n", toml_quote(job_type)));
        out.push_str(&format!(
            "enabled = {}\n",
            job.get("enabled").and_then(Value::as_bool).unwrap_or(true)
        ));
        out.push_str(&format!(
            "uses_memory = {}\n",
            job.get("usesMemory")
                .or_else(|| job.get("uses_memory"))
                .and_then(Value::as_bool)
                .unwrap_or(true)
        ));
        append_optional_toml_string(out, "command", command);
        append_optional_toml_string(out, "prompt", prompt);
        append_optional_toml_string(
            out,
            "model",
            string_field(&job, "model")
                .or_else(|| string_field(&job, "modelId"))
                .or_else(|| string_field(&job, "model_id")),
        );
        append_optional_toml_string(
            out,
            "session_target",
            string_field(&job, "sessionTarget").or_else(|| string_field(&job, "session_target")),
        );
        let allowed_tools = string_array_field(&job, "allowedTools");
        let allowed_tools = if allowed_tools.is_empty() {
            string_array_field(&job, "allowed_tools")
        } else {
            allowed_tools
        };
        if !allowed_tools.is_empty() {
            out.push_str(&format!(
                "allowed_tools = {}\n",
                toml_string_array(&allowed_tools)
            ));
        }
        out.push('\n');

        out.push_str(&format!("[cron.{alias}.schedule]\n"));
        out.push_str(&format!("kind = {}\n", toml_quote(schedule_kind)));
        match schedule_kind {
            "every" => {
                if let Some(every_ms) = schedule
                    .get("every_ms")
                    .or_else(|| schedule.get("everyMs"))
                    .and_then(Value::as_u64)
                {
                    out.push_str(&format!("every_ms = {every_ms}\n"));
                }
            }
            "at" => append_optional_toml_string(out, "at", string_field(schedule, "at")),
            _ => {
                append_optional_toml_string(
                    out,
                    "expr",
                    string_field(schedule, "expr").or_else(|| string_field(schedule, "expression")),
                );
                append_optional_toml_string(
                    out,
                    "tz",
                    string_field(schedule, "tz").or_else(|| string_field(schedule, "timezone")),
                );
            }
        }
        out.push('\n');

        if let Some(delivery) = job.get("delivery").filter(|value| value.is_object()) {
            out.push_str(&format!("[cron.{alias}.delivery]\n"));
            append_optional_toml_string(out, "mode", string_field(delivery, "mode"));
            append_optional_toml_string(out, "channel", string_field(delivery, "channel"));
            append_optional_toml_string(out, "to", string_field(delivery, "to"));
            append_optional_toml_string(
                out,
                "thread_id",
                string_field(delivery, "threadId").or_else(|| string_field(delivery, "thread_id")),
            );
            out.push_str(&format!(
                "best_effort = {}\n\n",
                delivery
                    .get("bestEffort")
                    .or_else(|| delivery.get("best_effort"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true)
            ));
        }

        aliases.push(alias);
    }
    aliases
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
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn string_array_field(value: &Value, key: &str) -> Vec<String> {
    match value.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Some(Value::String(text)) => text
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        _ => Vec::new(),
    }
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
    let normalized = raw
        .trim()
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_string();
    if normalized.is_empty() || normalized.len() > 240 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid runtime file path",
        ));
    }
    if normalized
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/')))
    {
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

fn write_manifest(
    path: &Path,
    files: BTreeMap<String, ManifestFile>,
    identity: Value,
) -> io::Result<()> {
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
    entry
        .get("credentials")
        .and_then(|credentials| credentials.get(provider))
        .and_then(|credential| {
            string_field(credential, "modelId")
                .or_else(|| string_field(credential, "model_id"))
                .map(ToOwned::to_owned)
        })
}

fn telegram_root(entry: &Value) -> Option<&Value> {
    entry
        .get("channels")
        .and_then(|channels| channels.get("telegram"))
}

fn telegram_token_value(value: &Value) -> Option<&str> {
    string_field(value, "token")
        .or_else(|| string_field(value, "botToken"))
        .or_else(|| string_field(value, "bot_token"))
}

fn enabled_with_fallback(value: &Value, fallback: bool) -> bool {
    value
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(fallback)
}

fn telegram_allowed_peer_values(value: &Value) -> Vec<String> {
    [
        "allowFrom",
        "allow_from",
        "allowedUserIds",
        "allowed_user_ids",
        "allowedUsers",
        "allowed_users",
    ]
    .into_iter()
    .flat_map(|key| string_array_field(value, key))
    .collect()
}

fn normalize_telegram_peer(value: &str) -> String {
    value.trim().trim_start_matches('@').to_string()
}

fn dedupe_telegram_peers(peers: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for peer in peers {
        let normalized = normalize_telegram_peer(&peer);
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

fn telegram_allowed_peers_for_alias(entry: &Value, alias: &str) -> Vec<String> {
    let Some(telegram) = telegram_root(entry) else {
        return Vec::new();
    };
    let root_peers = telegram_allowed_peer_values(telegram);
    let alias_peers = telegram
        .get(alias)
        .filter(|value| value.is_object())
        .map(telegram_allowed_peer_values)
        .unwrap_or_default();
    dedupe_telegram_peers(root_peers.into_iter().chain(alias_peers))
}

fn telegram_token_for_alias<'a>(entry: &'a Value, alias: &str) -> Option<&'a str> {
    let telegram = telegram_root(entry)?;
    let root_enabled = bool_field(telegram, "enabled");
    if root_enabled {
        if let Some(token) = telegram_token_value(telegram) {
            return Some(token);
        }
    }
    if let Some(alias_config) = telegram.get(alias).filter(|value| value.is_object()) {
        if enabled_with_fallback(alias_config, root_enabled) {
            if let Some(token) = telegram_token_value(alias_config) {
                return Some(token);
            }
        }
    }
    if root_enabled {
        return value_object(telegram)
            .into_iter()
            .flat_map(|children| children.values())
            .find_map(telegram_token_value);
    }
    None
}

fn telegram_token_fingerprint(entry: &Value) -> Option<String> {
    let token = telegram_token_for_alias(entry, "default")?;
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
    let telegram = entry
        .get("channels")
        .and_then(|channels| channels.get("telegram"));
    let mut registry = telegram
        .and_then(|telegram| telegram.get("topicRegistry"))
        .filter(|registry| registry.get("groups").and_then(Value::as_array).is_some())
        .cloned()
        .unwrap_or_else(|| json!({"groups": []}));
    if let (Some(registry_object), Some(lock)) = (
        registry.as_object_mut(),
        telegram.and_then(|telegram| telegram.get("topicLock")),
    ) {
        registry_object.insert("topicLock".to_string(), lock.clone());
    }
    registry
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
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Runtime bundle does not contain an active identity",
            )
        })?;
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
        "zeroclaw": entry.get("zeroclaw").cloned().unwrap_or_else(|| json!({})),
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
    let identity_id = string_field(&identity, "id")
        .unwrap_or_default()
        .to_string();
    let runtime_dir = runtime_dir_for_identity(&identity);
    let workspace_path = runtime_dir.join("workspace");
    let manifest_path = runtime_dir.join(".morneven-runtime-manifest.json");
    let config_path = runtime_dir.join("config.json");
    let zeroclaw_config_path = runtime_dir.join("config.toml");
    fs::create_dir_all(&workspace_path)?;
    let previous_manifest = load_manifest(&manifest_path);
    let mut written = BTreeMap::new();
    let mut written_paths = HashSet::new();

    let files = runtime_files_for_materialization(entry, &identity, general_config);
    let legacy_nanobot_file_count = files
        .iter()
        .filter(|file| {
            string_field(file, "objectPath")
                .is_some_and(|path| path.starts_with("legacy-nanobot://"))
        })
        .count();
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

    write_runtime_config(
        &config_path,
        entry,
        general_config,
        &workspace_path,
        gateway_port,
    )?;
    write_zeroclaw_toml_config(&zeroclaw_config_path, entry, &workspace_path, gateway_port)?;
    let topic_registry = topic_registry_from_entry(entry);
    json_write(&runtime_dir.join("telegram-topics.json"), &topic_registry)?;
    json_write(
        &workspace_path.join("MORNEVEN_TELEGRAM_TOPICS.json"),
        &topic_registry,
    )?;
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
        "zeroclawConfigPath": zeroclaw_config_path.to_string_lossy(),
        "telegramTopicsPath": runtime_dir.join("telegram-topics.json").to_string_lossy(),
        "usageEventsPath": runtime_dir
            .join("data")
            .join("state")
            .join("costs.jsonl")
            .to_string_lossy(),
        "legacyUsageEventsPath": runtime_dir.join("provider-usage.jsonl").to_string_lossy(),
        "gatewayPort": gateway_port,
        "telegramBotUsername": null,
        "telegramTokenFingerprint": telegram_token_fingerprint(entry),
        "telegramActiveBotUsernames": [],
        "autoDreamEnabled": auto_dream_enabled(entry),
        "legacyNanobotFileCount": legacy_nanobot_file_count,
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
        .unwrap_or_else(|| {
            entries[0]
                .get("identity")
                .cloned()
                .unwrap_or_else(|| json!({}))
        });
    let main_identity_id = string_field(&main_identity, "id")
        .or_else(|| {
            entries[0]
                .get("identity")
                .and_then(|identity| string_field(identity, "id"))
        })
        .unwrap_or_default()
        .to_string();
    let general_config = bundle
        .get("generalConfig")
        .cloned()
        .unwrap_or_else(|| json!({}));
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
        .map(|runtime| {
            runtime
                .get("fileCount")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize
        })
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
    append_log(format!(
        "runtime synced: {} runtime(s)",
        state["runtimeCount"]
    ));
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
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid runtime action",
            ));
        }
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
        identity_id
            .map(|id| format!(" for {id}"))
            .unwrap_or_default()
    ));
    Ok(desired)
}

fn ensure_runtime_desired_entry<'a>(
    desired: &'a mut DesiredGatewayState,
    identity_id: &str,
) -> &'a mut DesiredRuntimeState {
    desired
        .runtimes
        .entry(identity_id.to_string())
        .or_insert_with(|| DesiredRuntimeState {
            identity_id: identity_id.to_string(),
            state: "stopped".to_string(),
            started_at: None,
            stopped_at: None,
            restart_count: 0,
            last_action_at: None,
        })
}

fn mark_runtime_started(identity_id: &str, reset_started_at: bool) -> io::Result<String> {
    let mut desired = load_desired_state();
    let now = now_iso();
    let started_at = {
        let entry = ensure_runtime_desired_entry(&mut desired, identity_id);
        entry.state = "running".to_string();
        entry.stopped_at = None;
        if reset_started_at || entry.started_at.is_none() {
            entry.started_at = Some(now.clone());
        }
        entry.started_at.clone().unwrap_or_else(|| now.clone())
    };
    desired.updated_at = Some(now);
    save_desired_state(&desired)?;
    Ok(started_at)
}

fn mark_runtime_stopped(identity_id: &str) -> io::Result<()> {
    let mut desired = load_desired_state();
    let now = now_iso();
    {
        let entry = ensure_runtime_desired_entry(&mut desired, identity_id);
        entry.state = "stopped".to_string();
        entry.stopped_at = Some(now.clone());
    }
    desired.updated_at = Some(now);
    save_desired_state(&desired)
}

fn runtime_entries() -> Vec<Value> {
    load_runtime_state()
        .get("runtimes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn runtime_by_identity(identity_id: &str) -> Option<Value> {
    runtime_entries()
        .into_iter()
        .find(|runtime| string_field(runtime, "identityId") == Some(identity_id))
}

fn pid_path_for_runtime(runtime: &Value) -> PathBuf {
    PathBuf::from(string_field(runtime, "runtimePath").unwrap_or_default()).join("gateway.pid")
}

fn runtime_log_path(runtime: &Value) -> PathBuf {
    PathBuf::from(string_field(runtime, "runtimePath").unwrap_or_default()).join("gateway.log")
}

fn read_pid_file(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
}

fn write_pid_file(path: &Path, pid: u32) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, pid.to_string())
}

fn remove_pid_file(path: &Path) {
    if path.exists() {
        let _ = fs::remove_file(path);
    }
}

fn runtime_process_snapshot(identity_id: &str) -> (bool, Option<u32>, Option<i32>) {
    let mut processes = runtime_processes().lock();
    let mut remove_process = false;
    let snapshot = if let Some(child) = processes.get_mut(identity_id) {
        match child.try_wait() {
            Ok(None) => (true, Some(child.id()), None),
            Ok(Some(status)) => {
                remove_process = true;
                (false, None, status.code())
            }
            Err(_) => {
                remove_process = true;
                (false, None, None)
            }
        }
    } else {
        (false, None, None)
    };
    if remove_process {
        processes.remove(identity_id);
    }
    snapshot
}

fn spawn_gateway_process(runtime: &Value) -> io::Result<u32> {
    let identity_id = string_field(runtime, "identityId")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Runtime identityId is missing")
        })?;
    let (running, pid, _) = runtime_process_snapshot(identity_id);
    if running {
        let _ = mark_runtime_started(identity_id, false);
        return pid.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Runtime process is running without a PID",
            )
        });
    }
    let pid_path = pid_path_for_runtime(runtime);
    if let Some(pid) = read_pid_file(&pid_path) {
        let _ = stop_external_pid(pid);
        remove_pid_file(&pid_path);
    }

    let runtime_dir = PathBuf::from(string_field(runtime, "runtimePath").unwrap_or_default());
    if runtime_dir.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Runtime path is missing",
        ));
    }
    fs::create_dir_all(&runtime_dir)?;
    let port = runtime
        .get("gatewayPort")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Runtime gatewayPort is missing",
            )
        })?;
    let executable = env::current_exe()?;
    let runtime_log = runtime_log_path(runtime);
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&runtime_log)?;
    let stderr_file = log_file.try_clone()?;
    let mut child = Command::new(executable)
        .arg("--config-dir")
        .arg(&runtime_dir)
        .arg("daemon")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .env("ZEROCLAW_CONFIG_DIR", runtime_dir.as_os_str())
        .env("ZEROCLAW_DATA_DIR", runtime_dir.join("data").as_os_str())
        .env("MORNEVEN_CHILD_RUNTIME", "1")
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()?;
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(500));
    if let Some(status) = child.try_wait()? {
        let last_log = read_recent_file_lines(&runtime_log, 5).join("\n");
        remove_pid_file(&pid_path);
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "Runtime process exited during startup with status {status}. Last log: {last_log}"
            ),
        ));
    }
    if let Err(error) = write_pid_file(&pid_path, pid) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    runtime_processes()
        .lock()
        .insert(identity_id.to_string(), child);
    let _ = mark_runtime_started(identity_id, true);
    append_log(format!("runtime {identity_id} started pid={pid}"));
    Ok(pid)
}

fn stop_external_pid(pid: u32) -> io::Result<()> {
    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .status()?;
        if !status.success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("taskkill failed for PID {pid}"),
            ));
        }
    }
    #[cfg(not(windows))]
    {
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()?;
        if !status.success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("kill failed for PID {pid}"),
            ));
        }
    }
    Ok(())
}

fn stop_gateway_process(runtime: &Value) -> io::Result<()> {
    let identity_id = string_field(runtime, "identityId")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Runtime identityId is missing")
        })?;
    let pid_path = pid_path_for_runtime(runtime);
    let child = runtime_processes().lock().remove(identity_id);
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait();
    } else if let Some(pid) = read_pid_file(&pid_path) {
        let _ = stop_external_pid(pid);
    }
    remove_pid_file(&pid_path);
    let _ = mark_runtime_stopped(identity_id);
    append_log(format!("runtime {identity_id} stopped"));
    Ok(())
}

fn apply_runtime_process_action(runtime: &Value, action: &str) -> io::Result<Option<u32>> {
    match action {
        "start" => spawn_gateway_process(runtime).map(Some),
        "stop" => {
            stop_gateway_process(runtime)?;
            Ok(None)
        }
        "restart" => {
            stop_gateway_process(runtime)?;
            spawn_gateway_process(runtime).map(Some)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid runtime action",
        )),
    }
}

fn apply_gateway_process_action(action: &str) -> io::Result<Vec<Value>> {
    let mut results = Vec::new();
    let runtimes = runtime_entries();
    if runtimes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "No Morneven runtimes have been materialized",
        ));
    }
    for runtime in runtimes {
        let identity_id = string_field(&runtime, "identityId")
            .unwrap_or_default()
            .to_string();
        match apply_runtime_process_action(&runtime, action) {
            Ok(pid) => results.push(json!({
                "identityId": identity_id,
                "ok": true,
                "pid": pid
            })),
            Err(error) => results.push(json!({
                "identityId": identity_id,
                "ok": false,
                "error": error.to_string()
            })),
        }
    }
    if results.iter().any(|item| !bool_field(item, "ok")) {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            json!(results).to_string(),
        ));
    }
    Ok(results)
}

pub(crate) fn restore_desired_runtimes() {
    if env::var("MORNEVEN_CHILD_RUNTIME").ok().as_deref() == Some("1") {
        return;
    }
    let desired = load_desired_state();
    for runtime in runtime_entries() {
        let Some(identity_id) = string_field(&runtime, "identityId") else {
            continue;
        };
        let desired_state = desired
            .runtimes
            .get(identity_id)
            .map(|entry| entry.state.as_str())
            .unwrap_or(desired.global.as_str());
        if desired_state == "running" {
            let (running, _, _) = runtime_process_snapshot(identity_id);
            if !running {
                let _ = spawn_gateway_process(&runtime);
            }
        }
    }
}

fn runtime_uptime_seconds(started_at: Option<&String>) -> Option<i64> {
    let started_at = started_at?;
    let started_at = DateTime::parse_from_rfc3339(started_at).ok()?;
    Some(
        (Utc::now() - started_at.with_timezone(&Utc))
            .num_seconds()
            .max(0),
    )
}

fn runtime_status(runtime: &Value, desired: &DesiredGatewayState) -> Value {
    let identity_id = string_field(runtime, "identityId").unwrap_or_default();
    let desired_runtime = desired.runtimes.get(identity_id);
    let desired_state = desired_runtime
        .map(|entry| entry.state.as_str())
        .unwrap_or("stopped");
    let (process_running, pid, last_exit_code) = runtime_process_snapshot(identity_id);
    let state = if process_running {
        "running"
    } else {
        "stopped"
    };
    let last_log_line = read_recent_file_lines(&runtime_log_path(runtime), 1)
        .first()
        .cloned()
        .or_else(|| read_recent_logs(1).first().cloned());
    let last_error = if process_running {
        Value::Null
    } else {
        last_log_line
            .as_ref()
            .map(|line| json!(line))
            .unwrap_or(Value::Null)
    };
    let started_at = desired_runtime.and_then(|entry| entry.started_at.clone());
    let started_at = if process_running && started_at.is_none() {
        mark_runtime_started(identity_id, false).ok()
    } else {
        started_at
    };
    let uptime = if process_running {
        runtime_uptime_seconds(started_at.as_ref())
    } else {
        None
    };
    json!({
        "state": state,
        "identityId": identity_id,
        "name": string_field(runtime, "name").unwrap_or_default(),
        "pid": pid,
        "uptime": uptime,
        "startedAt": started_at,
        "restart_count": desired_runtime.map(|entry| entry.restart_count).unwrap_or(0),
        "gatewayPort": runtime.get("gatewayPort").cloned().unwrap_or(Value::Null),
        "telegramBotUsername": runtime.get("telegramBotUsername").cloned().unwrap_or(Value::Null),
        "telegramTokenFingerprint": runtime.get("telegramTokenFingerprint").cloned().unwrap_or(Value::Null),
        "lastError": last_error,
        "lastExitCode": last_exit_code,
        "desiredState": desired_state,
        "autoRestartEnabled": true,
        "lastUnplannedExitAt": null,
        "lastRestartAt": desired_runtime.and_then(|entry| entry.last_action_at.clone()),
        "lastLogLine": last_log_line,
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
        .map(|items| {
            items
                .iter()
                .map(|runtime| runtime_status(runtime, &desired))
                .collect()
        })
        .unwrap_or_default();
    let main = runtimes
        .iter()
        .find(|runtime| bool_field(runtime, "isMain"))
        .cloned()
        .or_else(|| runtimes.first().cloned());
    let running_count = runtimes
        .iter()
        .filter(|runtime| string_field(runtime, "state") == Some("running"))
        .count();
    let stopped_count = runtimes.len().saturating_sub(running_count);
    let aggregate_state = if running_count > 0 {
        "running"
    } else {
        "stopped"
    };
    json!({
        "state": aggregate_state,
        "running": running_count,
        "stopped": stopped_count,
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
    let content = String::from_utf8(raw)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "File is not valid UTF-8"))?;
    Ok((content, stat))
}

fn workspace_changes_at(workspace_root: &Path, manifest_path: &Path, include_all: bool) -> Value {
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
                    let base_hash = manifest
                        .files
                        .get(&relative_path)
                        .map(|file| file.content_hash.clone());
                    if !include_all && base_hash.as_deref() == Some(hash.as_str()) {
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

fn u64_field_any(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        let raw = value.get(*key)?;
        raw.as_u64()
            .or_else(|| raw.as_i64().and_then(|number| u64::try_from(number).ok()))
            .or_else(|| {
                raw.as_f64()
                    .filter(|number| number.is_finite() && *number >= 0.0)
                    .map(|number| number as u64)
            })
            .or_else(|| {
                raw.as_str()
                    .and_then(|text| text.trim().parse::<u64>().ok())
            })
    })
}

fn f64_field_any(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        let raw = value.get(*key)?;
        raw.as_f64()
            .or_else(|| raw.as_i64().map(|number| number as f64))
            .or_else(|| raw.as_u64().map(|number| number as f64))
            .or_else(|| {
                raw.as_str()
                    .and_then(|text| text.trim().parse::<f64>().ok())
            })
            .filter(|number| number.is_finite() && *number >= 0.0)
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

fn normalized_usage_event(record: &Value, runtime: Option<&Value>) -> Option<Value> {
    let usage = record
        .get("usage")
        .filter(|value| value.is_object())
        .unwrap_or(record);
    let recorded_at = string_field(record, "recordedAt")
        .or_else(|| string_field(record, "recorded_at"))
        .or_else(|| string_field(record, "timestamp"))
        .or_else(|| string_field(usage, "timestamp"))?;
    let provider = string_field(record, "provider")
        .or_else(|| string_field(record, "modelProvider"))
        .or_else(|| string_field(record, "model_provider"))
        .or_else(|| runtime.and_then(|runtime| string_field(runtime, "provider")))?;
    let model = string_field(record, "model")
        .or_else(|| string_field(record, "modelId"))
        .or_else(|| string_field(record, "model_id"))
        .or_else(|| string_field(usage, "model"))
        .unwrap_or_default();
    let prompt_tokens = u64_field_any(
        record,
        &[
            "promptTokens",
            "prompt_tokens",
            "inputTokens",
            "input_tokens",
        ],
    )
    .or_else(|| {
        u64_field_any(
            usage,
            &[
                "promptTokens",
                "prompt_tokens",
                "inputTokens",
                "input_tokens",
            ],
        )
    })
    .unwrap_or(0);
    let completion_tokens = u64_field_any(
        record,
        &[
            "completionTokens",
            "completion_tokens",
            "outputTokens",
            "output_tokens",
        ],
    )
    .or_else(|| {
        u64_field_any(
            usage,
            &[
                "completionTokens",
                "completion_tokens",
                "outputTokens",
                "output_tokens",
            ],
        )
    })
    .unwrap_or(0);
    let cached_tokens = u64_field_any(
        record,
        &[
            "cachedTokens",
            "cached_tokens",
            "cachedInputTokens",
            "cached_input_tokens",
            "prompt_cache_hit_tokens",
        ],
    )
    .or_else(|| {
        u64_field_any(
            usage,
            &[
                "cachedTokens",
                "cached_tokens",
                "cachedInputTokens",
                "cached_input_tokens",
                "prompt_cache_hit_tokens",
            ],
        )
    })
    .unwrap_or(0);
    let total_tokens = u64_field_any(
        record,
        &["totalTokens", "total_tokens", "tokensUsed", "tokens_used"],
    )
    .or_else(|| {
        u64_field_any(
            usage,
            &["totalTokens", "total_tokens", "tokensUsed", "tokens_used"],
        )
    })
    .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));
    let cost_usd = f64_field_any(
        record,
        &[
            "costUsd",
            "cost_usd",
            "cost",
            "total_cost",
            "estimated_cost",
        ],
    )
    .or_else(|| {
        f64_field_any(
            usage,
            &[
                "costUsd",
                "cost_usd",
                "cost",
                "total_cost",
                "estimated_cost",
            ],
        )
    })
    .unwrap_or(0.0);
    let runtime_id = string_field(record, "runtimeId")
        .or_else(|| string_field(record, "runtime_id"))
        .or_else(|| string_field(record, "identityId"))
        .or_else(|| runtime.and_then(|runtime| string_field(runtime, "identityId")))
        .unwrap_or_default();
    let runtime_name = string_field(record, "runtimeName")
        .or_else(|| string_field(record, "runtime_name"))
        .or_else(|| runtime.and_then(|runtime| string_field(runtime, "name")))
        .or_else(|| runtime.and_then(|runtime| string_field(runtime, "slug")))
        .unwrap_or_default();
    let session_key = string_field(record, "sessionKey")
        .or_else(|| string_field(record, "session_key"))
        .or_else(|| string_field(record, "session_id"));
    let event_id = string_field(record, "eventId")
        .or_else(|| string_field(record, "event_id"))
        .or_else(|| string_field(record, "id"))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            let stable = format!(
                "{runtime_id}:{provider}:{model}:{recorded_at}:{prompt_tokens}:{completion_tokens}:{total_tokens}"
            );
            content_hash(stable.as_bytes())
        });
    let request_count = u64_field_any(record, &["requestCount", "request_count"])
        .unwrap_or(1)
        .max(1);

    let mut raw_usage = Map::new();
    raw_usage.insert("source".to_string(), json!("zeroclaw"));
    raw_usage.insert("model".to_string(), json!(model));
    raw_usage.insert("input_tokens".to_string(), json!(prompt_tokens));
    raw_usage.insert("output_tokens".to_string(), json!(completion_tokens));
    raw_usage.insert("cached_input_tokens".to_string(), json!(cached_tokens));
    raw_usage.insert("prompt_cache_hit_tokens".to_string(), json!(cached_tokens));
    raw_usage.insert("total_tokens".to_string(), json!(total_tokens));
    raw_usage.insert("cost_usd".to_string(), json!(cost_usd));
    if cost_usd > 0.0 {
        raw_usage.insert("cost".to_string(), json!(cost_usd));
    }

    Some(json!({
        "eventId": event_id,
        "provider": provider,
        "runtimeId": runtime_id,
        "identityId": runtime_id,
        "runtimeName": runtime_name,
        "model": model,
        "modelId": model,
        "sessionKey": session_key,
        "recordedAt": recorded_at,
        "promptTokens": prompt_tokens,
        "completionTokens": completion_tokens,
        "totalTokens": total_tokens,
        "cachedTokens": cached_tokens,
        "requestCount": request_count,
        "stopReason": string_field(record, "stopReason").or_else(|| string_field(record, "stop_reason")),
        "error": string_field(record, "error"),
        "usage": Value::Object(raw_usage)
    }))
}

fn load_usage_events(
    path: &Path,
    runtime: Option<&Value>,
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
        .filter_map(|event| normalized_usage_event(&event, runtime))
        .filter(|event| usage_event_in_range(event, start, end))
        .collect()
}

fn runtime_usage_paths(runtime: &Value) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for key in ["usageEventsPath", "legacyUsageEventsPath"] {
        if let Some(path) = string_field(runtime, key) {
            push_usage_path(&mut paths, &mut seen, PathBuf::from(path));
        }
    }
    if let Some(runtime_path) =
        string_field(runtime, "runtimePath").filter(|value| !value.is_empty())
    {
        let runtime_path = PathBuf::from(runtime_path);
        push_usage_path(
            &mut paths,
            &mut seen,
            runtime_path.join("data").join("state").join("costs.jsonl"),
        );
        push_usage_path(
            &mut paths,
            &mut seen,
            runtime_path.join("state").join("costs.jsonl"),
        );
        push_usage_path(
            &mut paths,
            &mut seen,
            runtime_path.join("provider-usage.jsonl"),
        );
    }
    if let Some(workspace_path) =
        string_field(runtime, "workspacePath").filter(|value| !value.is_empty())
    {
        push_usage_path(
            &mut paths,
            &mut seen,
            PathBuf::from(workspace_path)
                .join("state")
                .join("costs.jsonl"),
        );
    }
    paths
}

fn push_usage_path(paths: &mut Vec<PathBuf>, seen: &mut HashSet<String>, path: PathBuf) {
    let key = path.to_string_lossy().to_string();
    if seen.insert(key) {
        paths.push(path);
    }
}

pub async fn handle_status(headers: HeaderMap) -> Response {
    if let Err(error) = require_morneven_token(&headers) {
        return error;
    }
    restore_desired_runtimes();
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
                    if let Err(error) = apply_gateway_process_action("restart") {
                        return response(
                            StatusCode::BAD_GATEWAY,
                            json!({"ok": false, "error": error.to_string()}),
                        );
                    }
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
        Err(error) => response(
            StatusCode::BAD_GATEWAY,
            json!({"ok": false, "error": error}),
        ),
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
        .map(|items| {
            items
                .iter()
                .map(config_secret_payload_for_runtime)
                .collect()
        })
        .unwrap_or_default();
    response(StatusCode::OK, json!({"ok": true, "runtimes": runtimes}))
}

pub async fn handle_workspace_changes(
    headers: HeaderMap,
    Query(query): Query<WorkspaceChangesQuery>,
) -> Response {
    if let Err(error) = require_morneven_token(&headers) {
        return error;
    }
    let include_all = query
        .include_all
        .as_deref()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "all"
            )
        })
        .unwrap_or(false);
    let state = load_runtime_state();
    let runtimes: Vec<Value> = state
        .get("runtimes")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|runtime| {
                    let workspace =
                        PathBuf::from(string_field(runtime, "workspacePath").unwrap_or_default());
                    let manifest =
                        PathBuf::from(string_field(runtime, "runtimePath").unwrap_or_default())
                            .join(".morneven-runtime-manifest.json");
                    let changes = workspace_changes_at(&workspace, &manifest, include_all);
                    let mut payload = value_object(&changes).cloned().unwrap_or_default();
                    payload.insert(
                        "identityId".to_string(),
                        json!(string_field(runtime, "identityId").unwrap_or_default()),
                    );
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
        for path in runtime_usage_paths(runtime) {
            events.extend(load_usage_events(
                &path,
                Some(runtime),
                start.as_ref(),
                end.as_ref(),
            ));
        }
    }
    if events.is_empty() {
        events.extend(load_usage_events(
            &root_usage_path(),
            None,
            start.as_ref(),
            end.as_ref(),
        ));
    }
    events.sort_by(|left, right| {
        let left_time = parse_iso_datetime(string_field(left, "recordedAt"));
        let right_time = parse_iso_datetime(string_field(right, "recordedAt"));
        left_time.cmp(&right_time)
    });
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

pub async fn handle_gateway_action(
    headers: HeaderMap,
    AxumPath(action): AxumPath<String>,
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
    match apply_gateway_process_action(&action).and_then(|_| set_runtime_action(None, &action)) {
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
    let Some(runtime) = runtime_by_identity(&identity_id) else {
        return response(
            StatusCode::NOT_FOUND,
            json!({"ok": false, "error": "Runtime identity was not found"}),
        );
    };
    match apply_runtime_process_action(&runtime, &action)
        .and_then(|_| set_runtime_action(Some(&identity_id), &action))
    {
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

    #[test]
    fn morneven_provider_translates_to_zeroclaw_toml() {
        let entry = json!({
            "credentials": {
                "deepseek": {
                    "apiKey": "sk-test",
                    "modelId": "deepseek-chat",
                    "apiBase": "https://api.deepseek.com"
                }
            }
        });
        let (reference, toml) =
            append_provider_toml_to_string(&entry, "deepseek", "default").unwrap();

        assert_eq!(reference, "deepseek.default");
        assert!(toml.contains("[providers.models.deepseek.default]"));
        assert!(toml.contains("api_key = \"sk-test\""));
        assert!(toml.contains("model = \"deepseek-chat\""));
        assert!(toml.contains("uri = \"https://api.deepseek.com\""));
    }

    #[test]
    fn morneven_telegram_channel_translates_to_zeroclaw_toml() {
        let entry = json!({
            "channels": {
                "telegram": {
                    "enabled": true,
                    "token": "123:ABC"
                }
            }
        });
        let (reference, toml) = append_telegram_toml_to_string(&entry, "default").unwrap();

        assert_eq!(reference, "telegram.default");
        assert!(toml.contains("[channels.telegram.default]"));
        assert!(toml.contains("enabled = true"));
        assert!(toml.contains("bot_token = \"123:ABC\""));
        assert!(toml.contains("mention_only = true"));
        assert!(toml.contains("ack_reactions = true"));
    }

    #[test]
    fn morneven_telegram_allow_from_translates_to_peer_group() {
        let entry = json!({
            "channels": {
                "telegram": {
                    "enabled": true,
                    "token": "123:ABC",
                    "allowFrom": ["6606508025", "@alice", "6606508025"]
                }
            }
        });
        let (_, toml) = append_telegram_toml_to_string(&entry, "default").unwrap();

        assert!(toml.contains("[peer_groups.telegram_default]"));
        assert!(toml.contains("channel = \"telegram.default\""));
        assert!(toml.contains("external_peers = [\"6606508025\", \"alice\"]"));
    }

    #[test]
    fn morneven_telegram_nested_channel_token_translates_to_zeroclaw_toml() {
        let entry = json!({
            "channels": {
                "telegram": {
                    "enabled": true,
                    "default": {
                        "bot_token": "123:ABC"
                    }
                }
            }
        });
        let (reference, toml) = append_telegram_toml_to_string(&entry, "default").unwrap();

        assert_eq!(reference, "telegram.default");
        assert!(toml.contains("[channels.telegram.default]"));
        assert!(toml.contains("bot_token = \"123:ABC\""));
    }

    #[test]
    fn morneven_telegram_channel_without_token_is_not_emitted() {
        let entry = json!({
            "channels": {
                "telegram": {
                    "enabled": true,
                    "topicRegistry": {
                        "groups": []
                    },
                    "topicLock": {
                        "enabled": true
                    }
                }
            }
        });

        assert!(append_telegram_toml_to_string(&entry, "default").is_none());
    }

    #[test]
    fn morneven_runtime_toml_uses_current_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let workspace_path = dir.path().join("workspace");
        let entry = json!({
            "identity": {
                "slug": "sora"
            },
            "credentials": {
                "deepseek": {
                    "apiKey": "sk-test",
                    "modelId": "deepseek-chat"
                }
            }
        });

        write_zeroclaw_toml_config(&config_path, &entry, &workspace_path, 18080).unwrap();
        let toml = std::fs::read_to_string(config_path).unwrap();

        assert!(toml.starts_with(&format!(
            "schema_version = {}\n",
            zeroclaw_config::migration::CURRENT_SCHEMA_VERSION
        )));
    }

    #[test]
    fn morneven_runtime_toml_auto_approves_runtime_tools() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let workspace_path = dir.path().join("workspace");
        let entry = json!({
            "identity": {
                "slug": "sora"
            },
            "credentials": {
                "deepseek": {
                    "apiKey": "sk-test",
                    "modelId": "deepseek-chat"
                }
            }
        });

        write_zeroclaw_toml_config(&config_path, &entry, &workspace_path, 18080).unwrap();
        let toml = std::fs::read_to_string(config_path).unwrap();

        assert!(toml.contains("[risk_profiles.default]"));
        assert!(toml.contains("level = \"full\""));
        assert!(toml.contains("require_approval_for_medium_risk = false"));
        assert!(toml.contains("auto_approve = [\"*\"]"));
        assert!(toml.contains("always_ask = []"));
        assert!(!toml.contains("allowed_tools = [\"*\"]"));
        assert!(toml.contains("[runtime]"));
        assert!(toml.contains("reasoning_enabled = false"));
    }

    #[test]
    fn morneven_runtime_toml_includes_zeroclaw_cron_and_reasoning_guard() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let workspace_path = dir.path().join("workspace");
        let entry = json!({
            "identity": {
                "slug": "sora"
            },
            "credentials": {
                "deepseek": {
                    "apiKey": "sk-test",
                    "modelId": "deepseek-chat"
                }
            },
            "zeroclaw": {
                "cron": {
                    "jobs": [{
                        "id": "daily-dream",
                        "name": "Daily Dream",
                        "jobType": "agent",
                        "enabled": true,
                        "prompt": "Dream now",
                        "schedule": {
                            "kind": "cron",
                            "expr": "0 9 * * *",
                            "tz": "Asia/Singapore"
                        },
                        "delivery": {
                            "mode": "announce",
                            "channel": "telegram",
                            "to": "-100"
                        }
                    }]
                }
            }
        });

        write_zeroclaw_toml_config(&config_path, &entry, &workspace_path, 18080).unwrap();
        let toml = std::fs::read_to_string(config_path).unwrap();

        assert!(toml.contains("[cron.daily-dream]"));
        assert!(toml.contains("job_type = \"agent\""));
        assert!(toml.contains("prompt = \"Dream now\""));
        assert!(toml.contains("[cron.daily-dream.schedule]"));
        assert!(toml.contains("expr = \"0 9 * * *\""));
        assert!(toml.contains("[cron.daily-dream.delivery]"));
        assert!(toml.contains("cron_jobs = [\"daily-dream\"]"));
        assert!(toml.contains("[runtime_profiles.default.thinking]"));
        assert!(toml.contains("default_level = \"off\""));
        assert!(toml.contains("native_thinking = false"));
        assert!(toml.contains("reasoning_enabled = false"));
    }

    #[test]
    fn morneven_materializer_prefers_zeroclaw_canonical_files() {
        let entry = json!({
            "files": [{
                "path": "agents.md",
                "content": "legacy agents"
            }],
            "zeroclaw": {
                "canonicalFiles": [{
                    "path": "AGENTS.md",
                    "content": "canonical agents"
                }, {
                    "path": "MEMORY.md",
                    "content": "memory"
                }]
            }
        });

        let files = morneven_translated_files(&entry);

        assert_eq!(files.len(), 2);
        assert_eq!(string_field(&files[0], "path"), Some("AGENTS.md"));
        assert_eq!(string_field(&files[0], "content"), Some("canonical agents"));
    }

    #[test]
    fn morneven_materializer_keeps_legacy_nanobot_over_generated_defaults() {
        let legacy = vec![json!({
            "path": "AGENTS.md",
            "content": "legacy nanobot agents",
            "objectPath": "legacy-nanobot://AGENTS.md"
        })];
        let generated = vec![json!({
            "id": "zeroclaw-managed-agents.md",
            "path": "AGENTS.md",
            "content": "generated default agents",
            "objectPath": "zeroclaw-managed://sora/AGENTS.md"
        })];

        let files = merge_runtime_materialization_files(legacy, generated);

        assert_eq!(files.len(), 1);
        assert_eq!(string_field(&files[0], "path"), Some("AGENTS.md"));
        assert_eq!(
            string_field(&files[0], "content"),
            Some("legacy nanobot agents")
        );
    }

    #[test]
    fn morneven_materializer_prefers_explicit_bundle_over_legacy_nanobot() {
        let legacy = vec![json!({
            "path": "AGENTS.md",
            "content": "legacy nanobot agents",
            "objectPath": "legacy-nanobot://AGENTS.md"
        })];
        let explicit = vec![json!({
            "id": "bot-manager-file",
            "path": "AGENTS.md",
            "content": "bot manager agents",
            "objectPath": "bot-manager/workspace/sora/AGENTS.md"
        })];

        let files = merge_runtime_materialization_files(legacy, explicit);

        assert_eq!(files.len(), 1);
        assert_eq!(string_field(&files[0], "path"), Some("AGENTS.md"));
        assert_eq!(
            string_field(&files[0], "content"),
            Some("bot manager agents")
        );
    }

    #[test]
    fn morneven_runtime_files_include_managed_policy() {
        let entry = json!({
            "identity": {
                "slug": "sora",
                "name": "Sora",
                "roleTitle": "Chat Friend and Assistant"
            },
            "zeroclaw": {
                "runtimePolicy": {
                    "globalRules": "Always follow Bot Manager global rules.",
                    "generalInformation": "Morneven context."
                },
                "canonicalFiles": []
            }
        });
        let identity = entry.get("identity").unwrap().clone();
        let files = runtime_files_for_materialization(&entry, &identity, &json!({}));
        let policy = files
            .iter()
            .find(|file| string_field(file, "path") == Some("MORNEVEN_POLICY.md"))
            .expect("managed policy file should be materialized");
        let content = string_field(policy, "content").unwrap_or_default();

        assert!(content.contains("Always follow Bot Manager global rules."));
        assert!(content.contains("Morneven context."));
        assert!(content.contains("Never expose hidden reasoning"));
        let persona = files
            .iter()
            .find(|file| string_field(file, "path") == Some("MORNEVEN_PERSONA.md"))
            .expect("managed persona lock file should be materialized");
        let persona_content = string_field(persona, "content").unwrap_or_default();

        assert!(persona_content.contains("Always respond as the active Morneven personality"));
        assert!(persona_content.contains("- Name: Sora"));
        assert!(persona_content.contains("- Role: Chat Friend and Assistant"));
        assert!(persona_content.contains("Use Indonesian by default"));
    }

    #[test]
    fn morneven_runtime_files_include_cron_summary() {
        let entry = json!({
            "identity": {
                "slug": "sola"
            },
            "zeroclaw": {
                "cron": {
                    "jobs": [{
                        "id": "usd-idr-siang",
                        "name": "usd-idr-siang",
                        "enabled": true,
                        "jobType": "agent",
                        "prompt": "Fetch current USD/IDR exchange rate.",
                        "sourcePath": "cron/jobs.json",
                        "schedule": {
                            "kind": "cron",
                            "expr": "0 12 * * *",
                            "tz": "Asia/Makassar"
                        },
                        "delivery": {
                            "mode": "announce",
                            "channel": "telegram.default",
                            "to": "-1003602779585",
                            "threadId": "6151"
                        }
                    }]
                },
                "canonicalFiles": []
            }
        });
        let identity = entry.get("identity").unwrap().clone();
        let files = runtime_files_for_materialization(&entry, &identity, &json!({}));
        let cron = files
            .iter()
            .find(|file| string_field(file, "path") == Some("MORNEVEN_CRON.md"))
            .expect("managed cron summary should be materialized");
        let content = string_field(cron, "content").unwrap_or_default();

        assert!(content.contains("usd-idr-siang"));
        assert!(content.contains("0 12 * * * (Asia/Makassar)"));
        assert!(content.contains("telegram.default to -1003602779585 thread 6151"));
        assert!(content.contains("Fetch current USD/IDR exchange rate."));
    }

    #[test]
    fn morneven_topic_state_keeps_lock_rules_with_registry() {
        let entry = json!({
            "channels": {
                "telegram": {
                    "enabled": true,
                    "topicRegistry": {
                        "groups": [{
                            "chatId": "-100",
                            "topics": []
                        }]
                    },
                    "topicLock": {
                        "enabled": true,
                        "groups": [{
                            "chatId": "-100",
                            "allowedTopicIds": ["159"],
                            "allowMainTopic": false,
                            "primaryTopicId": "159"
                        }]
                    }
                }
            }
        });
        let state = topic_registry_from_entry(&entry);

        assert_eq!(
            state
                .get("topicLock")
                .and_then(|lock| lock.get("enabled"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            state.get("groups").and_then(Value::as_array).map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn morneven_runtime_files_include_telegram_topic_summary() {
        let entry = json!({
            "identity": {
                "slug": "sola"
            },
            "channels": {
                "telegram": {
                    "enabled": true,
                    "topicRegistry": {
                        "groups": [{
                            "chatId": "-1003950002621",
                            "title": "Morneven Playground",
                            "isForum": true,
                            "topics": [{
                                "messageThreadId": "2",
                                "title": "Bot",
                                "source": "observed"
                            }]
                        }]
                    },
                    "topicLock": {
                        "enabled": true,
                        "groups": [{
                            "chatId": "-1003950002621",
                            "allowedTopicIds": ["2"],
                            "allowMainTopic": false,
                            "primaryTopicId": "2"
                        }]
                    }
                }
            },
            "zeroclaw": {
                "canonicalFiles": []
            }
        });
        let identity = entry.get("identity").unwrap().clone();
        let files = runtime_files_for_materialization(&entry, &identity, &json!({}));
        let topics = files
            .iter()
            .find(|file| string_field(file, "path") == Some("MORNEVEN_TELEGRAM_TOPICS.md"))
            .expect("managed Telegram topic summary should be materialized");
        let content = string_field(topics, "content").unwrap_or_default();

        assert!(content.contains("Morneven Playground"));
        assert!(content.contains("Chat ID: `-1003950002621`"));
        assert!(content.contains("| `2` | Bot | true | true | observed |"));
        assert!(content.contains("Main topic allowed: false"));
    }

    #[test]
    fn morneven_runtime_usage_paths_include_current_and_legacy_cost_files() {
        let runtime_dir = PathBuf::from("runtime-dir");
        let workspace_dir = runtime_dir.join("workspace");
        let current_costs = runtime_dir.join("data").join("state").join("costs.jsonl");
        let runtime_state_costs = runtime_dir.join("state").join("costs.jsonl");
        let legacy_usage = runtime_dir.join("provider-usage.jsonl");
        let workspace_costs = workspace_dir.join("state").join("costs.jsonl");
        let runtime = json!({
            "runtimePath": runtime_dir.to_string_lossy().to_string(),
            "workspacePath": workspace_dir.to_string_lossy().to_string(),
            "usageEventsPath": current_costs.to_string_lossy().to_string(),
            "legacyUsageEventsPath": legacy_usage.to_string_lossy().to_string()
        });

        let paths = runtime_usage_paths(&runtime);

        assert!(paths.contains(&current_costs));
        assert!(paths.contains(&runtime_state_costs));
        assert!(paths.contains(&legacy_usage));
        assert!(paths.contains(&workspace_costs));
        assert_eq!(paths.len(), 4);
    }

    #[test]
    fn morneven_usage_event_normalizes_zeroclaw_cost_record() {
        let runtime = json!({
            "identityId": "sora-id",
            "name": "Sora",
            "provider": "deepseek"
        });
        let record = json!({
            "id": "cost-1",
            "session_id": "session-1",
            "usage": {
                "model": "deepseek-chat",
                "input_tokens": 100,
                "output_tokens": 40,
                "cached_input_tokens": 20,
                "total_tokens": 140,
                "cost_usd": 0.003,
                "timestamp": "2026-05-30T00:00:00Z"
            }
        });

        let event = normalized_usage_event(&record, Some(&runtime)).unwrap();

        assert_eq!(event["eventId"], "cost-1");
        assert_eq!(event["provider"], "deepseek");
        assert_eq!(event["runtimeId"], "sora-id");
        assert_eq!(event["runtimeName"], "Sora");
        assert_eq!(event["model"], "deepseek-chat");
        assert_eq!(event["sessionKey"], "session-1");
        assert_eq!(event["recordedAt"], "2026-05-30T00:00:00Z");
        assert_eq!(event["promptTokens"], 100);
        assert_eq!(event["completionTokens"], 40);
        assert_eq!(event["cachedTokens"], 20);
        assert_eq!(event["totalTokens"], 140);
        assert_eq!(event["usage"]["cost"], 0.003);
    }

    #[test]
    fn morneven_usage_event_omits_zero_cost_for_backend_estimation() {
        let runtime = json!({
            "identityId": "sora-id",
            "provider": "deepseek"
        });
        let record = json!({
            "id": "cost-1",
            "usage": {
                "model": "deepseek-chat",
                "input_tokens": 100,
                "output_tokens": 40,
                "total_tokens": 140,
                "cost_usd": 0,
                "timestamp": "2026-05-30T00:00:00Z"
            }
        });

        let event = normalized_usage_event(&record, Some(&runtime)).unwrap();

        assert!(event["usage"].get("cost").is_none());
        assert_eq!(event["usage"]["cost_usd"], 0.0);
    }
}

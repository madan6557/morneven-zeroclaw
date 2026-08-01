//! Agent-callable tool for sending an explicit message through a configured channel.
//!
//! Telegram forum topics use the existing channel recipient format
//! `chat_id:topic_id` (for example `-1003602779585:6151`). The Telegram channel
//! remains responsible for validating Morneven topic locks before delivery.

use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use zeroclaw_api::channel::{Channel, SendMessage};
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_config::policy::SecurityPolicy;

use crate::ask_user::ChannelMapHandle;

/// Sends a message through a channel registered after tool construction.
pub struct ChannelSendTool {
    security: Arc<SecurityPolicy>,
    channels: ChannelMapHandle,
}

impl ChannelSendTool {
    pub fn new(security: Arc<SecurityPolicy>, channels: ChannelMapHandle) -> Self {
        Self { security, channels }
    }

    fn resolve_channel(&self, requested: &str) -> Option<(String, Arc<dyn Channel>)> {
        let channels = self.channels.read();
        let requested = requested.trim();

        let candidates = [
            requested.to_string(),
            requested
                .split_once('.')
                .map(|(kind, _)| format!("{kind}.default"))
                .unwrap_or_else(|| format!("{requested}.default")),
            requested
                .split_once('.')
                .map(|(kind, _)| kind.to_string())
                .unwrap_or_default(),
        ];

        candidates
            .into_iter()
            .find_map(|key| channels.get(&key).map(|channel| (key, Arc::clone(channel))))
    }
}

#[async_trait]
impl Tool for ChannelSendTool {
    fn name(&self) -> &str {
        "channel_send"
    }

    fn description(&self) -> &str {
        "Send an explicit message through a configured messaging channel. For a Telegram forum topic, use `to` in the form `chat_id:topic_id` (for example `-1003602779585:6151`). Use this native tool instead of running a shell or Python script."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Configured channel key, such as telegram.default or telegram"
                },
                "to": {
                    "type": "string",
                    "description": "Recipient identifier. Telegram forum topics use chat_id:topic_id."
                },
                "body": {
                    "type": "string",
                    "description": "Message content to send"
                }
            },
            "required": ["channel", "to", "body"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked by the active security policy".to_string()),
            });
        }

        let channel_name = args
            .get("channel")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'channel' parameter"))?;
        let recipient = args
            .get("to")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'to' parameter"))?;
        let body = args
            .get("body")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'body' parameter"))?;

        let (resolved_name, channel) = self.resolve_channel(channel_name).ok_or_else(|| {
            let available = self
                .channels
                .read()
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!("Channel '{channel_name}' not found. Available: {available}")
        })?;

        channel
            .send(&SendMessage::new(body, recipient))
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "Failed to send message through '{resolved_name}' to '{recipient}': {error}"
                )
            })?;

        Ok(ToolResult {
            success: true,
            output: format!("Message sent to {resolved_name}:{recipient}"),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use zeroclaw_api::attribution::{Attributable, ChannelKind, Role};

    struct RecordingChannel {
        sent: Arc<Mutex<Vec<SendMessage>>>,
    }

    impl Attributable for RecordingChannel {
        fn role(&self) -> Role {
            Role::Channel(ChannelKind::Webhook)
        }

        fn alias(&self) -> &str {
            "test"
        }
    }

    #[async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            "telegram"
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(message.clone());
            Ok(())
        }

        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<zeroclaw_api::channel::ChannelMessage>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn forwards_telegram_topic_target_to_channel() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let channel: Arc<dyn Channel> = Arc::new(RecordingChannel {
            sent: Arc::clone(&sent),
        });
        let channels = Arc::new(parking_lot::RwLock::new(HashMap::from([(
            "telegram.default".to_string(),
            channel,
        )])));
        let tool = ChannelSendTool::new(Arc::new(SecurityPolicy::default()), channels);

        let result = tool
            .execute(json!({
                "channel": "telegram",
                "to": "-1003602779585:6151",
                "body": "Halo"
            }))
            .await
            .unwrap();

        assert!(result.success);
        let messages = sent.lock().unwrap();
        assert_eq!(messages[0].recipient, "-1003602779585:6151");
        assert_eq!(messages[0].content, "Halo");
    }
}

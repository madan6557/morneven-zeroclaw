use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use zeroclaw_api::attribution::{Attributable, Role};
use zeroclaw_api::channel::{Channel, ChannelMessage, SendMessage};
use zeroclaw_api::delivery_sanitizer::{sanitize_delivery_text, sanitize_delivery_text_partial};

pub struct SanitizingChannel {
    inner: Arc<dyn Channel>,
}

impl SanitizingChannel {
    pub fn wrap(inner: Arc<dyn Channel>) -> Arc<dyn Channel> {
        if delivery_sanitizer_enabled() {
            Arc::new(Self { inner })
        } else {
            inner
        }
    }

    fn sanitize_send_message(&self, message: &SendMessage) -> SendMessage {
        let sanitized = sanitize_delivery_text(&message.content);
        if sanitized.changed {
            log_sanitized_delivery(self.inner.name(), sanitized.blocked, message.content.len());
        }
        let mut next = message.clone();
        next.content = sanitized.text;
        next
    }

    fn sanitize_partial_message(&self, message: &SendMessage) -> SendMessage {
        let sanitized = sanitize_delivery_text_partial(&message.content);
        if sanitized.changed {
            log_sanitized_delivery(self.inner.name(), sanitized.blocked, message.content.len());
        }
        let mut next = message.clone();
        next.content = if sanitized.text.is_empty() && !message.content.trim().is_empty() {
            "...".to_string()
        } else {
            sanitized.text
        };
        next
    }

    fn sanitize_final_text(&self, text: &str) -> String {
        let sanitized = sanitize_delivery_text(text);
        if sanitized.changed {
            log_sanitized_delivery(self.inner.name(), sanitized.blocked, text.len());
        }
        sanitized.text
    }

    fn sanitize_partial_text(&self, text: &str) -> Option<String> {
        let sanitized = sanitize_delivery_text_partial(text);
        if sanitized.changed {
            log_sanitized_delivery(self.inner.name(), sanitized.blocked, text.len());
        }
        if sanitized.text.is_empty() && !text.trim().is_empty() {
            None
        } else {
            Some(sanitized.text)
        }
    }
}

fn delivery_sanitizer_enabled() -> bool {
    std::env::var("MORNEVEN_SANITIZE_DELIVERY")
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            !matches!(value.as_str(), "0" | "false" | "off" | "no")
        })
        .unwrap_or(true)
}

fn log_sanitized_delivery(channel: &str, blocked: bool, original_len: usize) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({
                "channel": channel,
                "blocked": blocked,
                "original_len": original_len,
            })),
        "delivery sanitizer removed internal output"
    );
}

impl Attributable for SanitizingChannel {
    fn role(&self) -> Role {
        self.inner.role()
    }

    fn alias(&self) -> &str {
        self.inner.alias()
    }
}

#[async_trait]
impl Channel for SanitizingChannel {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let message = self.sanitize_send_message(message);
        self.inner.send(&message).await
    }

    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        self.inner.listen(tx).await
    }

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        self.inner.start_typing(recipient).await
    }

    async fn stop_typing(&self, recipient: &str) -> anyhow::Result<()> {
        self.inner.stop_typing(recipient).await
    }

    fn supports_draft_updates(&self) -> bool {
        self.inner.supports_draft_updates()
    }

    fn self_handle(&self) -> Option<String> {
        self.inner.self_handle()
    }

    fn self_addressed_mention(&self) -> Option<String> {
        self.inner.self_addressed_mention()
    }

    fn drop_self_messages(&self, msg: &ChannelMessage) -> bool {
        self.inner.drop_self_messages(msg)
    }

    fn supports_multi_message_streaming(&self) -> bool {
        self.inner.supports_multi_message_streaming()
    }

    fn multi_message_delay_ms(&self) -> u64 {
        self.inner.multi_message_delay_ms()
    }

    async fn send_draft(&self, message: &SendMessage) -> anyhow::Result<Option<String>> {
        let message = self.sanitize_partial_message(message);
        self.inner.send_draft(&message).await
    }

    async fn update_draft(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let Some(text) = self.sanitize_partial_text(text) else {
            return Ok(());
        };
        self.inner.update_draft(recipient, message_id, &text).await
    }

    async fn update_draft_progress(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let Some(text) = self.sanitize_partial_text(text) else {
            return Ok(());
        };
        self.inner
            .update_draft_progress(recipient, message_id, &text)
            .await
    }

    async fn finalize_draft(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let text = self.sanitize_final_text(text);
        self.inner
            .finalize_draft(recipient, message_id, &text)
            .await
    }

    async fn cancel_draft(&self, recipient: &str, message_id: &str) -> anyhow::Result<()> {
        self.inner.cancel_draft(recipient, message_id).await
    }

    async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> anyhow::Result<()> {
        self.inner.add_reaction(channel_id, message_id, emoji).await
    }

    async fn remove_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> anyhow::Result<()> {
        self.inner
            .remove_reaction(channel_id, message_id, emoji)
            .await
    }

    async fn pin_message(&self, channel_id: &str, message_id: &str) -> anyhow::Result<()> {
        self.inner.pin_message(channel_id, message_id).await
    }

    async fn unpin_message(&self, channel_id: &str, message_id: &str) -> anyhow::Result<()> {
        self.inner.unpin_message(channel_id, message_id).await
    }

    async fn redact_message(
        &self,
        channel_id: &str,
        message_id: &str,
        reason: Option<String>,
    ) -> anyhow::Result<()> {
        self.inner
            .redact_message(channel_id, message_id, reason)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use zeroclaw_api::attribution::ChannelKind;
    use zeroclaw_api::delivery_sanitizer::BLOCKED_INTERNAL_OUTPUT_FALLBACK;

    #[derive(Default)]
    struct RecordingChannel {
        sent: Mutex<Vec<String>>,
        updates: Mutex<Vec<String>>,
    }

    impl Attributable for RecordingChannel {
        fn role(&self) -> Role {
            Role::Channel(ChannelKind::Cli)
        }

        fn alias(&self) -> &str {
            "default"
        }
    }

    #[async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            "cli"
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.lock().push(message.content.clone());
            Ok(())
        }

        async fn listen(&self, _tx: mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }

        async fn update_draft(
            &self,
            _recipient: &str,
            _message_id: &str,
            text: &str,
        ) -> anyhow::Result<()> {
            self.updates.lock().push(text.to_string());
            Ok(())
        }

        fn self_handle(&self) -> Option<String> {
            Some("@self".to_string())
        }

        fn supports_draft_updates(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn send_sanitizes_reasoning() {
        let inner = Arc::new(RecordingChannel::default());
        let wrapped = SanitizingChannel {
            inner: inner.clone(),
        };
        wrapped
            .send(&SendMessage::new(
                "The user is asking about rates.\n📊 USD/IDR - Pagi",
                "user",
            ))
            .await
            .unwrap();

        assert_eq!(inner.sent.lock()[0], "📊 USD/IDR - Pagi");
    }

    #[tokio::test]
    async fn update_draft_suppresses_partial_reasoning() {
        let inner = Arc::new(RecordingChannel::default());
        let wrapped = SanitizingChannel {
            inner: inner.clone(),
        };
        wrapped
            .update_draft("user", "draft", "The user is asking. Let me check.")
            .await
            .unwrap();

        assert!(inner.updates.lock().is_empty());
    }

    #[test]
    fn delegates_channel_capabilities() {
        let inner = Arc::new(RecordingChannel::default());
        let wrapped = SanitizingChannel { inner };
        assert_eq!(wrapped.name(), "cli");
        assert_eq!(wrapped.self_handle().as_deref(), Some("@self"));
        assert!(wrapped.supports_draft_updates());
    }

    #[test]
    fn fallback_constant_is_user_visible() {
        assert!(!BLOCKED_INTERNAL_OUTPUT_FALLBACK.is_empty());
    }
}

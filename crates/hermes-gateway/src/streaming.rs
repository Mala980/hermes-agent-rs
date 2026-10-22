//! Streaming message state for edit-in-place progressive updates.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::Mutex as AsyncMutex;
use tracing::debug;

use hermes_core::errors::GatewayError;
use hermes_core::traits::PlatformAdapter;

/// Mutable state shared between streaming flush tasks and finalization.
///
/// `route_streaming` sends a "..." placeholder, edits it in place on every
/// flush, and finalizes it when the stream completes, so a streaming reply
/// occupies exactly one chat message instead of one message per flush.
#[derive(Clone)]
pub(crate) struct StreamEditState {
    pub(crate) adapter: Arc<dyn PlatformAdapter>,
    pub(crate) chat_id: String,
    /// Forum topic/thread the stream belongs to (e.g. Telegram's
    /// `message_thread_id` as a string); used by the `send_message` fallbacks
    /// so a stream without a placeholder still lands in the right topic.
    pub(crate) thread_id: Option<String>,
    /// Platform id of the placeholder message (`None` when the adapter
    /// doesn't report message ids — flushes fall back to `send_message`).
    pub(crate) placeholder_id: Option<i64>,
    /// Text shown by the last successful edit; used to skip no-op edits.
    pub(crate) last_sent_text: Arc<StdMutex<String>>,
    /// Set once the stream is finalized; late chunk tasks must not edit
    /// after the final message state is written.
    pub(crate) finalized: Arc<AtomicBool>,
    /// Serializes placeholder edits so a slow flush can't land after the
    /// final edit and regress the visible message.
    pub(crate) edit_guard: Arc<AsyncMutex<()>>,
}

impl StreamEditState {
    /// Perform one streaming flush: edit the placeholder message in place.
    ///
    /// Skips the edit when `content` matches the last text sent (Telegram
    /// rejects identical edits with "message is not modified"). Falls back
    /// to `send_message` when the adapter did not report a placeholder
    /// message id. Edit failures are logged at debug and swallowed —
    /// streaming progress must never break the agent flow.
    pub(crate) async fn flush_edit(&self, content: &str) {
        let _guard = self.edit_guard.lock().await;
        if self.finalized.load(Ordering::SeqCst) {
            return;
        }
        let Some(message_id) = self.placeholder_id else {
            // Adapter doesn't report message ids: keep the legacy behavior
            // of sending the accumulated content as a new message.
            let _ = self
                .adapter
                .send_message_threaded(&self.chat_id, content, None, self.thread_id.as_deref())
                .await;
            *self.last_sent_text.lock().unwrap() = content.to_string();
            return;
        };
        if *self.last_sent_text.lock().unwrap() == content {
            return;
        }
        let mid = message_id.to_string();
        match self
            .adapter
            .edit_message(&self.chat_id, &mid, content)
            .await
        {
            Ok(()) => *self.last_sent_text.lock().unwrap() = content.to_string(),
            Err(e) if is_not_modified_error(&e) => {
                // The visible text already matches; treat as success.
                debug!("streaming edit skipped: message content unchanged");
                *self.last_sent_text.lock().unwrap() = content.to_string();
            }
            Err(e) => debug!("streaming edit failed: {e}"),
        }
    }

    /// Write the final stream state to the placeholder message.
    ///
    /// If the full response fits in a single message, the placeholder is
    /// edited in place (skipped when identical to the last flush). Longer
    /// responses can't be shown in one edited message, so the placeholder
    /// is deleted and the full text is sent instead (the adapter splits it
    /// as needed). An empty response just removes the placeholder.
    pub(crate) async fn finalize(&self, response: &str) -> Result<(), GatewayError> {
        // Mark finalized BEFORE acquiring the edit guard: a flush task that
        // already holds the guard finishes first, and any task that acquires
        // it afterwards sees the flag and skips — a stale flush can never
        // land after this final write.
        self.finalized.store(true, Ordering::SeqCst);
        let _guard = self.edit_guard.lock().await;

        if response.is_empty() {
            if let Some(pid) = self.placeholder_id {
                let _ = self.adapter.delete_message(&self.chat_id, pid).await;
            }
            return Ok(());
        }
        if *self.last_sent_text.lock().unwrap() == response {
            return Ok(());
        }
        if response.chars().count() <= self.adapter.max_edit_chars() {
            if let Some(pid) = self.placeholder_id {
                let mid = pid.to_string();
                match self
                    .adapter
                    .edit_message(&self.chat_id, &mid, response)
                    .await
                {
                    Ok(()) => {}
                    Err(e) if is_not_modified_error(&e) => {
                        debug!("final streaming edit skipped: message content unchanged");
                    }
                    Err(e) => debug!("final streaming edit failed: {e}"),
                }
                *self.last_sent_text.lock().unwrap() = response.to_string();
                return Ok(());
            }
        }
        // No placeholder id, or the response is too long for one message:
        // remove the placeholder (best-effort) and deliver the full text.
        if let Some(pid) = self.placeholder_id {
            let _ = self.adapter.delete_message(&self.chat_id, pid).await;
        }
        self.adapter
            .send_message_threaded(&self.chat_id, response, None, self.thread_id.as_deref())
            .await
    }

    /// Abort a stream: stop late flush edits and best-effort delete the
    /// placeholder message.
    pub(crate) async fn abort(&self) {
        self.finalized.store(true, Ordering::SeqCst);
        let _guard = self.edit_guard.lock().await;
        if let Some(pid) = self.placeholder_id {
            let _ = self.adapter.delete_message(&self.chat_id, pid).await;
        }
    }
}

/// Whether a gateway error from a message edit means the new text is
/// identical to the current text (Telegram: "message is not modified").
/// Callers should treat this as success.
pub(crate) fn is_not_modified_error(err: &GatewayError) -> bool {
    err.to_string()
        .to_lowercase()
        .contains("message is not modified")
}

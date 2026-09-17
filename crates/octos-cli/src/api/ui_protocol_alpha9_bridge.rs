//! Spawn-only background completion media coalescing (ex-α-9 bridge).
//!
//! The α-9 typed-envelope emitters (turn addenda, `file/attached`,
//! visual lifecycle, `voice/exit`) were retired with the
//! voice product line; the only surviving production helper coalesces the
//! dual media lists on `BackgroundResultPayload` for the canonical
//! background-child envelope.

use octos_agent::BackgroundResultPayload;

/// Resolve the effective envelope-media list for a spawn_only background
/// completion payload.
///
/// The `BackgroundResultPayload` shape carries TWO media lists by design
/// (see `BackgroundResultPayload::envelope_media` doc):
///
/// - `media` lands on the durable completion row. Populated by the contract
///   `Satisfied` path with `output_files`; left empty by the
///   `NotConfigured` `send_file` fallback because each delivered file
///   already has its own per-file durable companion row.
/// - `envelope_media` surfaces on the canonical background-child
///   envelope. Populated by the `NotConfigured` `send_file` fallback with
///   `sent_files`; left empty by the `Satisfied` path because `media`
///   already carries the list.
///
/// Every consumer that needs to render attachments must coalesce the two;
/// this helper centralises the `envelope_media`-over-`media` fallback so a
/// caller cannot silently drop half of the live spawn-only completion
/// shapes.
pub(super) fn effective_envelope_media(payload: &BackgroundResultPayload) -> Vec<String> {
    if payload.envelope_media.is_empty() {
        payload.media.clone()
    } else {
        payload.envelope_media.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_media_when_envelope_media_empty() {
        let payload = BackgroundResultPayload {
            media: vec!["output/deck.pptx".into()],
            envelope_media: vec![],
            ..Default::default()
        };
        assert_eq!(effective_envelope_media(&payload), vec!["output/deck.pptx"]);
    }

    #[test]
    fn prefers_envelope_media_when_present() {
        let payload = BackgroundResultPayload {
            media: vec![],
            envelope_media: vec!["sent/report.md".into()],
            ..Default::default()
        };
        assert_eq!(effective_envelope_media(&payload), vec!["sent/report.md"]);
    }
}

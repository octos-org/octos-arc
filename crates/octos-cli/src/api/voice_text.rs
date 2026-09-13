//! Pure text utilities for in-band rich-output directives and voice-style
//! reply splitting. Kept from the retired voice product line because the
//! WS transport's message pipeline still consumes the splitter/filter for
//! text streams, and `context_manager` mirrors the marker semantics.

use std::path::Path;

use octos_core::{Message, MessageRole};
use serde::Serialize;

// ── Voice-turn rich output (in-band `[[VISUAL:kind|brief]]` marker) ────────
//
// The fast turn may append a marker after the spoken reply when the model
// decides a visual would help. The backend parses it and dispatches: `html`
// goes to a focused tool-less LLM authoring call (octos-agent `rich_output`),
// the rest to mofa skills. The model never emits a tool call, sidestepping the
// Gemini-3 thought_signature 400.

/// Rich-output kind. `Html` goes to the focused LLM authoring call; the rest
/// go to mofa skills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VisualKind {
    Html,
    /// Realistic illustration embedded inside interactive HTML (two-stage:
    /// `mofa_image` generates a PNG → inlined into an `author_html` document).
    Illustrated,
    Image,
    Infographic,
}

impl VisualKind {
    fn from_token(s: &str) -> Option<Self> {
        match s.trim() {
            "html" => Some(Self::Html),
            "illustrated" => Some(Self::Illustrated),
            "image" => Some(Self::Image),
            "infographic" => Some(Self::Infographic),
            _ => None,
        }
    }

    /// Wire token for the `visual/generating` event (`kind` field).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Illustrated => "illustrated",
            Self::Image => "image",
            Self::Infographic => "infographic",
        }
    }
}

/// A parsed in-band rich-output directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisualDirective {
    pub kind: VisualKind,
    pub brief: String,
}

/// Parse a trailing `[[VISUAL:kind|brief]]` marker from the model reply.
/// Returns `None` (treat as a plain spoken reply) when absent, the kind is
/// unrecognized, the marker is malformed, or the brief is empty.
pub(crate) fn parse_visual_marker(reply: &str) -> Option<VisualDirective> {
    // The marker is appended AFTER the spoken reply, so only accept it when its
    // closing `]]` ends the (right-trimmed) reply. This stops a mid-reply
    // mention or quote of the `[[VISUAL:...]]` syntax from triggering an artifact.
    let trimmed = reply.trim_end();
    let start = trimmed.rfind("[[VISUAL:")?;
    let after = &trimmed[start + "[[VISUAL:".len()..];
    let end = after.find("]]")?;
    if start + "[[VISUAL:".len() + end + "]]".len() != trimmed.len() {
        return None; // not the trailing marker
    }
    let body = &after[..end];
    let (kind_tok, brief) = body.split_once('|')?;
    let kind = VisualKind::from_token(kind_tok)?;
    let brief = brief.trim().to_string();
    if brief.is_empty() {
        return None;
    }
    Some(VisualDirective { kind, brief })
}

/// The in-band visual marker opener. Streaming holds back only text that is —
/// or could still grow into — this exact prefix, **not** any `[[`, so ordinary
/// bracket notation (e.g. a `[[1]]` citation) never suppresses the rest of the
/// TTS.
const MARKER: &str = "[[VISUAL:";

/// The in-band exit-intent marker (UPCR-2026-025). A fixed, self-contained
/// trailing token the model appends after a farewell when the user wants to end
/// / leave / mute. Held back from TTS and the `message/delta` wire exactly like
/// [`MARKER`]; the actual exit decision is lifted off the final reply content
/// via [`parse_exit_marker`] / [`strip_exit_directive`].
const EXIT_MARKER: &str = "[[EXIT]]";

/// Every in-band control marker held back from TTS / the delta wire. All share
/// the `[[` opener; a trailing partial that could still grow into ANY of them is
/// held back, and a full occurrence of any marks the start of the held region.
/// Ordinary `[[…]]` notation (citations) matches none, so it is still spoken.
const CONTROL_MARKERS: &[&str] = &[MARKER, EXIT_MARKER];

/// Length in bytes of the longest suffix of `s` that is a non-empty prefix of
/// `marker`. Both control markers are ASCII, so any match is at a char boundary
/// (safe to slice).
fn single_marker_prefix_hold(s: &str, marker: &str) -> usize {
    let sb = s.as_bytes();
    let mb = marker.as_bytes();
    let max = mb.len().min(sb.len());
    (1..=max)
        .rev()
        .find(|&n| sb[sb.len() - n..] == mb[..n])
        .unwrap_or(0)
}

/// Length in bytes of the longest trailing partial that could still grow into
/// ANY control marker — those chars are held back from TTS because the next
/// streamed token might complete a marker.
fn marker_prefix_hold(s: &str) -> usize {
    CONTROL_MARKERS
        .iter()
        .map(|m| single_marker_prefix_hold(s, m))
        .max()
        .unwrap_or(0)
}

/// Earliest byte index in `s` of a fully-present control-marker opener (visual
/// or exit), or `None`. Everything from that index onward is held back.
fn find_control_marker(s: &str) -> Option<usize> {
    CONTROL_MARKERS.iter().filter_map(|m| s.find(m)).min()
}

/// Sentence chunking: strong boundaries (。！？!?…；;\n) always split; commas /
/// ideographic commas (soft) split once the segment is >=8 chars (faster first
/// audio). Returns the complete sentences; `buf` keeps the unfinished tail.
/// Module-level twin of the `ui_protocol` inline version, used by
/// [`VoiceReplySplitter`] and unit tests.
fn drain_sentences(buf: &mut String) -> Vec<String> {
    const STRONG: &[char] = &['。', '！', '？', '!', '?', '…', '；', ';', '\n'];
    const SOFT: &[char] = &['，', ',', '、'];
    const SOFT_MIN_CHARS: usize = 8;
    let mut out = Vec::new();
    loop {
        let mut cut = None;
        for (count, (i, c)) in buf.char_indices().enumerate() {
            if STRONG.contains(&c) || (SOFT.contains(&c) && count + 1 >= SOFT_MIN_CHARS) {
                cut = Some(i + c.len_utf8());
                break;
            }
        }
        match cut {
            Some(idx) => {
                let sentence = buf[..idx].trim().to_string();
                *buf = buf[idx..].to_string();
                if !sentence.is_empty() {
                    out.push(sentence);
                }
            }
            None => break,
        }
    }
    out
}

/// Streams speakable text apart from a trailing `[[VISUAL:...]]` marker. Once
/// `[[` appears, everything from there on is held back from TTS (it may be the
/// marker); at the end the directive is parsed from the accumulated full text.
pub(crate) struct VoiceReplySplitter {
    /// Text seen but not yet emitted to TTS. Speakable sentences are drained
    /// from its front each push; a (possibly partial) trailing marker is held
    /// back here until `finish` decides whether it was a real marker.
    pending: String,
    /// All text seen so far (used to parse the trailing marker on `finish`).
    full: String,
}

impl VoiceReplySplitter {
    pub(crate) fn new() -> Self {
        Self {
            pending: String::new(),
            full: String::new(),
        }
    }

    /// Feed a streamed token; returns the complete sentences ready for TTS now.
    pub(crate) fn push(&mut self, token: &str) -> Vec<String> {
        self.full.push_str(token);
        self.pending.push_str(token);

        if let Some(idx) = find_control_marker(&self.pending) {
            // A full control marker (`[[VISUAL:` or `[[EXIT]]`) appeared — treat
            // everything from it onward as the (trailing) marker region and hold
            // it back. Emit the speakable text before it; keep the unfinished
            // pre-marker tail + the held region in `pending` (recovered in
            // `finish` if it turns out not to be a real trailing marker).
            let mut head = self.pending[..idx].to_string();
            let rest = self.pending[idx..].to_string();
            let out = drain_sentences(&mut head);
            self.pending = head;
            self.pending.push_str(&rest);
            return out;
        }

        // No full marker yet: hold back only a trailing partial that could still
        // grow into `[[VISUAL:` (not arbitrary `[[`); drain the rest.
        let hold = marker_prefix_hold(&self.pending);
        let split = self.pending.len() - hold;
        let mut head = self.pending[..split].to_string();
        let held = self.pending[split..].to_string();
        let out = drain_sentences(&mut head);
        self.pending = head;
        self.pending.push_str(&held);
        out
    }

    /// End of stream: returns the remaining speakable tail + parsed directive.
    /// If no real trailing marker parsed, any held-back text is recovered as
    /// speech so nothing is lost to a mid-reply `[[` or stray partial.
    pub(crate) fn finish(self) -> (Option<String>, Option<VisualDirective>) {
        let directive = parse_visual_marker(&self.full);
        // Drop ALL real trailing control markers (visual and/or exit, in either
        // order) from the spoken tail so neither reaches TTS — a stacked
        // `…[[VISUAL:…]][[EXIT]]` must peel both. A held-back region that turned
        // out NOT to be a real trailing marker (a rare mid-reply `[[`) is left
        // intact and recovered as speech. The visual directive is returned for
        // dispatch; the exit decision is lifted separately off the final content
        // (`strip_control_directives`).
        let mut speak: &str = self.pending.as_str();
        loop {
            if parse_visual_marker(speak).is_some() {
                speak = strip_visual_marker(speak);
                continue;
            }
            if parse_exit_marker(speak) {
                speak = strip_exit_marker(speak);
                continue;
            }
            break;
        }
        let speak = speak.trim();
        let tail = if speak.is_empty() {
            None
        } else {
            Some(speak.to_string())
        };
        (tail, directive)
    }
}

/// Drop a trailing `[[VISUAL:...]]` marker, returning the speakable prefix. For
/// the **non-streamed fallback** (whole-reply synth) so the marker isn't read
/// aloud as text. Only a TRAILING marker is stripped (consistent with
/// [`parse_visual_marker`]); a mid-reply mention is left intact.
pub(crate) fn strip_visual_marker(reply: &str) -> &str {
    if parse_visual_marker(reply).is_some() {
        let i = reply.rfind("[[VISUAL:").expect("trailing marker present");
        reply[..i].trim_end()
    } else {
        reply
    }
}

/// Whether the model reply carries a TRAILING `[[EXIT]]` control marker
/// (UPCR-2026-025). Trailing-only: the marker must end the right-trimmed reply,
/// so a mid-reply mention / quote of the syntax never triggers an exit (mirrors
/// [`parse_visual_marker`]).
pub(crate) fn parse_exit_marker(reply: &str) -> bool {
    let trimmed = reply.trim_end();
    match trimmed.rfind(EXIT_MARKER) {
        Some(start) => start + EXIT_MARKER.len() == trimmed.len(),
        None => false,
    }
}

/// Drop a trailing `[[EXIT]]` marker, returning the speakable prefix. Only a
/// TRAILING marker is stripped (consistent with [`parse_exit_marker`]); a
/// mid-reply mention is left intact.
pub(crate) fn strip_exit_marker(reply: &str) -> &str {
    if parse_exit_marker(reply) {
        let i = reply
            .rfind(EXIT_MARKER)
            .expect("trailing exit marker present");
        reply[..i].trim_end()
    } else {
        reply
    }
}

/// Remove EVERY `[[VISUAL:...]]` span from `s` (not just a trailing one).
///
/// Unlike [`strip_visual_marker`] (which only drops a trailing directive and
/// preserves a mid-text mention), this scrubs the marker wherever it appears —
/// used for sanitizing free-form text that may have folded a marker in at an
/// arbitrary position, e.g. a pre-fix compaction summary. An unterminated
/// `[[VISUAL:` drops the remainder.
// Kept for sanitizing folded-in markers (e.g. legacy compaction summaries);
// no caller on the current turn path, but the scrubber stays available for
// the migration cleanups it was written for.
#[allow(dead_code)]
pub(crate) fn remove_all_visual_markers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("[[VISUAL:") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find("]]") {
            Some(end) => rest = &after[end + "]]".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Lift the trailing `[[VISUAL:...]]` directive out of the turn's authoritative
/// reply surfaces and strip it in place, so the internal control protocol never
/// reaches the WIRE (`message/delta` from `done`, `projection/envelope`) or
/// STORAGE (session JSONL). Strips both `content` (the final `response.content`)
/// and every Assistant carrier in `messages` that ends with the same trailing
/// marker. Returns the parsed directive for background dispatch, or `None` (and
/// leaves everything intact) when there is no real trailing marker. The frontend
/// learns a visual is coming from the typed `visual/generating` event instead.
pub(crate) fn strip_visual_directive(
    content: &mut String,
    messages: &mut [Message],
) -> Option<VisualDirective> {
    let directive = parse_visual_marker(content)?;
    *content = strip_visual_marker(content).to_string();
    for message in messages.iter_mut() {
        if message.role == MessageRole::Assistant {
            let stripped = strip_visual_marker(&message.content);
            if stripped.len() != message.content.len() {
                message.content = stripped.to_string();
            }
        }
    }
    Some(directive)
}

/// Lift the trailing `[[EXIT]]` control marker out of the turn's authoritative
/// reply surfaces and strip it in place, so the internal control protocol never
/// reaches the WIRE (`message/delta`, `projection/envelope`) or STORAGE (session
/// JSONL). Strips both `content` (the final `response.content`) and every
/// Assistant carrier in `messages` whose trailing marker matches. Returns `true`
/// when a real trailing marker was found and removed (the caller then emits the
/// typed `voice/exit` event), or `false` (leaving everything intact) otherwise.
/// Mirrors [`strip_visual_directive`]; the client learns to leave the voice
/// screen from the typed `voice/exit` event, not from the marker text.
pub(crate) fn strip_exit_directive(content: &mut String, messages: &mut [Message]) -> bool {
    if !parse_exit_marker(content) {
        return false;
    }
    *content = strip_exit_marker(content).to_string();
    for message in messages.iter_mut() {
        if message.role == MessageRole::Assistant {
            let stripped = strip_exit_marker(&message.content);
            if stripped.len() != message.content.len() {
                message.content = stripped.to_string();
            }
        }
    }
    true
}

/// Strip STACKED trailing control markers ([[VISUAL:...]] and/or [[EXIT]]) from
/// the turn's authoritative reply surfaces in EITHER order, returning the parsed
/// visual directive (if any) and whether an exit was requested.
///
/// A reply may end with both markers (e.g. `…[[VISUAL:…]][[EXIT]]`): stripping
/// only the outermost would leave the inner one trailing on the wire
/// (`message/delta`, `projection/envelope`) and in storage (session JSONL), and —
/// for the visual case — never dispatch it. So this loops, peeling whichever
/// marker is currently trailing, until neither is. Reuses (and supersedes on the
/// turn path) the per-marker [`strip_visual_directive`] / [`strip_exit_directive`].
pub(crate) fn strip_control_directives(
    content: &mut String,
    messages: &mut [Message],
) -> (Option<VisualDirective>, bool) {
    let mut directive = None;
    let mut exit = false;
    loop {
        if let Some(d) = strip_visual_directive(content, messages) {
            if directive.is_none() {
                directive = Some(d);
            }
            continue;
        }
        if strip_exit_directive(content, messages) {
            exit = true;
            continue;
        }
        break;
    }
    (directive, exit)
}

/// Byte length of the marker-free visible prefix of `full`: everything up to a
/// (possibly mid-reply) `[[VISUAL:` occurrence, else everything minus a trailing
/// partial that could still grow into the marker. Trailing whitespace before
/// that cut is also held back — the marker convention puts it on its own line,
/// so the preceding newline would otherwise leak as a blank line. Cut points
/// fall on char boundaries (`MARKER` is ASCII; `trim_end` is boundary-safe).
fn visible_prefix_len(full: &str) -> usize {
    let cut = match find_control_marker(full) {
        Some(i) => i,
        None => full.len() - marker_prefix_hold(full),
    };
    full[..cut].trim_end().len()
}

/// Token-granular twin of [`VoiceReplySplitter`] for the **UI message delta**
/// stream of a voice turn: emits the reply text token-by-token while holding
/// back any trailing (or still-forming) `[[VISUAL:...]]` marker, so the live
/// `message/delta` wire never carries the internal control protocol (the
/// durable surfaces are stripped separately by [`strip_visual_directive`]).
pub(crate) struct VisibleDeltaFilter {
    /// All text seen so far.
    full: String,
    /// Byte count already emitted as deltas (monotonic).
    emitted: usize,
}

impl VisibleDeltaFilter {
    pub(crate) fn new() -> Self {
        Self {
            full: String::new(),
            emitted: 0,
        }
    }

    /// Feed a streamed token; returns the newly-visible marker-free text to emit
    /// as a delta now (empty when the token only extended a held-back marker).
    pub(crate) fn push(&mut self, token: &str) -> String {
        self.full.push_str(token);
        let visible = visible_prefix_len(&self.full);
        if visible > self.emitted {
            let out = self.full[self.emitted..visible].to_string();
            self.emitted = visible;
            out
        } else {
            String::new()
        }
    }

    /// End of stream: returns any held-back text that turned out NOT to be a
    /// real trailing marker (a rare mid-reply `[[VISUAL:` / `[[EXIT]]` quote), so
    /// nothing is lost. A real trailing control marker (visual or exit) yields
    /// `""` for that span (stays off the wire).
    pub(crate) fn finish(self) -> String {
        // Peel ALL real trailing control markers (visual and/or exit, in either
        // order) so a stacked `…[[VISUAL:…]][[EXIT]]` never leaks the inner one
        // onto the `message/delta` wire.
        let mut clean: &str = self.full.as_str();
        loop {
            if parse_visual_marker(clean).is_some() {
                clean = strip_visual_marker(clean);
                continue;
            }
            if parse_exit_marker(clean) {
                clean = strip_exit_marker(clean);
                continue;
            }
            break;
        }
        let visible = clean.len();
        if visible > self.emitted {
            self.full[self.emitted..visible].to_string()
        } else {
            String::new()
        }
    }
}

/// Maps a `kind` to a mofa tool name + input args. `Html` is not a skill →
/// `None`.
fn image_skill_call(
    d: &VisualDirective,
    out_dir: &Path,
) -> Option<(&'static str, serde_json::Value)> {
    let out = out_dir.to_string_lossy().to_string();
    // A UNIQUE output filename per dispatch (#1477 follow-up): the mofa skill
    // caches by output path (`is_cached`: returns the existing file when it is
    // >10KB, skipping generation). A fixed name like `image.png` therefore made
    // every turn after the first in a session return the PREVIOUS turn's image
    // (and skip generation entirely, so reference frames were never applied).
    let uniq = uuid::Uuid::now_v7();
    match d.kind {
        VisualKind::Infographic => Some((
            "mofa_infographic",
            serde_json::json!({
                "sections": [{ "prompt": d.brief }],
                "out": format!("{out}/infographic-{uniq}.png"),
            }),
        )),
        // `mofa_image` (not `mofa_cards`): a plain "generate an image" request
        // wants a single picture, and — unlike `mofa_cards`, which emits no
        // `files_to_send` (octos #1041, see `workspace_policy` test) so the
        // backend would get empty rels and deliver nothing — `mofa_image`
        // reports its produced PNG via `files_to_send`, the same proven path
        // the Illustrated stage-1 call relies on.
        VisualKind::Image => Some((
            "mofa_image",
            serde_json::json!({
                "prompt": d.brief,
                "out": format!("{out}/image-{uniq}.png"),
            }),
        )),
        // Html (focused LLM call) and Illustrated (two-stage: run_illustration_image
        // then author_html) are not direct file-delivering skills.
        VisualKind::Html | VisualKind::Illustrated => None,
    }
}

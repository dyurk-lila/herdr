//! Client-local, never-sent drafts. A returned attempt is never replayed.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEventKind};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

use super::text_editor::TextEditor;
use crate::client::endpoint::ClientEndpointId;
use crate::detect::Agent;
use crate::protocol::{CellData, SurfaceRect};
use crate::raw_input::RawInputEvent;

const MAX_TARGETS: usize = 16;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_REPEAT: usize = 256;
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DraftTarget {
    pub endpoint_id: ClientEndpointId,
    pub pane_id: String,
}

/// One authoritative row, with complete cells and pane-relative input bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DraftAnchor {
    pub boot_id: String,
    pub agent: Agent,
    pub geometry: SurfaceRect,
    pub surface_size: (u16, u16),
    pub terminal_modes: (bool, bool),
    pub cursor_visible: bool,
    pub x: u16,
    pub y: u16,
    pub row: Vec<CellData>,
    pub input_start: usize,
    pub input_end: usize,
}

impl DraftAnchor {
    fn valid(&self) -> bool {
        let Some(x) = self.x.checked_sub(self.geometry.x) else {
            return false;
        };
        self.geometry.width != 0
            && self.geometry.height != 0
            && self.row.len() == usize::from(self.geometry.width)
            && u32::from(self.geometry.x) + u32::from(self.geometry.width)
                <= u32::from(self.surface_size.0)
            && u32::from(self.geometry.y) + u32::from(self.geometry.height)
                <= u32::from(self.surface_size.1)
            && self.y >= self.geometry.y
            && u32::from(self.y) < u32::from(self.geometry.y) + u32::from(self.geometry.height)
            && self.input_start <= usize::from(x)
            && usize::from(x) <= self.input_end
            && self.input_end < self.row.len()
            && self
                .row
                .iter()
                .try_fold(0usize, |bytes, symbol| {
                    bytes.checked_add(symbol.symbol.len())
                })
                .is_some_and(|bytes| bytes <= MAX_TEXT_BYTES)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DraftReason {
    Ready,
    NoAnchor,
    ManualRecovery,
    ContextChanged,
    UnsupportedText,
    AwaitingEcho,
    UncertainDelivery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DraftNotice {
    UnsupportedControl,
    LimitReached,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct EditResult {
    pub handled: bool,
    pub repaint: bool,
    /// The event was consumed but exceeded the draft bound.
    pub bounded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DraftAttempt {
    pub text: String,
    pub trailing_left: usize,
}

pub(super) struct DraftView<'a> {
    pub editor: &'a TextEditor,
    pub attempted: Option<&'a str>,
    pub uncertain: Option<&'a str>,
    pub reason: DraftReason,
    pub notice: Option<DraftNotice>,
}

struct PendingAttempt {
    payload: DraftAttempt,
    expected: DraftAnchor,
    generation: u64,
    started: Instant,
}

struct Draft {
    target: DraftTarget,
    anchor: Option<DraftAnchor>,
    allow_auto: bool,
    editor: TextEditor,
    pending: Option<PendingAttempt>,
    uncertain: Option<DraftAttempt>,
    reason: DraftReason,
    notice: Option<DraftNotice>,
}

impl Draft {
    fn retained_bytes(&self) -> usize {
        self.pending
            .as_ref()
            .map(|pending| pending.payload.text.len())
            .or_else(|| self.uncertain.as_ref().map(|attempt| attempt.text.len()))
            .unwrap_or(0)
    }

    fn make_uncertain(&mut self) -> bool {
        let Some(pending) = self.pending.take() else {
            return false;
        };
        self.uncertain = Some(pending.payload);
        self.allow_auto = false;
        self.reason = DraftReason::UncertainDelivery;
        true
    }

    fn expire(&mut self, now: Instant) -> bool {
        if self.pending.as_ref().is_some_and(|pending| {
            now.saturating_duration_since(pending.started) >= ATTEMPT_TIMEOUT
        }) {
            self.make_uncertain()
        } else {
            false
        }
    }
}

#[derive(Default)]
pub(super) struct ReconnectDrafts {
    entries: Vec<Draft>,
}

impl ReconnectDrafts {
    pub fn begin(
        &mut self,
        target: &DraftTarget,
        anchor: Option<DraftAnchor>,
        allow_auto: bool,
    ) -> bool {
        if self.entries.iter().any(|draft| draft.target == *target) {
            return true;
        }
        if self.entries.len() == MAX_TARGETS {
            return false;
        }
        let anchor = anchor.filter(DraftAnchor::valid);
        let reason = if anchor.is_none() {
            DraftReason::NoAnchor
        } else if !allow_auto {
            DraftReason::ManualRecovery
        } else {
            DraftReason::Ready
        };
        self.entries.push(Draft {
            target: target.clone(),
            anchor,
            allow_auto,
            editor: TextEditor::default(),
            pending: None,
            uncertain: None,
            reason,
            notice: None,
        });
        true
    }

    #[cfg(test)]
    fn targets(&self) -> impl Iterator<Item = &DraftTarget> {
        self.entries.iter().map(|draft| &draft.target)
    }

    pub fn view(&self, target: &DraftTarget) -> Option<DraftView<'_>> {
        let draft = self.entries.iter().find(|draft| draft.target == *target)?;
        Some(DraftView {
            editor: &draft.editor,
            attempted: draft
                .pending
                .as_ref()
                .map(|pending| pending.payload.text.as_str()),
            uncertain: draft
                .uncertain
                .as_ref()
                .map(|attempt| attempt.text.as_str()),
            reason: draft.reason,
            notice: draft.notice,
        })
    }

    pub fn edit(&mut self, target: &DraftTarget, event: &RawInputEvent) -> EditResult {
        let Some(draft) = self
            .entries
            .iter_mut()
            .find(|draft| draft.target == *target)
        else {
            return EditResult::default();
        };
        let mut candidate = draft.editor.clone();
        let mut bounded = false;
        let supported = match event {
            RawInputEvent::Key(key) if key.kind == KeyEventKind::Release => {
                return EditResult {
                    handled: true,
                    ..EditResult::default()
                };
            }
            RawInputEvent::Key(key) => {
                let repeat = usize::from(key.repeat_count.max(1));
                let text = key.generated_text.as_deref();
                let editable_code = match key.code {
                    KeyCode::Char(character) => !character.is_control(),
                    KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::Backspace
                    | KeyCode::Delete => true,
                    _ => false,
                };
                if !editable_code || text.is_some_and(|text| text.chars().any(char::is_control)) {
                    false
                } else if repeat > MAX_REPEAT
                    || text.is_some_and(|text| {
                        text.len().checked_mul(repeat).is_none_or(|bytes| {
                            bytes > MAX_TEXT_BYTES - draft.retained_bytes() - candidate.len()
                        })
                    })
                {
                    bounded = true;
                    true
                } else {
                    let mut supported = true;
                    for _ in 0..repeat {
                        if candidate.handle_key(key).is_none() {
                            supported = false;
                            break;
                        }
                        if candidate.len() + draft.retained_bytes() > MAX_TEXT_BYTES {
                            bounded = true;
                            break;
                        }
                    }
                    supported
                }
            }
            RawInputEvent::Text(text) => {
                if text.as_str().chars().any(char::is_control) {
                    false
                } else if text.as_str().len() + candidate.len() + draft.retained_bytes()
                    > MAX_TEXT_BYTES
                {
                    bounded = true;
                    true
                } else {
                    candidate.insert(text.as_str());
                    true
                }
            }
            RawInputEvent::Paste(text) => {
                if text.chars().any(char::is_control) {
                    false
                } else if text.len() + candidate.len() + draft.retained_bytes() > MAX_TEXT_BYTES {
                    bounded = true;
                    true
                } else {
                    candidate.insert(text);
                    true
                }
            }
            _ => return EditResult::default(),
        };
        let previous_notice = draft.notice;
        if !supported || bounded {
            draft.notice = Some(if bounded {
                DraftNotice::LimitReached
            } else {
                DraftNotice::UnsupportedControl
            });
            return EditResult {
                handled: true,
                repaint: previous_notice != draft.notice,
                bounded,
            };
        }
        draft.notice = None;
        let repaint = candidate != draft.editor || previous_notice.is_some();
        draft.editor = candidate;
        EditResult {
            handled: true,
            repaint,
            bounded: false,
        }
    }

    /// Marks delivery attempted before the caller can forward the returned payload.
    pub fn attempt(
        &mut self,
        target: &DraftTarget,
        current: &DraftAnchor,
        generation: u64,
        now: Instant,
    ) -> Option<DraftAttempt> {
        let draft = self
            .entries
            .iter_mut()
            .find(|draft| draft.target == *target)?;
        draft.expire(now);
        if !draft.allow_auto
            || draft.pending.is_some()
            || draft.uncertain.is_some()
            || draft.editor.is_empty()
        {
            return None;
        }
        let Some(anchor) = draft.anchor.as_ref() else {
            draft.reason = DraftReason::NoAnchor;
            return None;
        };
        if !current.valid() || anchor != current {
            draft.allow_auto = false;
            draft.reason = DraftReason::ContextChanged;
            return None;
        }
        let Some((expected, trailing_left)) = expected_anchor(anchor, &draft.editor) else {
            draft.reason = DraftReason::UnsupportedText;
            return None;
        };
        let payload = DraftAttempt {
            text: draft.editor.as_str().to_owned(),
            trailing_left,
        };
        draft.editor.clear();
        draft.pending = Some(PendingAttempt {
            payload: payload.clone(),
            expected,
            generation,
            started: now,
        });
        draft.reason = DraftReason::AwaitingEcho;
        draft.notice = None;
        Some(payload)
    }

    /// Exact echo only retires an attempt on its original connection generation.
    pub fn observe(
        &mut self,
        target: &DraftTarget,
        current: &DraftAnchor,
        generation: u64,
        now: Instant,
    ) -> bool {
        let Some(index) = self
            .entries
            .iter()
            .position(|draft| draft.target == *target)
        else {
            return false;
        };
        let draft = &mut self.entries[index];
        if draft.expire(now) {
            return true;
        }
        if let Some(pending) = draft.pending.as_ref() {
            if generation != pending.generation {
                return draft.make_uncertain();
            }
            if echo_matches(current, &pending.expected) {
                draft.anchor = Some(current.clone());
                draft.pending = None;
                draft.reason = DraftReason::Ready;
                if draft.editor.is_empty() {
                    self.entries.remove(index);
                }
                return true;
            }
            // Intermediate echoes are possible. No mismatch can authorize a retry.
            return false;
        }
        if draft.allow_auto && draft.anchor.as_ref() != Some(current) {
            draft.allow_auto = false;
            draft.reason = DraftReason::ContextChanged;
            return true;
        }
        false
    }

    pub fn disconnect(&mut self, target: &DraftTarget) -> bool {
        self.entries
            .iter_mut()
            .find(|draft| draft.target == *target)
            .is_some_and(Draft::make_uncertain)
    }

    pub fn hold(&mut self, target: &DraftTarget) -> bool {
        let Some(draft) = self
            .entries
            .iter_mut()
            .find(|draft| draft.target == *target)
        else {
            return false;
        };
        hold_draft(draft)
    }

    pub fn hold_all(&mut self) -> bool {
        let mut changed = false;
        for draft in &mut self.entries {
            changed |= hold_draft(draft);
        }
        changed
    }

    pub fn tick(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for draft in &mut self.entries {
            changed |= draft.expire(now);
        }
        changed
    }

    /// Explicit recovery includes uncertain/attempted text, which may already be remote.
    pub fn copy_text(&self, target: &DraftTarget) -> Option<String> {
        let draft = self.entries.iter().find(|draft| draft.target == *target)?;
        let attempt = draft
            .uncertain
            .as_ref()
            .or_else(|| draft.pending.as_ref().map(|pending| &pending.payload));
        let Some(attempt) = attempt else {
            return Some(draft.editor.as_str().to_owned());
        };
        let cursor = if attempt.trailing_left == 0 {
            attempt.text.len()
        } else {
            attempt
                .text
                .char_indices()
                .rev()
                .nth(attempt.trailing_left - 1)
                .map(|(index, _)| index)
                .unwrap_or(0)
        };
        let mut text = attempt.text.clone();
        text.insert_str(cursor, draft.editor.as_str());
        Some(text)
    }

    /// Find a stranded original target for recovery; never retarget its text.
    pub fn recovery_target(
        &self,
        endpoint: &ClientEndpointId,
        pane_exists: impl Fn(&str) -> bool,
    ) -> Option<&DraftTarget> {
        self.entries
            .iter()
            .rev()
            .find(|draft| {
                draft.target.endpoint_id == *endpoint
                    && !pane_exists(&draft.target.pane_id)
                    && (!draft.editor.is_empty()
                        || draft.pending.is_some()
                        || draft.uncertain.is_some())
            })
            .map(|draft| &draft.target)
    }

    pub fn discard(&mut self, target: &DraftTarget) -> bool {
        let count = self.entries.len();
        self.entries.retain(|draft| draft.target != *target);
        self.entries.len() != count
    }
}

fn hold_draft(draft: &mut Draft) -> bool {
    if draft.make_uncertain() {
        return true;
    }
    if draft.uncertain.is_some() {
        return false;
    }
    let changed = draft.allow_auto || draft.reason != DraftReason::ManualRecovery;
    draft.allow_auto = false;
    draft.reason = DraftReason::ManualRecovery;
    changed
}

fn safe_scalar(symbol: &str) -> bool {
    let mut chars = symbol.chars();
    let Some(character) = chars.next() else {
        return false;
    };
    chars.next().is_none()
        && !character.is_control()
        && UnicodeWidthChar::width(character) == Some(1)
        && crate::ghostty::unicode_codepoint_width(character as u32) == 1
}

fn echo_matches(current: &DraftAnchor, expected: &DraftAnchor) -> bool {
    current.boot_id == expected.boot_id
        && current.agent == expected.agent
        && current.geometry == expected.geometry
        && current.surface_size == expected.surface_size
        && current.terminal_modes == expected.terminal_modes
        && current.cursor_visible == expected.cursor_visible
        && current.x == expected.x
        && current.y == expected.y
        && current.input_start == expected.input_start
        && current.input_end == expected.input_end
        && current.row.len() == expected.row.len()
        && current
            .row
            .iter()
            .zip(&expected.row)
            .enumerate()
            .all(|(index, (actual, wanted))| {
                !actual.skip
                    && actual.symbol == wanted.symbol
                    && if (expected.input_start..=expected.input_end).contains(&index) {
                        actual.hyperlink.is_none()
                    } else {
                        actual == wanted
                    }
            })
}

fn expected_anchor(anchor: &DraftAnchor, editor: &TextEditor) -> Option<(DraftAnchor, usize)> {
    if !anchor.valid()
        || anchor.row.iter().any(|cell| cell.skip)
        || anchor.row[anchor.input_start..=anchor.input_end]
            .iter()
            .any(|cell| cell.hyperlink.is_some())
        || !anchor.row[anchor.input_end..]
            .iter()
            .all(|cell| cell.symbol == " ")
        || !anchor.row[anchor.input_start..anchor.input_end]
            .iter()
            .all(|cell| !cell.skip && cell.hyperlink.is_none() && safe_scalar(&cell.symbol))
    {
        return None;
    }
    let symbols: Vec<_> = editor.graphemes(true).collect();
    if !symbols.iter().all(|symbol| safe_scalar(symbol))
        || anchor.input_end.checked_add(symbols.len())? >= anchor.row.len()
    {
        return None;
    }
    let insertion = usize::from(anchor.x - anchor.geometry.x);
    let trailing_left = editor.as_str()[editor.cursor_position()..].chars().count();
    let mut expected = anchor.clone();
    for index in (insertion..anchor.input_end).rev() {
        expected.row[index + symbols.len()].clone_from(&anchor.row[index]);
    }
    for (index, symbol) in symbols.iter().enumerate() {
        expected.row[insertion + index].symbol = (*symbol).to_owned();
    }
    expected.input_end += symbols.len();
    // A scalar can join neighboring clusters even when it is one cell alone.
    let start = expected.input_start.saturating_sub(1);
    let segment: String = expected.row[start..=expected.input_end]
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect();
    if segment.graphemes(true).count() != expected.input_end - start + 1 {
        return None;
    }
    expected.x = anchor.geometry.x + (insertion + symbols.len() - trailing_left) as u16;
    Some((expected, trailing_left))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{TerminalKey, TextCommit};
    use crossterm::event::KeyModifiers;

    fn target(pane_id: &str) -> DraftTarget {
        DraftTarget {
            endpoint_id: ClientEndpointId::Local,
            pane_id: pane_id.to_owned(),
        }
    }

    fn anchor() -> DraftAnchor {
        let mut row = vec![
            CellData {
                symbol: " ".to_owned(),
                fg: 0,
                bg: 0,
                modifier: 0,
                skip: false,
                hyperlink: None,
            };
            40
        ];
        row[0].symbol = ">".to_owned();
        DraftAnchor {
            boot_id: "boot".to_owned(),
            agent: Agent::Codex,
            geometry: SurfaceRect {
                x: 3,
                y: 4,
                width: 40,
                height: 2,
            },
            surface_size: (80, 24),
            terminal_modes: (true, true),
            cursor_visible: true,
            x: 5,
            y: 4,
            row,
            input_start: 2,
            input_end: 2,
        }
    }

    fn text(drafts: &mut ReconnectDrafts, target: &DraftTarget, text: &str) -> EditResult {
        drafts.edit(target, &RawInputEvent::Text(TextCommit::new(text)))
    }

    fn key(drafts: &mut ReconnectDrafts, target: &DraftTarget, code: KeyCode) -> EditResult {
        drafts.edit(
            target,
            &RawInputEvent::Key(TerminalKey::new(code, KeyModifiers::NONE)),
        )
    }

    #[test]
    fn empty_begin_captures_new_text_and_only_exact_echo_retires_attempt() {
        let now = Instant::now();
        let target = target("pane");
        let initial = anchor();
        let mut drafts = ReconnectDrafts::default();
        assert!(drafts.begin(&target, Some(initial.clone()), true));
        assert!(drafts.view(&target).unwrap().editor.is_empty());
        text(&mut drafts, &target, "abλ");
        let payload = drafts.attempt(&target, &initial, 7, now).unwrap();
        assert_eq!(payload.text, "abλ");
        assert_eq!(payload.trailing_left, 0);
        assert!(drafts.attempt(&target, &initial, 7, now).is_none());
        assert!(!drafts.observe(&target, &initial, 7, now));
        let mut expected = initial;
        for (index, symbol) in ["a", "b", "λ"].into_iter().enumerate() {
            expected.row[2 + index].symbol = symbol.to_owned();
        }
        expected.x += 3;
        expected.input_end += 3;
        let mut wrong_cursor = expected.clone();
        wrong_cursor.x -= 1;
        assert!(!drafts.observe(&target, &wrong_cursor, 7, now));
        assert!(drafts.observe(&target, &expected, 7, now));
        assert!(drafts.view(&target).is_none());
    }

    #[test]
    fn second_disconnect_preserves_uncertain_attempt_and_new_unsent_suffix() {
        let now = Instant::now();
        let target = target("pane");
        let initial = anchor();
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target, Some(initial.clone()), true);
        text(&mut drafts, &target, "abc");
        drafts.attempt(&target, &initial, 1, now).unwrap();
        text(&mut drafts, &target, "def");
        assert!(drafts.disconnect(&target));
        assert!(!drafts.disconnect(&target));
        assert!(drafts.begin(&target, Some(initial.clone()), true));
        let view = drafts.view(&target).unwrap();
        assert_eq!(view.uncertain, Some("abc"));
        assert_eq!(view.editor.as_str(), "def");
        assert_eq!(view.reason, DraftReason::UncertainDelivery);
        assert!(drafts.attempt(&target, &initial, 2, now).is_none());
        assert_eq!(drafts.copy_text(&target).unwrap(), "abcdef");
        assert!(drafts.discard(&target));
    }

    #[test]
    fn generation_change_and_timeout_never_authorize_replay_or_late_retirement() {
        for timeout in [false, true] {
            let now = Instant::now();
            let target = target("pane");
            let initial = anchor();
            let mut drafts = ReconnectDrafts::default();
            drafts.begin(&target, Some(initial.clone()), true);
            text(&mut drafts, &target, "x");
            drafts.attempt(&target, &initial, 1, now).unwrap();
            let expected = drafts.entries[0].pending.as_ref().unwrap().expected.clone();
            if timeout {
                assert!(drafts.tick(now + ATTEMPT_TIMEOUT));
                assert!(!drafts.observe(&target, &expected, 1, now + ATTEMPT_TIMEOUT));
            } else {
                assert!(drafts.observe(&target, &expected, 2, now));
            }
            assert_eq!(drafts.view(&target).unwrap().uncertain, Some("x"));
            assert!(drafts.attempt(&target, &initial, 2, now).is_none());
        }
    }

    #[test]
    fn exact_echo_preserves_new_suffix_and_allows_a_separate_attempt() {
        let now = Instant::now();
        let target = target("pane");
        let initial = anchor();
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target, Some(initial.clone()), true);
        text(&mut drafts, &target, "abc");
        drafts.attempt(&target, &initial, 1, now).unwrap();
        let expected = drafts.entries[0].pending.as_ref().unwrap().expected.clone();
        text(&mut drafts, &target, "def");
        assert!(drafts.observe(&target, &expected, 1, now));
        assert_eq!(drafts.copy_text(&target).unwrap(), "def");
        assert_eq!(
            drafts.attempt(&target, &expected, 1, now).unwrap().text,
            "def"
        );
    }

    #[test]
    fn every_anchor_identity_change_blocks_auto_without_losing_draft() {
        for case in 0..10 {
            let now = Instant::now();
            let target = target("pane");
            let initial = anchor();
            let mut changed = initial.clone();
            match case {
                0 => changed.boot_id.push('2'),
                1 => changed.agent = Agent::Claude,
                2 => changed.geometry.x += 1,
                3 => changed.surface_size.0 += 1,
                4 => changed.terminal_modes.0 = false,
                5 => changed.cursor_visible = false,
                6 => changed.x += 1,
                7 => changed.y += 1,
                8 => changed.row[20].symbol = "x".to_owned(),
                9 => changed.input_start += 1,
                _ => unreachable!(),
            }
            let mut drafts = ReconnectDrafts::default();
            drafts.begin(&target, Some(initial.clone()), true);
            text(&mut drafts, &target, "draft");
            assert!(
                drafts.attempt(&target, &changed, 1, now).is_none(),
                "case {case}"
            );
            assert_eq!(
                drafts.view(&target).unwrap().reason,
                DraftReason::ContextChanged
            );
            assert!(drafts.attempt(&target, &initial, 1, now).is_none());
            assert_eq!(drafts.copy_text(&target).unwrap(), "draft");
        }
    }

    #[test]
    fn unicode_local_editor_is_grapheme_safe_but_complex_text_stays_local() {
        let target = target("pane");
        let initial = anchor();
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target, Some(initial.clone()), true);
        text(&mut drafts, &target, "e\u{301}中👩‍💻");
        assert!(drafts
            .attempt(&target, &initial, 1, Instant::now())
            .is_none());
        assert_eq!(
            drafts.view(&target).unwrap().reason,
            DraftReason::UnsupportedText
        );
        for remaining in ["e\u{301}中", "e\u{301}", ""] {
            key(&mut drafts, &target, KeyCode::Backspace);
            assert_eq!(drafts.copy_text(&target).unwrap(), remaining);
        }
        text(&mut drafts, &target, "λ");
        assert!(drafts
            .attempt(&target, &initial, 1, Instant::now())
            .is_some());
    }

    #[test]
    fn software_cursor_and_local_cursor_movement_use_authoritative_coordinates() {
        let target = target("pane");
        let mut initial = anchor();
        initial.cursor_visible = false;
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target, Some(initial.clone()), true);
        text(&mut drafts, &target, "abc");
        key(&mut drafts, &target, KeyCode::Left);
        let attempt = drafts
            .attempt(&target, &initial, 1, Instant::now())
            .unwrap();
        assert_eq!(attempt.trailing_left, 1);
        let expected = &drafts.entries[0].pending.as_ref().unwrap().expected;
        assert_eq!(expected.x, initial.x + 2);
        assert_eq!(expected.input_end, initial.input_end + 3);
    }

    #[test]
    fn controls_and_multiline_pastes_are_rejected_without_silent_normalization() {
        let target = target("pane");
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target, None, false);
        text(&mut drafts, &target, "kept");
        for event in [
            RawInputEvent::Paste("line\nline".into()),
            RawInputEvent::Text(TextCommit::new("tab\ttext")),
            RawInputEvent::Key(TerminalKey::new(KeyCode::Enter, KeyModifiers::NONE)),
            RawInputEvent::Key(TerminalKey::new(KeyCode::Esc, KeyModifiers::NONE)),
            RawInputEvent::Key(TerminalKey::new(KeyCode::F(1), KeyModifiers::NONE)),
        ] {
            assert!(drafts.edit(&target, &event).handled);
            assert_eq!(drafts.copy_text(&target).unwrap(), "kept");
            assert_eq!(
                drafts.view(&target).unwrap().notice,
                Some(DraftNotice::UnsupportedControl)
            );
        }
        text(&mut drafts, &target, "x");
        assert_eq!(drafts.view(&target).unwrap().notice, None);
    }

    #[test]
    fn target_and_total_text_bounds_never_evict_or_truncate_recovery() {
        let mut drafts = ReconnectDrafts::default();
        for index in 0..MAX_TARGETS {
            assert!(drafts.begin(&target(&index.to_string()), None, false));
        }
        assert!(!drafts.begin(&target("overflow"), None, false));
        assert_eq!(drafts.targets().count(), MAX_TARGETS);
        let target = target("0");
        let full = "x".repeat(MAX_TEXT_BYTES);
        assert!(!text(&mut drafts, &target, &full).bounded);
        assert!(text(&mut drafts, &target, "x").bounded);
        assert_eq!(drafts.copy_text(&target).unwrap(), full);
        key(&mut drafts, &target, KeyCode::Backspace);
        assert!(text(&mut drafts, &target, "λ").bounded);
        assert_eq!(drafts.copy_text(&target).unwrap().len(), MAX_TEXT_BYTES - 1);
    }

    #[test]
    fn attempted_text_counts_toward_the_bound_and_is_immutable_during_edits() {
        let target = target("pane");
        let initial = anchor();
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target, Some(initial.clone()), true);
        text(&mut drafts, &target, "abc");
        drafts
            .attempt(&target, &initial, 1, Instant::now())
            .unwrap();
        assert!(!text(&mut drafts, &target, &"x".repeat(MAX_TEXT_BYTES - 3)).bounded);
        assert!(text(&mut drafts, &target, "z").bounded);
        key(&mut drafts, &target, KeyCode::Backspace);
        assert_eq!(drafts.view(&target).unwrap().attempted, Some("abc"));
        assert_eq!(drafts.copy_text(&target).unwrap().len(), MAX_TEXT_BYTES - 1);
    }

    #[test]
    fn hold_blocks_auto_and_target_identity_isolates_recovery() {
        let now = Instant::now();
        let first = target("first");
        let second = target("second");
        let initial = anchor();
        let mut drafts = ReconnectDrafts::default();
        for target in [&first, &second] {
            drafts.begin(target, Some(initial.clone()), true);
            text(&mut drafts, target, "x");
        }
        assert!(drafts.hold(&first));
        assert!(drafts.attempt(&first, &initial, 1, now).is_none());
        assert!(drafts.attempt(&second, &initial, 1, now).is_some());
        assert!(drafts.hold_all());
        assert_eq!(
            drafts.view(&second).unwrap().reason,
            DraftReason::UncertainDelivery
        );
        assert_eq!(
            drafts.view(&first).unwrap().reason,
            DraftReason::ManualRecovery
        );
        assert_eq!(drafts.copy_text(&first).unwrap(), "x");
        assert_eq!(drafts.copy_text(&second).unwrap(), "x");
        assert!(!drafts.hold_all());
    }

    #[test]
    fn repeats_are_atomic_bounded_and_release_does_not_edit() {
        let target = target("pane");
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target, None, false);
        let mut event = TerminalKey::new(KeyCode::Char('λ'), KeyModifiers::NONE);
        event.repeat_count = 3;
        assert!(
            drafts
                .edit(&target, &RawInputEvent::Key(event.clone()))
                .handled
        );
        assert_eq!(drafts.copy_text(&target).unwrap(), "λλλ");
        event.repeat_count = (MAX_REPEAT + 1) as u16;
        assert!(
            drafts
                .edit(&target, &RawInputEvent::Key(event.clone()))
                .bounded
        );
        assert_eq!(drafts.copy_text(&target).unwrap(), "λλλ");
        event.kind = KeyEventKind::Release;
        assert!(!drafts.edit(&target, &RawInputEvent::Key(event)).repaint);
        let event = TerminalKey::new(KeyCode::Enter, KeyModifiers::NONE)
            .with_generated_text(Some("never insert".into()));
        drafts.edit(&target, &RawInputEvent::Key(event));
        assert_eq!(drafts.copy_text(&target).unwrap(), "λλλ");
    }

    #[test]
    fn absent_and_invalid_anchors_keep_copy_recovery_and_cannot_gain_trust() {
        for mut initial in [None, Some(anchor())] {
            if let Some(anchor) = initial.as_mut() {
                anchor.input_end = anchor.row.len();
            }
            let target = target("pane");
            let mut drafts = ReconnectDrafts::default();
            drafts.begin(&target, initial, true);
            text(&mut drafts, &target, "kept");
            assert!(drafts
                .attempt(&target, &anchor(), 1, Instant::now())
                .is_none());
            drafts.begin(&target, Some(anchor()), true);
            assert!(drafts
                .attempt(&target, &anchor(), 1, Instant::now())
                .is_none());
            assert_eq!(drafts.view(&target).unwrap().reason, DraftReason::NoAnchor);
            assert_eq!(drafts.copy_text(&target).unwrap(), "kept");
        }
    }

    #[test]
    fn opaque_tail_unsafe_existing_text_and_wrapping_remain_copy_only() {
        for case in 0..3 {
            let target = target("pane");
            let mut initial = anchor();
            match case {
                0 => initial.row[20].symbol = "opaque".to_owned(),
                1 => {
                    initial.input_end += 1;
                    initial.row[2].symbol = "中".to_owned();
                }
                2 => {
                    initial.input_end = 38;
                    initial.x = initial.geometry.x + 38;
                }
                _ => unreachable!(),
            }
            let mut drafts = ReconnectDrafts::default();
            drafts.begin(&target, Some(initial.clone()), true);
            text(&mut drafts, &target, "abc");
            assert!(drafts
                .attempt(&target, &initial, 1, Instant::now())
                .is_none());
            assert_eq!(drafts.copy_text(&target).unwrap(), "abc");
        }
    }

    #[test]
    fn restored_middle_caret_drains_new_typing_into_the_confirmed_tail() {
        let now = Instant::now();
        let target = target("pane");
        let initial = anchor();
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target, Some(initial.clone()), true);
        text(&mut drafts, &target, "abcd");
        key(&mut drafts, &target, KeyCode::Left);
        key(&mut drafts, &target, KeyCode::Left);
        let first = drafts.attempt(&target, &initial, 1, now).unwrap();
        assert_eq!(first.trailing_left, 2);
        let first_echo = drafts.entries[0].pending.as_ref().unwrap().expected.clone();
        text(&mut drafts, &target, "xy");
        assert!(drafts.observe(&target, &first_echo, 1, now));
        let second = drafts.attempt(&target, &first_echo, 1, now).unwrap();
        assert_eq!(second.text, "xy");
        let second_echo = drafts.entries[0].pending.as_ref().unwrap().expected.clone();
        assert_eq!(
            second_echo.row[2..8]
                .iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>(),
            "abxycd"
        );
        assert_eq!(second_echo.x, initial.x + 4);
        assert!(drafts.observe(&target, &second_echo, 1, now));
        assert!(drafts.view(&target).is_none());
    }

    #[test]
    fn originally_middle_caret_can_insert_only_with_safe_observed_input_bounds() {
        let target = target("pane");
        let mut initial = anchor();
        initial.row[2].symbol = "a".to_owned();
        initial.row[3].symbol = "b".to_owned();
        initial.input_end = 4;
        initial.x += 1;
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target, Some(initial.clone()), true);
        text(&mut drafts, &target, "λ");
        assert!(drafts
            .attempt(&target, &initial, 1, Instant::now())
            .is_some());
        let expected = &drafts.entries[0].pending.as_ref().unwrap().expected;
        assert_eq!(
            expected.row[2..5]
                .iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>(),
            "aλb"
        );
        assert_eq!(expected.x, initial.x + 1);
    }

    #[test]
    fn recovery_copy_inserts_new_suffix_at_the_attempted_cursor_after_a_drop() {
        for (before, left, suffix, expected) in [
            ("abc", 1, "x", "abxc"),
            ("aλc", 2, "👩‍💻", "a👩‍💻λc"),
            ("abc", 3, "x", "xabc"),
            ("abc", 0, "x", "abcx"),
        ] {
            let target = target("pane");
            let initial = anchor();
            let mut drafts = ReconnectDrafts::default();
            drafts.begin(&target, Some(initial.clone()), true);
            text(&mut drafts, &target, before);
            for _ in 0..left {
                key(&mut drafts, &target, KeyCode::Left);
            }
            drafts
                .attempt(&target, &initial, 1, Instant::now())
                .unwrap();
            text(&mut drafts, &target, suffix);
            assert_eq!(drafts.copy_text(&target).unwrap(), expected);
            drafts.disconnect(&target);
            assert_eq!(drafts.copy_text(&target).unwrap(), expected);
            key(&mut drafts, &target, KeyCode::Backspace);
            assert_eq!(drafts.copy_text(&target).unwrap(), before);
        }
    }

    #[test]
    fn recovery_fallback_returns_latest_missing_original_target_and_skips_empty() {
        let mut drafts = ReconnectDrafts::default();
        let first = target("first");
        let live = target("live");
        let latest = target("latest");
        let empty = target("empty");
        for target in [&first, &live, &latest, &empty] {
            drafts.begin(target, None, false);
            if target != &empty {
                text(&mut drafts, target, "held");
            }
        }
        let foreign = DraftTarget {
            endpoint_id: ClientEndpointId::Ssh(
                crate::client::endpoint::ProfileId::parse("a".repeat(32)).unwrap(),
            ),
            pane_id: "foreign".into(),
        };
        drafts.begin(&foreign, None, false);
        text(&mut drafts, &foreign, "separate");
        assert_eq!(
            drafts.recovery_target(&ClientEndpointId::Local, |pane| pane == "live"),
            Some(&latest)
        );
        assert_eq!(drafts.copy_text(&latest).unwrap(), "held");
        drafts.discard(&latest);
        assert_eq!(
            drafts.recovery_target(&ClientEndpointId::Local, |pane| pane == "live"),
            Some(&first)
        );
        assert!(drafts
            .recovery_target(&ClientEndpointId::Local, |_| true)
            .is_none());
        assert_eq!(
            drafts.recovery_target(&foreign.endpoint_id, |_| false),
            Some(&foreign)
        );
    }

    fn change_metadata(cell: &mut CellData, field: usize) {
        match field {
            0 => cell.fg = 1,
            1 => cell.bg = 2,
            2 => cell.modifier = 4,
            3 => cell.skip = true,
            4 => cell.hyperlink = Some(7),
            _ => unreachable!(),
        }
    }

    #[test]
    fn full_cell_metadata_changes_before_attempt_hold_the_unchanged_text() {
        for observe_first in [false, true] {
            for index in [0, 2, 20] {
                for field in 0..5 {
                    let now = Instant::now();
                    let target = target("pane");
                    let initial = anchor();
                    let mut current = initial.clone();
                    change_metadata(&mut current.row[index], field);
                    assert!(current
                        .row
                        .iter()
                        .zip(&initial.row)
                        .all(|(a, b)| a.symbol == b.symbol));
                    let mut drafts = ReconnectDrafts::default();
                    drafts.begin(&target, Some(initial.clone()), true);
                    text(&mut drafts, &target, "kept");
                    if observe_first {
                        assert!(drafts.observe(&target, &current, 1, now));
                    }
                    assert!(drafts.attempt(&target, &current, 1, now).is_none());
                    assert_eq!(
                        drafts.view(&target).unwrap().reason,
                        DraftReason::ContextChanged
                    );
                    assert!(drafts.attempt(&target, &initial, 1, now).is_none());
                    assert_eq!(drafts.copy_text(&target).unwrap(), "kept");
                }
            }
        }
    }

    #[test]
    fn edited_text_echo_adopts_authoritative_styles_and_software_caret_padding() {
        let now = Instant::now();
        let target = target("pane");
        let mut initial = anchor();
        initial.cursor_visible = false;
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target, Some(initial.clone()), true);
        text(&mut drafts, &target, "abc");
        drafts.attempt(&target, &initial, 1, now).unwrap();
        let mut echo = drafts.entries[0].pending.as_ref().unwrap().expected.clone();
        for cell in &mut echo.row[echo.input_start..=echo.input_end] {
            cell.fg = 1;
            cell.bg = 2;
            cell.modifier = 4;
        }
        text(&mut drafts, &target, "λ");
        assert!(drafts.observe(&target, &echo, 1, now));
        assert_eq!(drafts.entries[0].anchor.as_ref(), Some(&echo));
        let next = drafts.attempt(&target, &echo, 1, now).unwrap();
        assert_eq!(next.text, "λ");
        let expected = &drafts.entries[0].pending.as_ref().unwrap().expected;
        assert_eq!(expected.row[2..5], echo.row[2..5]);
        assert_eq!(expected.row[5].symbol, "λ");
        assert_eq!(expected.row[5].fg, echo.row[5].fg);
        assert_eq!(expected.row[5].bg, echo.row[5].bg);
        assert_eq!(expected.row[5].modifier, echo.row[5].modifier);
    }

    #[test]
    fn non_edit_metadata_and_unsafe_cell_flags_cannot_confirm_an_attempt() {
        for index in [0, 20, 2] {
            for field in 0..5 {
                if index == 2 && field < 3 {
                    continue;
                }
                let now = Instant::now();
                let target = target("pane");
                let initial = anchor();
                let mut drafts = ReconnectDrafts::default();
                drafts.begin(&target, Some(initial.clone()), true);
                text(&mut drafts, &target, "abc");
                drafts.attempt(&target, &initial, 1, now).unwrap();
                let mut echo = drafts.entries[0].pending.as_ref().unwrap().expected.clone();
                change_metadata(&mut echo.row[index], field);
                assert!(
                    !drafts.observe(&target, &echo, 1, now),
                    "index {index} field {field}"
                );
                assert_eq!(drafts.view(&target).unwrap().attempted, Some("abc"));
                assert!(drafts.attempt(&target, &echo, 1, now).is_none());
                assert!(drafts.tick(now + ATTEMPT_TIMEOUT));
                assert_eq!(drafts.view(&target).unwrap().uncertain, Some("abc"));
            }
        }
    }
}

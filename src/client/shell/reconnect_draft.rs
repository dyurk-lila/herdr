//! Bounded client-local scratchpads containing only input not yet accepted for sending.

use crossterm::event::{KeyCode, KeyEventKind};
use unicode_segmentation::UnicodeSegmentation;

use super::text_editor::TextEditor;
use crate::client::endpoint::ClientEndpointId;
use crate::raw_input::RawInputEvent;

const MAX_TARGETS: usize = 16;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_REPEAT: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DraftTarget {
    pub endpoint_id: ClientEndpointId,
    pub pane_id: String,
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
    /// The event was consumed but exceeded the scratchpad bound.
    pub bounded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DraftAttempt {
    pub text: String,
    pub trailing_left: usize,
}

pub(super) struct DraftView<'a> {
    pub editor: &'a TextEditor,
    pub notice: Option<DraftNotice>,
}

struct Draft {
    target: DraftTarget,
    editor: TextEditor,
    notice: Option<DraftNotice>,
}

#[derive(Default)]
pub(super) struct ReconnectDrafts {
    entries: Vec<Draft>,
}

impl ReconnectDrafts {
    pub fn begin(&mut self, target: &DraftTarget) -> bool {
        if self.entries.iter().any(|draft| draft.target == *target) {
            return true;
        }
        if self.entries.len() == MAX_TARGETS {
            return false;
        }
        self.entries.push(Draft {
            target: target.clone(),
            editor: TextEditor::default(),
            notice: None,
        });
        true
    }

    #[cfg(test)]
    pub fn targets(&self) -> impl Iterator<Item = &DraftTarget> {
        self.entries.iter().map(|draft| &draft.target)
    }

    pub fn view(&self, target: &DraftTarget) -> Option<DraftView<'_>> {
        let draft = self.entries.iter().find(|draft| draft.target == *target)?;
        Some(DraftView {
            editor: &draft.editor,
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
                        text.len()
                            .checked_mul(repeat)
                            .is_none_or(|bytes| bytes > MAX_TEXT_BYTES - candidate.len())
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
                        if candidate.len() > MAX_TEXT_BYTES {
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
                } else if text.as_str().len() > MAX_TEXT_BYTES - candidate.len() {
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
                } else if text.len() > MAX_TEXT_BYTES - candidate.len() {
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

    /// Preparing input does not clear it: a failed enqueue leaves the scratchpad intact.
    pub fn attempt(&self, target: &DraftTarget) -> Option<DraftAttempt> {
        let draft = self.entries.iter().find(|draft| draft.target == *target)?;
        if draft.editor.is_empty() {
            return None;
        }
        Some(DraftAttempt {
            text: draft.editor.as_str().to_owned(),
            trailing_left: draft.editor.as_str()[draft.editor.cursor_position()..]
                .graphemes(true)
                .count(),
        })
    }

    /// Call only after the canonical endpoint reports that it accepted the input.
    pub fn commit(&mut self, target: &DraftTarget) -> bool {
        self.discard(target)
    }

    pub fn target_for_endpoint(&self, endpoint: &ClientEndpointId) -> Option<&DraftTarget> {
        self.entries
            .iter()
            .find(|draft| &draft.target.endpoint_id == endpoint)
            .map(|draft| &draft.target)
    }

    pub fn remove_empty(&mut self, endpoint: &ClientEndpointId) -> bool {
        let count = self.entries.len();
        self.entries
            .retain(|draft| &draft.target.endpoint_id != endpoint || !draft.editor.is_empty());
        self.entries.len() != count
    }

    #[cfg(test)]
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
                    && !draft.editor.is_empty()
            })
            .map(|draft| &draft.target)
    }

    pub fn discard(&mut self, target: &DraftTarget) -> bool {
        let count = self.entries.len();
        self.entries.retain(|draft| draft.target != *target);
        self.entries.len() != count
    }

    #[cfg(test)]
    pub fn copy_text(&self, target: &DraftTarget) -> Option<String> {
        self.view(target)
            .map(|view| view.editor.as_str().to_owned())
    }
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
    fn preparing_and_retrying_does_not_clear_text_before_accepted_send() {
        let target = target("pane");
        let mut drafts = ReconnectDrafts::default();
        assert!(drafts.begin(&target));
        assert!(drafts.attempt(&target).is_none());
        text(&mut drafts, &target, "abλ");
        let first = drafts.attempt(&target).unwrap();
        assert_eq!(first.text, "abλ");
        assert_eq!(first.trailing_left, 0);
        assert_eq!(drafts.attempt(&target), Some(first));
        assert_eq!(drafts.view(&target).unwrap().editor.as_str(), "abλ");
        assert!(drafts.commit(&target));
        assert!(drafts.view(&target).is_none());
        assert!(drafts.attempt(&target).is_none());
        assert!(!drafts.commit(&target));
    }

    #[test]
    fn failed_send_and_another_outage_preserve_unsent_text_and_cursor() {
        let target = target("pane");
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target);
        text(&mut drafts, &target, "abcd");
        key(&mut drafts, &target, KeyCode::Left);
        key(&mut drafts, &target, KeyCode::Left);
        assert_eq!(drafts.attempt(&target).unwrap().trailing_left, 2);
        drafts.begin(&target);
        text(&mut drafts, &target, "λ");
        let retry = drafts.attempt(&target).unwrap();
        assert_eq!(retry.text, "abλcd");
        assert_eq!(retry.trailing_left, 2);
        assert!(drafts.commit(&target));
        assert!(drafts.view(&target).is_none());
    }

    #[test]
    fn accepted_text_is_never_resurrected_during_a_second_outage() {
        let target = target("pane");
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target);
        text(&mut drafts, &target, "first");
        assert_eq!(drafts.attempt(&target).unwrap().text, "first");
        drafts.commit(&target);
        assert!(drafts.begin(&target));
        assert!(drafts.view(&target).unwrap().editor.is_empty());
        text(&mut drafts, &target, "second");
        assert_eq!(drafts.attempt(&target).unwrap().text, "second");
    }

    #[test]
    fn unicode_editing_and_restored_cursor_count_graphemes() {
        let target = target("pane");
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target);
        text(&mut drafts, &target, "e\u{301}中👩‍💻");
        key(&mut drafts, &target, KeyCode::Left);
        assert_eq!(drafts.attempt(&target).unwrap().trailing_left, 1);
        key(&mut drafts, &target, KeyCode::Left);
        assert_eq!(drafts.attempt(&target).unwrap().trailing_left, 2);
        key(&mut drafts, &target, KeyCode::Home);
        assert_eq!(drafts.attempt(&target).unwrap().trailing_left, 3);
        key(&mut drafts, &target, KeyCode::Delete);
        assert_eq!(drafts.attempt(&target).unwrap().text, "中👩‍💻");
        key(&mut drafts, &target, KeyCode::End);
        for remaining in ["中", ""] {
            key(&mut drafts, &target, KeyCode::Backspace);
            assert_eq!(drafts.view(&target).unwrap().editor.as_str(), remaining);
        }
        assert!(drafts.attempt(&target).is_none());
    }

    #[test]
    fn wide_and_long_text_flush_without_terminal_row_or_prediction_constraints() {
        let target = target("pane");
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target);
        let long = "中👩‍💻λ".repeat(300);
        assert!(!text(&mut drafts, &target, &long).bounded);
        key(&mut drafts, &target, KeyCode::Left);
        assert_eq!(
            drafts.attempt(&target),
            Some(DraftAttempt {
                text: long,
                trailing_left: 1,
            })
        );
    }

    #[test]
    fn local_word_deletion_and_cursor_edits_compose_before_flush() {
        let target = target("pane");
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target);
        text(&mut drafts, &target, "hello world");
        drafts.edit(
            &target,
            &RawInputEvent::Key(TerminalKey::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
        );
        assert_eq!(drafts.view(&target).unwrap().editor.as_str(), "hello ");
        text(&mut drafts, &target, "new");
        key(&mut drafts, &target, KeyCode::Home);
        key(&mut drafts, &target, KeyCode::Delete);
        text(&mut drafts, &target, "H");
        key(&mut drafts, &target, KeyCode::End);
        assert_eq!(drafts.attempt(&target).unwrap().text, "Hello new");
        assert_eq!(drafts.attempt(&target).unwrap().trailing_left, 0);
    }

    #[test]
    fn controls_and_multiline_pastes_are_rejected_without_silent_normalization() {
        let target = target("pane");
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target);
        text(&mut drafts, &target, "kept");
        for event in [
            RawInputEvent::Paste("line\nline".into()),
            RawInputEvent::Text(TextCommit::new("tab\ttext")),
            RawInputEvent::Key(TerminalKey::new(KeyCode::Enter, KeyModifiers::NONE)),
            RawInputEvent::Key(TerminalKey::new(KeyCode::Esc, KeyModifiers::NONE)),
            RawInputEvent::Key(TerminalKey::new(KeyCode::F(1), KeyModifiers::NONE)),
        ] {
            assert!(drafts.edit(&target, &event).handled);
            assert_eq!(drafts.view(&target).unwrap().editor.as_str(), "kept");
            assert_eq!(
                drafts.view(&target).unwrap().notice,
                Some(DraftNotice::UnsupportedControl)
            );
        }
        text(&mut drafts, &target, "x");
        assert_eq!(drafts.view(&target).unwrap().notice, None);
    }

    #[test]
    fn target_and_text_bounds_never_evict_or_truncate_unsent_input() {
        let mut drafts = ReconnectDrafts::default();
        let overflow = target("overflow");
        for index in 0..MAX_TARGETS {
            assert!(drafts.begin(&target(&index.to_string())));
        }
        assert!(!drafts.begin(&overflow));
        assert_eq!(drafts.targets().count(), MAX_TARGETS);
        let target = target("0");
        let full = "x".repeat(MAX_TEXT_BYTES);
        assert!(!text(&mut drafts, &target, &full).bounded);
        assert!(text(&mut drafts, &target, "x").bounded);
        assert_eq!(drafts.attempt(&target).unwrap().text, full);
        key(&mut drafts, &target, KeyCode::Backspace);
        assert!(text(&mut drafts, &target, "λ").bounded);
        assert_eq!(
            drafts.view(&target).unwrap().editor.len(),
            MAX_TEXT_BYTES - 1
        );
        assert!(drafts.commit(&target));
        assert!(drafts.begin(&overflow));
    }

    #[test]
    fn commit_is_scoped_to_both_endpoint_and_pane() {
        let first = target("pane");
        let second = target("other-pane");
        let foreign = DraftTarget {
            endpoint_id: ClientEndpointId::Ssh(
                crate::client::endpoint::ProfileId::parse("a".repeat(32)).unwrap(),
            ),
            pane_id: "pane".into(),
        };
        let mut drafts = ReconnectDrafts::default();
        for (target, value) in [
            (&first, "first"),
            (&second, "second"),
            (&foreign, "foreign"),
        ] {
            drafts.begin(target);
            text(&mut drafts, target, value);
        }
        assert!(drafts.commit(&first));
        assert!(drafts.view(&first).is_none());
        assert_eq!(drafts.attempt(&second).unwrap().text, "second");
        assert_eq!(drafts.attempt(&foreign).unwrap().text, "foreign");
    }

    #[test]
    fn repeats_are_atomic_bounded_and_release_does_not_edit() {
        let target = target("pane");
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target);
        let mut event = TerminalKey::new(KeyCode::Char('λ'), KeyModifiers::NONE);
        event.repeat_count = 3;
        assert!(
            drafts
                .edit(&target, &RawInputEvent::Key(event.clone()))
                .handled
        );
        assert_eq!(drafts.view(&target).unwrap().editor.as_str(), "λλλ");
        event.repeat_count = (MAX_REPEAT + 1) as u16;
        assert!(
            drafts
                .edit(&target, &RawInputEvent::Key(event.clone()))
                .bounded
        );
        assert_eq!(drafts.view(&target).unwrap().editor.as_str(), "λλλ");
        event.kind = KeyEventKind::Release;
        assert!(!drafts.edit(&target, &RawInputEvent::Key(event)).repaint);
        let event = TerminalKey::new(KeyCode::Enter, KeyModifiers::NONE)
            .with_generated_text(Some("never insert".into()));
        drafts.edit(&target, &RawInputEvent::Key(event));
        assert_eq!(drafts.view(&target).unwrap().editor.as_str(), "λλλ");
    }

    #[test]
    fn repeated_yank_cannot_exceed_the_text_bound_or_partially_edit() {
        let target = target("pane");
        let mut drafts = ReconnectDrafts::default();
        drafts.begin(&target);
        text(&mut drafts, &target, &"x".repeat(MAX_TEXT_BYTES));
        drafts.edit(
            &target,
            &RawInputEvent::Key(TerminalKey::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        );
        let mut yank = TerminalKey::new(KeyCode::Char('y'), KeyModifiers::CONTROL);
        yank.repeat_count = 2;
        assert!(
            drafts
                .edit(&target, &RawInputEvent::Key(yank.clone()))
                .bounded
        );
        assert!(drafts.view(&target).unwrap().editor.is_empty());
        yank.repeat_count = 1;
        assert!(!drafts.edit(&target, &RawInputEvent::Key(yank)).bounded);
        assert_eq!(drafts.view(&target).unwrap().editor.len(), MAX_TEXT_BYTES);
    }

    #[test]
    fn missing_original_target_never_retargets_unsent_text() {
        let mut drafts = ReconnectDrafts::default();
        let first = target("first");
        let live = target("live");
        let latest = target("latest");
        let empty = target("empty");
        for target in [&first, &live, &latest, &empty] {
            drafts.begin(target);
            if target != &empty {
                text(&mut drafts, target, "unsent");
            }
        }
        assert_eq!(
            drafts.recovery_target(&ClientEndpointId::Local, |pane| pane == "live"),
            Some(&latest)
        );
        drafts.discard(&latest);
        assert_eq!(
            drafts.recovery_target(&ClientEndpointId::Local, |pane| pane == "live"),
            Some(&first)
        );
        assert!(drafts
            .recovery_target(&ClientEndpointId::Local, |_| true)
            .is_none());
    }
}

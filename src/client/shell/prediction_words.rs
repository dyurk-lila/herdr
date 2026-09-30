//! Bounded hypotheses for an observed previous-word deletion. Each input gesture
//! needs its own rules; agreement is useful evidence, not an editor capability.

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

const MAX_PREFIX_BYTES: usize = 2048;
const WORD_PUNCTUATION: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";

#[derive(Clone, Copy, Debug)]
enum Rule {
    Whitespace,
    CharacterRun,
    AlphanumericWord,
    UnicodeWord,
}

const RULES: [Rule; 4] = [
    Rule::Whitespace,
    Rule::CharacterRun,
    Rule::AlphanumericWord,
    Rule::UnicodeWord,
];
// Each family has both trailing-space behaviors. These indices are persisted;
// changes to their meaning require a new profile schema version.
const CANDIDATE_COUNT: usize = RULES.len() * 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WordRules {
    candidates: [bool; CANDIDATE_COUNT],
    trained: bool,
}

impl Default for WordRules {
    fn default() -> Self {
        Self {
            candidates: [true; CANDIDATE_COUNT],
            trained: false,
        }
    }
}

impl WordRules {
    pub(super) fn is_trained(&self) -> bool {
        self.trained && self.is_valid()
    }

    /// Validate deserialized evidence before applying or persisting it.
    pub(super) fn is_valid(&self) -> bool {
        if self.trained {
            self.candidates.iter().any(|active| *active)
        } else {
            self.candidates.iter().all(|active| *active)
        }
    }

    /// Intersect independent trained observations. Contradiction forgets the
    /// profile rather than choosing which observation to trust.
    pub(super) fn merge_observation(&mut self, other: &Self) -> bool {
        if !other.is_trained() {
            return false;
        }
        let previous = self.clone();
        if !self.is_valid() {
            *self = Self::default();
            return *self != previous;
        }
        for (active, observed) in self.candidates.iter_mut().zip(other.candidates) {
            *active &= observed;
        }
        self.trained = true;
        if !self.candidates.iter().any(|active| *active) {
            *self = Self::default();
        }
        *self != previous
    }

    /// Byte offsets proposed by surviving rules, sorted and deduplicated.
    /// Zero does not establish a real input-field boundary.
    pub(super) fn candidate_starts(&self, text: &str) -> Vec<usize> {
        if !self.is_valid() || !valid_prefix(text) {
            return Vec::new();
        }
        let mut starts: Vec<_> = self
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, active)| **active)
            .map(|(index, _)| candidate_start(index, text))
            .collect();
        starts.sort_unstable();
        starts.dedup();
        starts
    }

    pub(super) fn agreed_start(&self, text: &str) -> Option<usize> {
        if !self.is_trained() || !valid_prefix(text) {
            return None;
        }
        let mut starts = self
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, active)| **active)
            .map(|(index, _)| candidate_start(index, text));
        let start = starts.next()?;
        (start < text.len() && starts.all(|candidate| candidate == start)).then_some(start)
    }

    /// Learn only from an independently verified exact suffix deletion. Invalid
    /// or conflicting evidence invalidates this instance until its owner resets it.
    pub(super) fn learn(&mut self, before: &str, deleted_from: usize) -> bool {
        if !self.is_valid()
            || !valid_prefix(before)
            || deleted_from >= before.len()
            || !before.is_char_boundary(deleted_from)
        {
            self.invalidate();
            return false;
        }
        for (index, active) in self.candidates.iter_mut().enumerate() {
            *active &= candidate_start(index, before) == deleted_from;
        }
        self.trained = self.candidates.iter().any(|active| *active);
        self.trained
    }

    fn invalidate(&mut self) {
        self.candidates.fill(false);
        self.trained = false;
    }
}

fn candidate_start(index: usize, text: &str) -> usize {
    if !index.is_multiple_of(2) {
        let trimmed = text.trim_end_matches(char::is_whitespace);
        if trimmed.len() < text.len() {
            return trimmed.len();
        }
    }
    RULES[index / 2].start(text)
}

fn valid_prefix(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_PREFIX_BYTES && !text.chars().any(char::is_control)
}

impl Rule {
    fn start(self, text: &str) -> usize {
        let trimmed = text.trim_end_matches(char::is_whitespace);
        match self {
            Self::Whitespace => trimmed
                .char_indices()
                .rev()
                .find(|(_, ch)| ch.is_whitespace())
                .map_or(0, |(index, ch)| index + ch.len_utf8()),
            Self::CharacterRun => {
                let Some(last) = trimmed.chars().next_back() else {
                    return 0;
                };
                let alphanumeric = last.is_alphanumeric();
                trimmed
                    .char_indices()
                    .rev()
                    .take_while(|(_, ch)| {
                        !ch.is_whitespace() && ch.is_alphanumeric() == alphanumeric
                    })
                    .last()
                    .map_or(0, |(index, _)| index)
            }
            Self::AlphanumericWord => {
                let mut found_word = false;
                let mut start = 0;
                for (index, ch) in trimmed.char_indices().rev() {
                    if ch.is_alphanumeric() {
                        found_word = true;
                        start = index;
                    } else if found_word {
                        break;
                    }
                }
                start
            }
            Self::UnicodeWord => unicode_word_start(trimmed),
        }
    }
}

fn unicode_word_start(text: &str) -> usize {
    let token_start = text
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(0, |(index, ch)| index + ch.len_utf8());
    let mut start = token_start;
    let mut previous_punctuation = false;
    for (segment_start, segment) in text[token_start..].split_word_bound_indices() {
        let mut segment_class = None;
        for (index, ch) in segment.char_indices() {
            let punctuation = WORD_PUNCTUATION.contains(ch);
            if segment_class != Some(punctuation) {
                // Adjacent punctuation pieces form one run even across Unicode
                // word boundaries; other pieces retain their own boundaries.
                if !punctuation || !previous_punctuation {
                    start = token_start + segment_start + index;
                }
                segment_class = Some(punctuation);
            }
            previous_punctuation = punctuation;
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agreement_requires_observed_deletion() {
        let mut rules = WordRules::default();
        assert_eq!(rules.candidate_starts("hello world"), vec![6]);
        assert_eq!(rules.agreed_start("hello world"), None);
        assert!(rules.learn("hello world", 6));
        assert!(rules.is_trained());
        assert_eq!(rules.agreed_start("hello world"), Some(6));
        assert_eq!(rules.candidate_starts("hello world   "), vec![6, 11]);
        assert_eq!(rules.agreed_start("hello world   "), None);
    }

    #[test]
    fn paths_and_underscores_keep_distinct_hypotheses() {
        let mut rules = WordRules::default();
        assert!(rules.learn("hello world", 6));
        assert_eq!(rules.candidate_starts("x src/utils/foo.ts"), vec![2, 16]);
        assert_eq!(rules.agreed_start("x src/utils/foo.ts"), None);
        assert_eq!(rules.candidate_starts("x hello_world"), vec![2, 8]);
        assert_eq!(rules.agreed_start("x hello_world"), None);
        assert_eq!(rules.candidate_starts("x hello..."), vec![2, 7]);
    }

    #[test]
    fn trailing_whitespace_behavior_needs_its_own_observation() {
        let mut whole_word = WordRules::default();
        assert!(whole_word.learn("hello world", 6));
        assert_eq!(whole_word.agreed_start("hello world   "), None);
        assert!(whole_word.learn("hello world   ", 6));
        assert_eq!(whole_word.agreed_start("hello world   "), Some(6));

        let mut spaces = WordRules::default();
        assert!(spaces.learn("hello world", 6));
        assert!(spaces.learn("hello world   ", 11));
        assert_eq!(spaces.agreed_start("hello world   "), Some(11));
        assert_eq!(spaces.agreed_start("hello world"), Some(6));

        let mut unicode_spaces = WordRules::default();
        let before = "olé café\u{a0}\u{a0}";
        assert!(unicode_spaces.learn(before, "olé café".len()));
        assert_eq!(unicode_spaces.agreed_start(before), Some("olé café".len()));
    }

    #[test]
    fn each_word_family_keeps_both_trailing_whitespace_hypotheses() {
        for evidence in [
            vec![("x src/file.txt", 2)],
            vec![
                ("x src/file.txt", 11),
                ("x hello...", 7),
                ("x hello_world", 8),
            ],
            vec![("x src/file.txt", 11), ("x hello...", 2)],
            vec![
                ("x src/file.txt", 11),
                ("x hello...", 7),
                ("x hello_world", 2),
            ],
        ] {
            let mut rules = WordRules::default();
            for (before, offset) in evidence {
                assert!(rules.learn(before, offset));
            }
            assert_eq!(rules.candidates.iter().filter(|active| **active).count(), 2);
            assert_eq!(rules.agreed_start("x word   "), None);
            assert_eq!(rules.candidate_starts("x word   "), vec![2, 6]);
            assert!(rules.learn("x word   ", 6));
            assert_eq!(rules.agreed_start("x word   "), Some(6));
        }
    }

    #[test]
    fn serialized_rules_preserve_evidence_and_reject_invalid_states() {
        let mut trained = WordRules::default();
        assert!(trained.learn("x src/file.txt", 2));
        assert!(trained.learn("x hello   ", 7));
        for rules in [WordRules::default(), trained] {
            let json = serde_json::to_string(&rules).unwrap();
            let restored: WordRules = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, rules);
            assert!(restored.is_valid());
            assert_eq!(
                restored.agreed_start("x hello   "),
                rules.agreed_start("x hello   ")
            );
        }

        for rules in [
            WordRules {
                candidates: [false; CANDIDATE_COUNT],
                trained: true,
            },
            WordRules {
                candidates: [false; CANDIDATE_COUNT],
                trained: false,
            },
            WordRules {
                candidates: [true, false, true, true, true, true, true, true],
                trained: false,
            },
        ] {
            let json = serde_json::to_string(&rules).unwrap();
            let restored: WordRules = serde_json::from_str(&json).unwrap();
            assert!(!restored.is_valid());
            assert!(!restored.is_trained());
            assert_eq!(restored.agreed_start("hello world"), None);
            assert!(restored.candidate_starts("hello world").is_empty());
        }
        let malformed = serde_json::json!({ "candidates": [true], "trained": true });
        assert!(serde_json::from_value::<WordRules>(malformed).is_err());
    }

    #[test]
    fn profile_merge_intersects_evidence_and_resets_on_contradiction() {
        let mut profile = WordRules::default();
        let mut token = WordRules::default();
        assert!(token.learn("x src/file.txt", 2));
        assert!(profile.merge_observation(&token));
        assert_eq!(profile, token);
        assert!(!profile.merge_observation(&token));
        assert!(!profile.merge_observation(&WordRules::default()));

        let mut trailing = WordRules::default();
        assert!(trailing.learn("x word   ", 6));
        assert!(profile.merge_observation(&trailing));
        assert_eq!(profile.agreed_start("x src/file.txt   "), Some(14));

        let mut punctuation = WordRules::default();
        assert!(punctuation.learn("x src/file.txt", 11));
        assert!(profile.merge_observation(&punctuation));
        assert_eq!(profile, WordRules::default());
        assert!(profile.is_valid());
        assert!(!profile.is_trained());
    }

    #[test]
    fn invalid_profiles_never_become_trained_through_merge() {
        let mut invalid = WordRules::default();
        assert!(!invalid.learn("hello", 1));
        assert!(!invalid.is_valid());
        let mut profile = WordRules::default();
        assert!(!profile.merge_observation(&invalid));
        assert_eq!(profile, WordRules::default());

        let mut observed = WordRules::default();
        assert!(observed.learn("hello world", 6));
        assert!(invalid.merge_observation(&observed));
        assert_eq!(invalid, WordRules::default());
        assert!(!invalid.is_trained());
    }

    #[test]
    fn whitespace_evidence_narrows_without_aliasing_gestures() {
        let mut whitespace = WordRules::default();
        assert!(whitespace.learn("x src/utils/foo.ts", 2));
        assert_eq!(whitespace.agreed_start("x --flag=value"), Some(2));
        let mut punctuation = WordRules::default();
        assert!(punctuation.learn("x src/utils/foo.ts", 16));
        assert_eq!(punctuation.agreed_start("x --flag=value"), Some(9));
        assert_eq!(punctuation.agreed_start("x hello_world"), None);
    }

    #[test]
    fn punctuation_runs_and_skip_punctuation_can_be_distinguished() {
        let mut run = WordRules::default();
        assert!(run.learn("x src/file.txt", 11));
        assert!(run.learn("x hello...", 7));
        assert_eq!(run.agreed_start("x hello..."), Some(7));

        let mut skip = WordRules::default();
        assert!(skip.learn("x src/file.txt", 11));
        assert!(skip.learn("x hello...", 2));
        assert_eq!(skip.agreed_start("x hello..."), Some(2));
    }

    #[test]
    fn unicode_offsets_are_byte_boundaries() {
        let mut rules = WordRules::default();
        assert!(rules.learn("olé café", "olé ".len()));
        assert_eq!(rules.agreed_start("olé café"), Some("olé ".len()));
        assert_eq!(rules.candidate_starts("x\u{a0}café"), vec![3]);
        assert_eq!(rules.agreed_start("x\u{a0}café"), Some(3));
    }

    #[test]
    fn conflicting_or_invalid_evidence_cannot_retrain_implicitly() {
        let mut rules = WordRules::default();
        assert!(rules.learn("x src/file.txt", 2));
        assert!(!rules.learn("x src/file.txt", 11));
        assert!(!rules.is_trained());
        assert!(rules.candidate_starts("hello world").is_empty());
        assert_eq!(rules.agreed_start("hello world"), None);
        assert!(!rules.learn("hello world", 6));

        for (before, offset) in [("café", 4), ("hello", 5), ("", 0), ("a\nb", 2)] {
            let mut invalid = WordRules::default();
            assert!(!invalid.learn(before, offset));
            assert_eq!(invalid.agreed_start("hello world"), None);
        }
        let mut bounded = WordRules::default();
        assert!(!bounded.learn(&"x".repeat(MAX_PREFIX_BYTES + 1), 0));
    }

    #[test]
    fn zero_is_a_candidate_without_proving_the_input_start() {
        let mut rules = WordRules::default();
        assert!(rules.learn("hello world", 6));
        assert_eq!(rules.agreed_start("hello"), Some(0));
        assert_eq!(rules.agreed_start("   "), Some(0));
        assert_eq!(rules.agreed_start(""), None);
    }
}

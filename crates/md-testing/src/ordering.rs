use crate::parser::{OrderingDirective, SlotKind};

/// Describes an actual assistant message by the kinds of content it carries.
#[derive(Debug, Clone, PartialEq)]
pub struct ActualMessage {
    pub has_tool_calls: bool,
    pub has_text: bool,
}

/// Error produced when the expected assistant messages cannot be matched against
/// the actual assistant messages produced by the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum MatchError {
    /// The agent produced fewer assistant messages than the test case expected.
    TooFewMessages { expected: usize, actual: usize },
    /// The agent produced more assistant messages than the test case expected.
    TooManyMessages { expected: usize, actual: usize },
    /// The actual message at `actual_index` is incompatible with the slot's
    /// expected kind.
    KindMismatch {
        slot_index: usize,
        expected_kind: SlotKind,
        actual_index: usize,
    },
}

impl std::fmt::Display for MatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MatchError::TooFewMessages { expected, actual } => {
                write!(
                    f,
                    "Expected {} assistant message(s), but only {} were produced",
                    expected, actual
                )
            }
            MatchError::TooManyMessages { expected, actual } => {
                write!(
                    f,
                    "Expected {} assistant message(s), but {} were produced (trailing extras)",
                    expected, actual
                )
            }
            MatchError::KindMismatch {
                slot_index,
                expected_kind,
                actual_index,
            } => {
                write!(
                    f,
                    "Slot {} expected {:?} but actual message {} is incompatible",
                    slot_index, expected_kind, actual_index
                )
            }
        }
    }
}

impl std::error::Error for MatchError {}

/// Match typed expected slots against actual assistant messages.
///
/// # Semantics
///
/// Each expected slot carries a [`SlotKind`] and an [`OrderingDirective`].
/// The actual assistant messages form an ordered stream `A[0] … A[n-1]`.
/// The expected slots `S[0] … S[k-1]` must be matched monotonically with gaps
/// allowed between consecutive matches.
///
/// * For each slot except the last, the algorithm scans forward from the
///   current cursor and picks the first actual message whose content is
///   compatible with the slot kind.
/// * The last slot is anchored to the last actual message (`A[n-1]`).
///   If that message is incompatible, a [`MatchError::KindMismatch`] is
///   returned.
///
/// Compatibility:
/// * `SlotKind::ToolCall` matches `ActualMessage { has_tool_calls: true, .. }`
/// * `SlotKind::Text` matches `ActualMessage { has_text: true, .. }`
///
/// Returns `Ok(vec![m_0, …, m_{k-1}])` where each element is the index in the
/// actual stream that the corresponding expected slot matched.
pub fn match_slots(
    expected: &[(SlotKind, OrderingDirective)],
    actual: &[ActualMessage],
) -> Result<Vec<usize>, MatchError> {
    let expected_count = expected.len();
    let actual_count = actual.len();

    if expected_count == 0 {
        if actual_count == 0 {
            return Ok(Vec::new());
        }
        return Err(MatchError::TooManyMessages {
            expected: 0,
            actual: actual_count,
        });
    }

    if actual_count < expected_count {
        return Err(MatchError::TooFewMessages {
            expected: expected_count,
            actual: actual_count,
        });
    }

    // When all expected slots are Text (no ToolCall slots), use untyped
    // matching for backward compatibility: every actual message is considered
    // compatible regardless of its content flags.
    let all_text = expected.iter().all(|(k, _)| *k == SlotKind::Text);

    let mut mapping = Vec::with_capacity(expected_count);
    let mut cursor = 0usize;

    for (slot_idx, (kind, _)) in expected.iter().enumerate().take(expected_count - 1) {
        let found = (cursor..actual_count)
            .find(|&i| all_text || is_compatible(kind, &actual[i]));
        match found {
            Some(idx) => {
                mapping.push(idx);
                cursor = idx + 1;
            }
            None => {
                return Err(MatchError::TooFewMessages {
                    expected: expected_count,
                    actual: actual_count,
                });
            }
        }

        let remaining_slots = expected_count - slot_idx - 1;
        let remaining_actual = actual_count - cursor;
        if remaining_actual < remaining_slots {
            return Err(MatchError::TooFewMessages {
                expected: expected_count,
                actual: actual_count,
            });
        }
    }

    // Last slot is anchored to the last actual message.
    let last_slot_idx = expected_count - 1;
    let (last_kind, _) = &expected[last_slot_idx];
    let last_actual_idx = actual_count - 1;

    if !all_text && !is_compatible(last_kind, &actual[last_actual_idx]) {
        return Err(MatchError::KindMismatch {
            slot_index: last_slot_idx,
            expected_kind: last_kind.clone(),
            actual_index: last_actual_idx,
        });
    }

    mapping.push(last_actual_idx);

    Ok(mapping)
}

/// Check whether an actual message is compatible with a slot kind.
fn is_compatible(kind: &SlotKind, actual: &ActualMessage) -> bool {
    match kind {
        SlotKind::ToolCall => actual.has_tool_calls,
        SlotKind::Text => actual.has_text,
    }
}

/// Match expected assistant message slots against actual assistant messages.
///
/// This is a backward-compatible wrapper around [`match_slots`] that treats all
/// slots as `SlotKind::Text` and all actual messages as text-only.
///
/// # Semantics
///
/// Each expected slot is a `CHECK:` directive.  The actual assistant messages
/// form an ordered stream `A[0] … A[n-1]`.  The expected slots `S[0] … S[k-1]`
/// must be matched monotonically with gaps allowed between consecutive matches.
///
/// * `S[j]` is matched to some `A[m_j]` where `m_0 < m_1 < … < m_{k-1}`.
/// * `m_{k-1}` must equal `n-1` (the last actual message is consumed by the
///   last expected slot).  This enforces "no trailing extras".
/// * Therefore `n` must be at least `k`.  If `n > k`, the extra messages are
///   gaps between expected slots.
///
/// Returns `Ok(vec![m_0, …, m_{k-1}])` where each element is the index in the
/// actual stream that the corresponding expected slot matched.
pub fn match_assistant_slots(
    directives: &[OrderingDirective],
    actual_count: usize,
) -> Result<Vec<usize>, MatchError> {
    // Preserve single-slot strictness: a lone expected slot enforces 1:1
    // matching so that single-slot tests fail when the agent emits extras.
    if directives.len() == 1 && actual_count > 1 {
        return Err(MatchError::TooManyMessages {
            expected: 1,
            actual: actual_count,
        });
    }

    let expected: Vec<(SlotKind, OrderingDirective)> = directives
        .iter()
        .map(|d| (SlotKind::Text, d.clone()))
        .collect();
    let actual: Vec<ActualMessage> = (0..actual_count)
        .map(|_| ActualMessage {
            has_tool_calls: false,
            has_text: true,
        })
        .collect();
    match_slots(&expected, &actual)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match() {
        let directives = vec![OrderingDirective::Check, OrderingDirective::Check];
        let result = match_assistant_slots(&directives, 2).unwrap();
        assert_eq!(result, vec![0, 1]);
    }

    #[test]
    fn test_gap_tolerant() {
        // 3 expected, 5 actual: gaps at positions 1 and 3
        let directives = vec![
            OrderingDirective::Check,
            OrderingDirective::Check,
            OrderingDirective::Check,
        ];
        let result = match_assistant_slots(&directives, 5).unwrap();
        assert_eq!(result, vec![0, 1, 4]);
    }

    #[test]
    fn test_too_few() {
        let directives = vec![OrderingDirective::Check, OrderingDirective::Check];
        let err = match_assistant_slots(&directives, 1).unwrap_err();
        assert_eq!(
            err,
            MatchError::TooFewMessages {
                expected: 2,
                actual: 1
            }
        );
    }

    #[test]
    fn test_single_strict_no_gap() {
        // Single-slot tests enforce strict 1:1; extras are trailing failures.
        let directives = vec![OrderingDirective::Check];
        let err = match_assistant_slots(&directives, 3).unwrap_err();
        assert_eq!(
            err,
            MatchError::TooManyMessages {
                expected: 1,
                actual: 3
            }
        );
    }

    #[test]
    fn test_single_exact() {
        let directives = vec![OrderingDirective::Check];
        let result = match_assistant_slots(&directives, 1).unwrap();
        assert_eq!(result, vec![0]);
    }

    #[test]
    fn test_empty_expected() {
        let directives: Vec<OrderingDirective> = vec![];
        let result = match_assistant_slots(&directives, 0).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_empty_expected_with_actual() {
        let directives: Vec<OrderingDirective> = vec![];
        let err = match_assistant_slots(&directives, 1).unwrap_err();
        assert_eq!(
            err,
            MatchError::TooManyMessages {
                expected: 0,
                actual: 1
            }
        );
    }

    // --- Typed slot matching tests ---

    #[test]
    fn test_tool_call_matches_tool_message() {
        let expected = vec![(SlotKind::ToolCall, OrderingDirective::Check)];
        let actual = vec![
            ActualMessage {
                has_tool_calls: false,
                has_text: true,
            },
            ActualMessage {
                has_tool_calls: true,
                has_text: false,
            },
        ];
        let result = match_slots(&expected, &actual).unwrap();
        assert_eq!(result, vec![1]);
    }

    #[test]
    fn test_text_matches_text_message() {
        let expected = vec![(SlotKind::Text, OrderingDirective::Check)];
        let actual = vec![
            ActualMessage {
                has_tool_calls: true,
                has_text: false,
            },
            ActualMessage {
                has_tool_calls: false,
                has_text: true,
            },
        ];
        let result = match_slots(&expected, &actual).unwrap();
        assert_eq!(result, vec![1]);
    }

    #[test]
    fn test_mixed_slots_interleaved() {
        let expected = vec![
            (SlotKind::ToolCall, OrderingDirective::Check),
            (SlotKind::Text, OrderingDirective::Check),
        ];
        let actual = vec![
            ActualMessage {
                has_tool_calls: true,
                has_text: false,
            },
            ActualMessage {
                has_tool_calls: false,
                has_text: true,
            },
            ActualMessage {
                has_tool_calls: true,
                has_text: false,
            },
            ActualMessage {
                has_tool_calls: false,
                has_text: true,
            },
        ];
        let result = match_slots(&expected, &actual).unwrap();
        assert_eq!(result, vec![0, 3]);
    }

    #[test]
    fn test_kind_mismatch_last_slot() {
        let expected = vec![
            (SlotKind::ToolCall, OrderingDirective::Check),
            (SlotKind::Text, OrderingDirective::Check),
        ];
        let actual = vec![
            ActualMessage {
                has_tool_calls: true,
                has_text: false,
            },
            ActualMessage {
                has_tool_calls: true,
                has_text: false,
            },
        ];
        let err = match_slots(&expected, &actual).unwrap_err();
        assert!(matches!(err, MatchError::KindMismatch { .. }));
    }

    #[test]
    fn test_gap_tolerance_typed() {
        let expected = vec![
            (SlotKind::ToolCall, OrderingDirective::Check),
            (SlotKind::ToolCall, OrderingDirective::Check),
            (SlotKind::Text, OrderingDirective::Check),
        ];
        let actual = vec![
            ActualMessage {
                has_tool_calls: true,
                has_text: false,
            },
            ActualMessage {
                has_tool_calls: false,
                has_text: true,
            },
            ActualMessage {
                has_tool_calls: true,
                has_text: false,
            },
            ActualMessage {
                has_tool_calls: false,
                has_text: true,
            },
            ActualMessage {
                has_tool_calls: false,
                has_text: true,
            },
        ];
        let result = match_slots(&expected, &actual).unwrap();
        assert_eq!(result, vec![0, 2, 4]);
    }

    #[test]
    fn test_backward_compat_all_text() {
        let expected = vec![
            (SlotKind::Text, OrderingDirective::Check),
            (SlotKind::Text, OrderingDirective::Check),
        ];
        let actual = vec![
            ActualMessage {
                has_tool_calls: false,
                has_text: true,
            },
            ActualMessage {
                has_tool_calls: false,
                has_text: true,
            },
        ];
        let result = match_slots(&expected, &actual).unwrap();
        assert_eq!(result, vec![0, 1]);
    }

    #[test]
    fn test_both_kinds_matches_either() {
        let expected = vec![(SlotKind::ToolCall, OrderingDirective::Check)];
        let actual = vec![ActualMessage {
            has_tool_calls: true,
            has_text: true,
        }];
        let result = match_slots(&expected, &actual).unwrap();
        assert_eq!(result, vec![0]);
    }
}

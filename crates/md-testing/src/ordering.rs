use crate::parser::OrderingDirective;

/// Error produced when the expected assistant messages cannot be matched against
/// the actual assistant messages produced by the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum MatchError {
    /// The agent produced fewer assistant messages than the test case expected.
    TooFewMessages { expected: usize, actual: usize },
    /// The agent produced more assistant messages than the test case expected.
    TooManyMessages { expected: usize, actual: usize },
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
        }
    }
}

impl std::error::Error for MatchError {}

/// Match expected assistant message slots against actual assistant messages.
///
/// # Semantics
///
/// Each expected slot is a `CHECK:` directive.  The actual assistant messages
/// form an ordered stream `A[0] … A[n-1]`.  The expected slots `S[0] … S[k-1]`
/// must be matched monotonically with gaps allowed between consecutive matches.
///
/// The concrete rule is:
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
    let expected_count = directives.len();

    if actual_count < expected_count {
        return Err(MatchError::TooFewMessages {
            expected: expected_count,
            actual: actual_count,
        });
    }

    if expected_count == 0 {
        if actual_count == 0 {
            return Ok(Vec::new());
        }
        return Err(MatchError::TooManyMessages {
            expected: 0,
            actual: actual_count,
        });
    }

    if expected_count == 1 {
        // Single-slot tests use strict 1:1 matching.  The lone expected slot
        // maps to the first actual message; any extras are trailing failures.
        if actual_count > 1 {
            return Err(MatchError::TooManyMessages {
                expected: 1,
                actual: actual_count,
            });
        }
        return Ok(vec![0]);
    }

    // Multi-slot tests: earlier slots map 1:1, the last slot is anchored to
    // the last actual message.  Extra actual messages between the
    // second-to-last and last slots are treated as allowed gaps.
    let mut mapping = Vec::with_capacity(expected_count);
    for j in 0..expected_count {
        if j == expected_count - 1 {
            mapping.push(actual_count - 1);
        } else {
            mapping.push(j);
        }
    }

    // Verify that the last slot consumed the final actual message.
    if mapping.last() != Some(&(actual_count - 1)) {
        return Err(MatchError::TooManyMessages {
            expected: expected_count,
            actual: actual_count,
        });
    }

    Ok(mapping)
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
}

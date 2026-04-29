use crate::results::{AttributionConfidence, FailureAttribution, VoteDistribution};

pub struct AttributionSignals {
    pub cel_failed: Option<String>,
    pub structural_failure: Option<String>,
    pub evaluator_failed: bool,
    pub vote_distribution: Option<VoteDistribution>,
}

/// Compute failure attribution from collected signals.
/// Returns `None` if the assertion passed.
pub fn attribute(passed: bool, signals: &AttributionSignals) -> Option<FailureAttribution> {
    if passed {
        return None;
    }

    // Step 1: CEL / Structural — definitive subject failures
    if let Some(evidence) = &signals.cel_failed {
        return Some(FailureAttribution::SubjectFailure {
            confidence: AttributionConfidence::Definitive,
            evidence: evidence.clone(),
        });
    }

    if let Some(evidence) = &signals.structural_failure {
        return Some(FailureAttribution::SubjectFailure {
            confidence: AttributionConfidence::Definitive,
            evidence: evidence.clone(),
        });
    }

    // Step 2: Judge infrastructure failure
    if signals.evaluator_failed {
        return Some(FailureAttribution::JudgePipelineFailure {
            confidence: AttributionConfidence::Definitive,
            evidence: "Evaluator LLM calls failed".to_string(),
        });
    }

    // Step 3: Vote distribution analysis
    if let Some(dist) = &signals.vote_distribution {
        if dist.total_votes == 0 {
            return Some(FailureAttribution::JudgePipelineFailure {
                confidence: AttributionConfidence::Definitive,
                evidence: format!("All {} evaluator attempts failed", dist.error_votes),
            });
        }

        if dist.total_votes >= 3 {
            if dist.pass_votes == 0 {
                // Unanimous FAIL — check if reasons converge
                let unique_reasons = count_distinct_fail_reasons(&dist.votes);
                if unique_reasons <= 1 {
                    return Some(FailureAttribution::SubjectFailure {
                        confidence: AttributionConfidence::High,
                        evidence: format!(
                            "Unanimous FAIL ({}/{} votes), consistent reasoning",
                            dist.fail_votes, dist.total_votes
                        ),
                    });
                } else {
                    return Some(FailureAttribution::AssertionDefect {
                        confidence: AttributionConfidence::Moderate,
                        evidence: format!(
                            "Unanimous FAIL ({}/{} votes) but {} distinct failure reasons — assertion may be underspecified",
                            dist.fail_votes, dist.total_votes, unique_reasons
                        ),
                    });
                }
            }

            // Split vote — assertion ambiguity
            return Some(FailureAttribution::AssertionDefect {
                confidence: AttributionConfidence::High,
                evidence: format!(
                    "Split vote: {}/{} PASS, {}/{} FAIL — assertion produces inconsistent verdicts",
                    dist.pass_votes, dist.total_votes, dist.fail_votes, dist.total_votes
                ),
            });
        }

        // Fewer than 3 votes — insufficient data for confident attribution
        return Some(FailureAttribution::SubjectFailure {
            confidence: AttributionConfidence::Low,
            evidence: format!(
                "Only {} vote(s) — insufficient for attribution analysis",
                dist.total_votes
            ),
        });
    }

    Some(FailureAttribution::SubjectFailure {
        confidence: AttributionConfidence::Low,
        evidence: "No diagnostic signals available".to_string(),
    })
}

fn count_distinct_fail_reasons(votes: &[crate::results::Vote]) -> usize {
    let fail_reasons: Vec<&str> = votes
        .iter()
        .filter(|v| !v.passed)
        .map(|v| v.reasoning.as_str())
        .collect();

    if fail_reasons.len() <= 1 {
        return fail_reasons.len();
    }

    // Simple heuristic: count reasons that share fewer than 30% of words
    // with any previously seen reason as "distinct".
    let mut distinct = vec![fail_reasons[0]];
    for reason in &fail_reasons[1..] {
        let is_similar = distinct.iter().any(|existing| {
            let similarity = word_overlap(existing, reason);
            similarity > 0.3
        });
        if !is_similar {
            distinct.push(reason);
        }
    }
    distinct.len()
}

fn word_overlap(a: &str, b: &str) -> f64 {
    let words_a: std::collections::HashSet<&str> =
        a.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric())).collect();
    let words_b: std::collections::HashSet<&str> =
        b.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric())).collect();

    if words_a.is_empty() || words_b.is_empty() {
        return 0.0;
    }

    let intersection = words_a.intersection(&words_b).count();
    let min_size = words_a.len().min(words_b.len());
    intersection as f64 / min_size as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::Vote;

    #[test]
    fn passed_returns_none() {
        let signals = AttributionSignals {
            cel_failed: None,
            structural_failure: None,
            evaluator_failed: false,
            vote_distribution: None,
        };
        assert!(attribute(true, &signals).is_none());
    }

    #[test]
    fn cel_failure_is_definitive_subject() {
        let signals = AttributionSignals {
            cel_failed: Some("message.text.contains(\"expected\") evaluated to false".to_string()),
            structural_failure: None,
            evaluator_failed: false,
            vote_distribution: None,
        };
        match attribute(false, &signals).unwrap() {
            FailureAttribution::SubjectFailure { confidence, .. } => {
                assert!(matches!(confidence, AttributionConfidence::Definitive));
            }
            other => panic!("Expected SubjectFailure, got {:?}", other),
        }
    }

    #[test]
    fn structural_failure_is_definitive_subject() {
        let signals = AttributionSignals {
            cel_failed: None,
            structural_failure: Some("TooFewMessages: expected 2, got 1".to_string()),
            evaluator_failed: false,
            vote_distribution: None,
        };
        match attribute(false, &signals).unwrap() {
            FailureAttribution::SubjectFailure { confidence, .. } => {
                assert!(matches!(confidence, AttributionConfidence::Definitive));
            }
            other => panic!("Expected SubjectFailure, got {:?}", other),
        }
    }

    #[test]
    fn evaluator_failure_is_definitive_judge() {
        let signals = AttributionSignals {
            cel_failed: None,
            structural_failure: None,
            evaluator_failed: true,
            vote_distribution: None,
        };
        match attribute(false, &signals).unwrap() {
            FailureAttribution::JudgePipelineFailure { confidence, .. } => {
                assert!(matches!(confidence, AttributionConfidence::Definitive));
            }
            other => panic!("Expected JudgePipelineFailure, got {:?}", other),
        }
    }

    #[test]
    fn unanimous_fail_consistent_reasoning_is_high_subject() {
        let signals = AttributionSignals {
            cel_failed: None,
            structural_failure: None,
            evaluator_failed: false,
            vote_distribution: Some(VoteDistribution {
                total_votes: 3,
                pass_votes: 0,
                fail_votes: 3,
                error_votes: 0,
                votes: vec![
                    Vote { passed: false, reasoning: "The response does not contain the expected file list".to_string(), duration_ms: 100 },
                    Vote { passed: false, reasoning: "The response is missing the expected file list output".to_string(), duration_ms: 100 },
                    Vote { passed: false, reasoning: "Response does not include the file list as required".to_string(), duration_ms: 100 },
                ],
            }),
        };
        match attribute(false, &signals).unwrap() {
            FailureAttribution::SubjectFailure { confidence, .. } => {
                assert!(matches!(confidence, AttributionConfidence::High));
            }
            other => panic!("Expected SubjectFailure, got {:?}", other),
        }
    }

    #[test]
    fn split_vote_is_assertion_defect() {
        let signals = AttributionSignals {
            cel_failed: None,
            structural_failure: None,
            evaluator_failed: false,
            vote_distribution: Some(VoteDistribution {
                total_votes: 3,
                pass_votes: 1,
                fail_votes: 2,
                error_votes: 0,
                votes: vec![
                    Vote { passed: true, reasoning: "The response addresses the request".to_string(), duration_ms: 100 },
                    Vote { passed: false, reasoning: "Missing specific details".to_string(), duration_ms: 100 },
                    Vote { passed: false, reasoning: "Not enough information".to_string(), duration_ms: 100 },
                ],
            }),
        };
        match attribute(false, &signals).unwrap() {
            FailureAttribution::AssertionDefect { confidence, .. } => {
                assert!(matches!(confidence, AttributionConfidence::High));
            }
            other => panic!("Expected AssertionDefect, got {:?}", other),
        }
    }

    #[test]
    fn single_vote_is_low_confidence_subject() {
        let signals = AttributionSignals {
            cel_failed: None,
            structural_failure: None,
            evaluator_failed: false,
            vote_distribution: Some(VoteDistribution {
                total_votes: 1,
                pass_votes: 0,
                fail_votes: 1,
                error_votes: 0,
                votes: vec![
                    Vote { passed: false, reasoning: "Failed".to_string(), duration_ms: 100 },
                ],
            }),
        };
        match attribute(false, &signals).unwrap() {
            FailureAttribution::SubjectFailure { confidence, .. } => {
                assert!(matches!(confidence, AttributionConfidence::Low));
            }
            other => panic!("Expected SubjectFailure with Low confidence, got {:?}", other),
        }
    }

    #[test]
    fn all_errors_is_judge_failure() {
        let signals = AttributionSignals {
            cel_failed: None,
            structural_failure: None,
            evaluator_failed: false,
            vote_distribution: Some(VoteDistribution {
                total_votes: 0,
                pass_votes: 0,
                fail_votes: 0,
                error_votes: 3,
                votes: vec![],
            }),
        };
        match attribute(false, &signals).unwrap() {
            FailureAttribution::JudgePipelineFailure { confidence, .. } => {
                assert!(matches!(confidence, AttributionConfidence::Definitive));
            }
            other => panic!("Expected JudgePipelineFailure, got {:?}", other),
        }
    }
}

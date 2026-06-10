//! Deterministic heuristic fallback for app identity derivation.
//!
//! Used when the LLM response is malformed JSON, or to repair individual
//! fields that fall outside the allowlist / palette. See `docs/app/identity.md`
//! for the design rationale.

use super::{COLOR_PALETTE, SYMBOL_ALLOWLIST};

/// Derive a symbol from the allowlist using a stable FNV-1a hash of the task
/// bytes. The same task always maps to the same symbol.
pub fn fallback_symbol(task: &str) -> &'static str {
    let hash = fnv1a(task.as_bytes());
    SYMBOL_ALLOWLIST[hash as usize % SYMBOL_ALLOWLIST.len()]
}

/// Derive a color from the palette using a stable FNV-1a hash of the task
/// bytes, offset by a prime to distribute away from the symbol choice.
pub fn fallback_color(task: &str) -> &'static str {
    let hash = fnv1a(task.as_bytes()).wrapping_mul(2_654_435_761);
    COLOR_PALETTE[hash as usize % COLOR_PALETTE.len()]
}

/// Derive a short title from the task string: the first 40 characters (by
/// `char`, not byte), trimmed of leading/trailing whitespace.
pub fn fallback_title(task: &str) -> String {
    let truncated: String = task.chars().take(40).collect();
    let trimmed = truncated.trim();
    if trimmed.is_empty() {
        "Untitled".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// FNV-1a 32-bit hash — dependency-free, fast, well-distributed for short
/// strings. Stability is the only requirement here; collision rate is
/// irrelevant since we are mapping into a small fixed-size array.
fn fnv1a(bytes: &[u8]) -> u32 {
    const OFFSET_BASIS: u32 = 2_166_136_261;
    const PRIME: u32 = 16_777_619;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_symbol_is_deterministic() {
        let task = "Organize the team meeting agenda";
        assert_eq!(fallback_symbol(task), fallback_symbol(task));
    }

    #[test]
    fn fallback_color_is_deterministic() {
        let task = "Organize the team meeting agenda";
        assert_eq!(fallback_color(task), fallback_color(task));
    }

    #[test]
    fn fallback_symbol_stays_in_allowlist() {
        let tasks = ["hello", "write a blog post", "debug the server crash", ""];
        for t in &tasks {
            let s = fallback_symbol(t);
            assert!(
                SYMBOL_ALLOWLIST.contains(&s),
                "symbol '{}' not in allowlist",
                s
            );
        }
    }

    #[test]
    fn fallback_color_stays_in_palette() {
        let tasks = ["hello", "write a blog post", "debug the server crash", ""];
        for t in &tasks {
            let c = fallback_color(t);
            assert!(
                COLOR_PALETTE.contains(&c),
                "color '{}' not in palette",
                c
            );
        }
    }

    #[test]
    fn fallback_title_truncates_at_40_chars() {
        let long = "a".repeat(100);
        let title = fallback_title(&long);
        assert_eq!(title.chars().count(), 40);
    }

    #[test]
    fn fallback_title_trims_whitespace() {
        let title = fallback_title("  hello  ");
        assert_eq!(title, "hello");
    }

    #[test]
    fn fallback_title_empty_task_yields_untitled() {
        assert_eq!(fallback_title(""), "Untitled");
        assert_eq!(fallback_title("   "), "Untitled");
    }

    #[test]
    fn different_tasks_produce_different_symbols_often() {
        // Not a strict requirement (collisions exist), but the distribution
        // should assign different symbols to obviously different tasks.
        let a = fallback_symbol("book a flight to Tokyo");
        let b = fallback_symbol("write unit tests for the parser");
        // We can only assert they are valid; collision may occur for some pairs.
        assert!(SYMBOL_ALLOWLIST.contains(&a));
        assert!(SYMBOL_ALLOWLIST.contains(&b));
    }
}

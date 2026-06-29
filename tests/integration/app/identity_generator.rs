//! LIVE (human-gated): the App identity generator (`src/app/identity`) returns
//! an allowlisted SF Symbol and a palette color from a real model call.
//!
//! `derive_identity` is infallible — it repairs any out-of-allowlist field via
//! the heuristic fallback — so even a degenerate model response yields a valid
//! identity. The contract this test pins is therefore the *invariant the rest
//! of the board relies on*: whatever comes back, the symbol is in
//! [`SYMBOL_ALLOWLIST`] and the color is in [`COLOR_PALETTE`], and the title is
//! non-empty within the documented length bound. Per the mock-data policy
//! (root `CLAUDE.md`), the call is real, not mocked, so this is the human-run
//! suite and skips cleanly without `RUBBERDUX_LLM_API_KEY`.
//!
//! See `docs/app/identity.md`.

use rubberdux::app::identity::{COLOR_PALETTE, SYMBOL_ALLOWLIST, derive_identity};
use rubberdux::provider::selected_from_env;

use crate::support::live_gate::skip_without_live_llm;

#[tokio::test(flavor = "multi_thread")]
async fn live_identity_is_allowlisted_symbol_and_color() {
    if skip_without_live_llm("live_identity_is_allowlisted_symbol_and_color") {
        return;
    }

    let client = selected_from_env().expect("build provider from env");

    // A few distinct tasks across domains: each must yield an in-allowlist
    // symbol and an in-palette color, whether the LLM picked them or the
    // per-field repair did.
    let tasks = [
        "Plan a two-week vacation itinerary to Japan",
        "Refactor the payment processing module and add tests",
        "Draft and send the quarterly investor update email",
    ];

    for task in tasks {
        let identity = derive_identity(&*client, task).await;

        assert!(
            SYMBOL_ALLOWLIST.contains(&identity.icon.symbol.as_str()),
            "symbol `{}` for task `{task}` must be in the allowlist",
            identity.icon.symbol
        );
        assert!(
            COLOR_PALETTE.contains(&identity.icon.color.as_str()),
            "color `{}` for task `{task}` must be in the palette",
            identity.icon.color
        );
        assert!(
            !identity.title.trim().is_empty(),
            "title for task `{task}` must be non-empty"
        );
        assert!(
            identity.title.chars().count() <= 40,
            "title `{}` for task `{task}` must respect the 40-char bound",
            identity.title
        );
    }
}

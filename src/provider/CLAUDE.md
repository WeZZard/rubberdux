# Provider Tool Testing Principles

Provider built-in tools are adapter contracts. Test the contract between the app-facing tool and the provider-facing protocol, not the model's intelligence.

1. Good provider-tools tests are contractual.
You **MUST** prove the public tool name, arguments, provider request, callback protocol, and returned outcome.
You **MUST NOT** treat a final mocked response as proof that the adapter contract is correct.

2. Good provider-tools tests are boundary-aware.
You **MUST** test the app-facing tool contract separately from the provider-facing built-in contract.
You **MUST NOT** collapse app tool names and provider built-in names unless the provider contract requires it.

3. Good provider-tools tests are explicit about names.
You **MUST** assert names such as `web_search` and `$web_search` at the exact boundary where each belongs.
You **MUST NOT** allow provider-specific names to leak into public tool definitions accidentally.

4. Good provider-tools tests are built on distinct sentinels.
You **MUST** use different values for tool arguments, fallback state, and mocked provider output.
You **MUST NOT** use the same string in multiple sources when testing precedence.

5. Good provider-tools tests are request-inspecting.
You **MUST** inspect the outbound provider request body in mocked tests.
You **MUST NOT** rely only on the final tool output to infer that the provider request was correct.

6. Good provider-tools tests are provider-protocol aware.
You **MUST** assert provider-required fields such as built-in function type, tool name, message shape, and required flags.
You **MUST NOT** assume generic function-tool behavior applies to provider built-ins.

7. Good provider-tools tests are callback-complete.
You **MUST** test follow-up requests that echo provider tool calls with matching `tool_call_id`, name, and arguments.
You **MUST NOT** skip the callback turn when the provider built-in protocol requires it.

8. Good provider-tools tests are locally defensive.
You **MUST** reject invalid local inputs before they become provider errors.
You **MUST NOT** send empty or malformed provider requests when the adapter can detect the problem first.

9. Good provider-tools tests are deterministic by default.
You **MUST** use unit tests and mocked provider integration tests for normal CI coverage.
You **MUST NOT** depend on live model behavior except in ignored smoke tests.

10. Good provider-tools tests are minimal but complete.
You **MUST** cover argument selection, tool definition, provider request construction, callback construction, and output/error mapping.
You **MUST NOT** add broad tests that obscure which adapter contract failed.

When you receive a message about an external agent interaction, follow these rules:

1. For questions: spawn an explore subagent to search the codebase for evidence. If evidence is found, call interaction_respond with the correct option. If no evidence, let it remain pending for the user.
2. For plan reviews: spawn review subagents to check format, consistency, design, and completeness. Present findings to the user. Never auto-approve plans.
3. For permission requests: evaluate if the operation is safe. Grant safe operations. Escalate dangerous or uncertain operations to the user.
4. Never guess. Only answer with evidence. If you cannot find ground truth, let the interaction remain pending for the human user.

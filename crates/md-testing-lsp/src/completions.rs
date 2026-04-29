use tower_lsp::lsp_types::*;

pub fn cel_completions() -> Vec<CompletionItem> {
    let mut items = Vec::new();

    // Message context variables
    items.push(variable_completion(
        "message.text",
        "string",
        "The assistant's text content",
    ));
    items.push(variable_completion(
        "message.tool_calls",
        "list",
        "Tool calls made in this message",
    ));
    items.push(variable_completion(
        "message.reasoning",
        "string",
        "Chain-of-thought reasoning (if present)",
    ));
    items.push(variable_completion(
        "message.reaction",
        "string",
        "Emoji reaction sent (telegram-channel)",
    ));
    items.push(variable_completion(
        "message.reply_to",
        "int",
        "Index of the message being replied to (telegram-channel)",
    ));
    items.push(variable_completion(
        "message.format",
        "string",
        "Message format: text, photo, sticker, etc. (telegram-channel)",
    ));
    items.push(variable_completion(
        "message.inline_keyboard",
        "list",
        "Inline keyboard buttons (telegram-channel)",
    ));

    // Trajectory context variables
    items.push(variable_completion(
        "trajectory.assistant_count",
        "int",
        "Number of assistant messages",
    ));
    items.push(variable_completion(
        "trajectory.user_count",
        "int",
        "Number of user messages",
    ));
    items.push(variable_completion(
        "trajectory.tool_call_count",
        "int",
        "Total tool calls across all messages",
    ));

    // CEL string functions
    items.push(function_completion(
        "contains",
        "string.contains(substring)",
        "Check if string contains substring",
    ));
    items.push(function_completion(
        "startsWith",
        "string.startsWith(prefix)",
        "Check if string starts with prefix",
    ));
    items.push(function_completion(
        "endsWith",
        "string.endsWith(suffix)",
        "Check if string ends with suffix",
    ));
    items.push(function_completion(
        "matches",
        "string.matches(regex)",
        "Check if string matches RE2 regex pattern",
    ));
    items.push(function_completion(
        "size",
        "collection.size()",
        "Get the size of a string, list, or map",
    ));

    // CEL list functions
    items.push(function_completion(
        "exists",
        "list.exists(x, predicate)",
        "True if any element satisfies the predicate",
    ));
    items.push(function_completion(
        "all",
        "list.all(x, predicate)",
        "True if all elements satisfy the predicate",
    ));
    items.push(function_completion(
        "filter",
        "list.filter(x, predicate)",
        "Return elements that satisfy the predicate",
    ));
    items.push(function_completion(
        "map",
        "list.map(x, expression)",
        "Transform each element",
    ));

    // CEL macros
    items.push(function_completion(
        "has",
        "has(field)",
        "Check if a field or key exists",
    ));

    items
}

fn variable_completion(label: &str, type_name: &str, doc: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::VARIABLE),
        detail: Some(type_name.to_string()),
        documentation: Some(Documentation::String(doc.to_string())),
        insert_text: Some(label.to_string()),
        ..Default::default()
    }
}

fn function_completion(label: &str, signature: &str, doc: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::FUNCTION),
        detail: Some(signature.to_string()),
        documentation: Some(Documentation::String(doc.to_string())),
        insert_text: Some(label.to_string()),
        ..Default::default()
    }
}

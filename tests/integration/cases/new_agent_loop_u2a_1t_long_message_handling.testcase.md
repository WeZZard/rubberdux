---
target: agent-loop
---

## Storyline
<!-- The agent should handle a very long user message without truncation -->
<!-- The agent should address all parts of the long message -->

## User Message
Please summarize the following long note into five concise bullets. The summary must cover every named topic and should not skip the final async section.

I am preparing onboarding material for developers who know another systems language but are new to Rust. The note starts with memory safety and explains that Rust tries to prevent use-after-free, double-free, null pointer mistakes, and data races before a program runs. It emphasizes that this safety does not depend on a garbage collector. Instead, the compiler tracks how values are owned, borrowed, and dropped, which lets many bugs become compile-time errors rather than production incidents.

The next part focuses on ownership. Each value has one owner, ownership can move when values are assigned or passed to functions, and a value is dropped when its owner goes out of scope. Borrowing then lets code access data without taking ownership. Shared references allow reading, mutable references allow mutation, and Rust prevents simultaneous mutable access with other active references. Lifetimes describe how long references remain valid, usually inferred by the compiler, but sometimes written explicitly when structs, functions, or trait bounds connect multiple borrowed values.

The note then introduces traits and generics. Traits define shared behavior, support trait bounds, and make polymorphism possible through static dispatch or trait objects. Generics let functions, structs, and enums reuse logic across types while preserving performance through monomorphization. Finally, the async/await section explains that async functions return futures, `.await` yields until work is ready, executors drive those futures, and developers should avoid holding blocking locks or large borrowed state across await points.

## Assistant Message

```cel
message.text.contains("async") && message.text.contains("ownership")
```

<!-- The assistant should provide a coherent response covering all topics without truncation -->

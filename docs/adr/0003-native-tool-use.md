# Native tool_use blocks instead of a text tool protocol

Local-model agents commonly invent a text protocol for Tool Calls (e.g. `tool: name({json})` parsed by regex) because model tool-template support used to be unreliable. We use native Anthropic `tool_use`/`tool_result` blocks instead: local inference servers now apply the model's own trained tool-call template (grammar-constrained where the runtime supports it), which is strictly more reliable for small models than teaching them a homemade syntax in the system prompt. This removes the text parser, the format-teaching prompt sections (~40% of the system prompt), and the parse-failure class we hit repeatedly in testing (positional args, code fences, truncated JSON).

Trade-off accepted: a model whose chat template lacks tool support degrades hard. We control which models we run; if one matters enough, a text-protocol fallback can be reintroduced behind the same Tool contract.

# FFF.nvim repository guidance

FFF is a file finder, not a completion plugin. Rust owns performance-critical indexing/scoring/watch state; Lua owns Neovim integration and UI.

## Vis-native execution

- Use Vis native tools: `grep` first when location is unknown, then structural inspection and focused reads. Use native formatting/lint/test tools when supported rather than shell replicas.
- For Lua/Rust behavior, reproduce with the smallest executable test. Preserve unrelated work and stop only resources you started.

## Compatibility

- Top-level Rust, C, Lua, and Bun APIs must not break.
- Public Lua functions require type annotations and `vim.validate()` input validation.
- Prefer private Rust structs/functions. Do not make an item public unless required.

## Architecture and locking

- Keep performance-sensitive work in Rust and Neovim-specific behavior in Lua.
- Prefer Rust methods over free functions. If a file accumulates more than two distinct impl blocks, split it.
- Avoid long-held mutex/RwLock guards. If a design requires one, reassess before implementing.
- Utility functions belong at the end of the file.

## UI

- Preserve navigation, selection, and preview behavior when adding features.
- Test every UI change with both `prompt_position="top"` and `prompt_position="bottom"`; layout order differs while navigation semantics must remain identical.
- New highlights, shortcuts, or configurable UI options require config and README updates.
- For substantial UI work, use an executable Neovim test against a real codebase.

## Style and verification

- No module/top-of-file comments. Private Rust items get no doc comments. Keep comments to one or two concise lines unless complexity truly requires more.
- Prefer Make targets: `make format`, `make lint`, `make test`, `make build`.
- There is limited traditional coverage: add inline Rust unit tests for isolated logic or executable Neovim tests (`nvim -l <file>`) for integration/UI behavior.

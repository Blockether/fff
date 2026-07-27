# FFF.nvim repository guidance

FFF is a file finder, not a completion plugin. Rust owns indexing, scoring, and watch state; Lua owns Neovim integration and UI.

## Contracts

- Do not break top-level Rust, C, Lua, or Bun APIs.
- Public Lua functions require type annotations and `vim.validate()`.
- Keep performance-sensitive work in Rust and Neovim-specific behavior in Lua.
- Avoid long-held mutex/RwLock guards.

## UI

- Preserve navigation, selection, and preview behavior.
- Test UI changes with both `prompt_position="top"` and `prompt_position="bottom"`.
- Use executable Neovim tests for substantial UI behavior.

## Style and verification

No module/top-of-file comments. Prefer private Rust items. Run `make format`, `make lint`, `make test`, and `make build` as relevant.

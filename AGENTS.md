# FFF.nvim repository guidance

FFF is a file finder, not a completion plugin. Rust owns indexing, scoring, and watch state; Lua owns Neovim integration and UI.

## Public contracts

- Preserve top-level Rust, C, Lua, and Bun APIs. Public Lua functions require type annotations and `vim.validate()`.
- `fff-core::list_directory` / `fff_list_directory` list one directory with ignore rules and no index, so they work in a filesystem root or home dir where the indexed picker refuses. Keep them stateless and bounded-depth.
- Keep performance-sensitive work in Rust and Neovim-specific behavior in Lua. Avoid long-held mutex/RwLock guards.
- Regenerate JavaScript API artifacts from source; never hand-edit generated output.

## UI behavior

- Preserve navigation, selection, and preview semantics. Test UI changes with `prompt_position="top"` and `prompt_position="bottom"`.
- Use executable Neovim tests for substantial UI behavior.

## Style and verification

- Do not add module or file-header comments; prefer private Rust items.
- Run the smallest relevant Make targets while iterating and `make format`, `make lint`, `make test`, and `make build` before finishing.

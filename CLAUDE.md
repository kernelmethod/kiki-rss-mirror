# CLAUDE.md

## Build, Lint, and Test Commands

```bash
# Build commands
cargo build             # Create a debug build
cargo build --release   # Build in release mode

# Formatting and linting commands
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings

# Testing commands
cargo test
```

## Codebase Organization

Modules in the codebase are structured as follows:

- `src/` - Main source code directory
  - `main.rs` - Entry point of the application
  - `db/` - Database-related code
  - `routes/` - HTTP route handlers organized by API version
  - `cli/` - Command-line interface commands
  - `test/` - Test modules and test data

## Code Style Guidelines

### Error Handling
- Use `thiserror` to create custom error types
- Use `anyhow` for propagating errors in main functions
- Return `Result<T, E>` from functions that can fail
- Handle errors gracefully, don't panic unless absolutely necessary
- Log errors appropriately using the `tracing` crate

### Database Access
- Reach the database only through `db::Db`: `read`/`write` from async code, and `read_blocking`/`write_blocking` from synchronous code or when the closure borrows from the caller
- Use `write` for anything that modifies the database; read connections are `query_only` and reject writes
- Don't call back into `Db` from inside a closure passed to it: the nested checkout is refused with `DbError::Nested`, since waiting for it could deadlock

### Documentation
- Document all public functions with doc comments (`///`) using `rustdoc`-style documentation
- Include examples for public APIs
- Document error cases and return values


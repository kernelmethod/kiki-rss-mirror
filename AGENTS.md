# AGENTS.md

## Build, Lint, and Test Commands

```bash
# Build commands
cargo build             # Create a debug build
cargo build --release   # Build in release mode

# Linting commands
cargo clippy

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

### Documentation
- Document all public functions with doc comments (`///`) using `rustdoc`-style documentation
- Include examples for public APIs
- Document error cases and return values

### Conventions
- Use `tokio` for asynchronous programming
- Follow async/await patterns for concurrent code
- Use `axum` for HTTP routing and handlers
- Use `serde` for serialization/deserialization
- Use `r2d2` and `rusqlite` for database operations
- Use `chrono` for time handling
- Use `tracing` for logging and structured tracing

### Testing
- Write tests for all public APIs
- Use integration tests for HTTP endpoints
- Use unit tests for business logic
- Use `tempdir` crate for temporary file handling in tests
- Ensure all tests pass before committing

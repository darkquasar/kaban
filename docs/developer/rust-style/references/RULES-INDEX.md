## err
- `references/rules/err-anyhow-app.md`: Use `anyhow` for application error handling
- `references/rules/err-context-chain.md`: Add context with `.context()` or `.with_context()`
- `references/rules/err-custom-type.md`: Define custom error types for domain-specific failures
- `references/rules/err-doc-errors.md`: Document error conditions with `# Errors` section in doc comments
- `references/rules/err-expect-bugs-only.md`: Use `expect()` only for invariants that indicate bugs, not user errors
- `references/rules/err-from-impl.md`: Implement `From<E>` for error conversions to enable `?` operator
- `references/rules/err-lowercase-msg.md`: Start error messages lowercase, no trailing punctuation
- `references/rules/err-no-unwrap-prod.md`: Avoid `unwrap()` in production code; use `?`, `expect()`, or handle errors
- `references/rules/err-question-mark.md`: Use `?` operator for clean propagation
- `references/rules/err-result-over-panic.md`: Return `Result<T, E>` instead of panicking for recoverable errors
- `references/rules/err-source-chain.md`: Preserve error chains with `#[source]` or `source()` method
- `references/rules/err-thiserror-lib.md`: Use `thiserror` for library error types

## own
- `references/rules/own-arc-shared.md`: Use `Arc<T>` for thread-safe shared ownership
- `references/rules/own-borrow-over-clone.md`: Prefer `&T` borrowing over `.clone()`
- `references/rules/own-clone-explicit.md`: Use explicit `Clone` for types where copying has meaningful cost
- `references/rules/own-copy-small.md`: Implement `Copy` for small, simple types
- `references/rules/own-cow-conditional.md`: Use `Cow<'a, T>` for conditional ownership
- `references/rules/own-lifetime-elision.md`: Rely on lifetime elision rules; add explicit lifetimes only when required
- `references/rules/own-move-large.md`: Move large types instead of copying; use `Box` if moves are expensive
- `references/rules/own-mutex-interior.md`: Use `Mutex<T>` for interior mutability across threads
- `references/rules/own-rc-single-thread.md`: Use `Rc<T>` for shared ownership in single-threaded contexts
- `references/rules/own-refcell-interior.md`: Use `RefCell<T>` for interior mutability in single-threaded code
- `references/rules/own-rwlock-readers.md`: Use `RwLock<T>` when reads significantly outnumber writes
- `references/rules/own-slice-over-vec.md`: Accept `&[T]` not `&Vec<T>`, `&str` not `&String`

## async
- `references/rules/async-async-fn-bounds.md`: Use `AsyncFn`/`AsyncFnMut`/`AsyncFnOnce` bounds instead of `F: Fn() -> Fut, Fut: Future`
- `references/rules/async-bounded-channel.md`: Use bounded channels to apply backpressure and prevent unbounded memory growth
- `references/rules/async-broadcast-pubsub.md`: Use `broadcast` channel for pub/sub where all subscribers receive all messages
- `references/rules/async-cancel-safety.md`: Ensure futures used in `tokio::select!` branches are cancellation-safe
- `references/rules/async-cancellation-token.md`: Use `CancellationToken` for graceful shutdown and task cancellation
- `references/rules/async-clone-before-await.md`: Clone Arc/Rc data before await points to avoid holding references across suspension
- `references/rules/async-fn-in-trait.md`: Use native `async fn` in traits (stable 1.75) instead of the `async_trait` macro
- `references/rules/async-join-parallel.md`: Use `join!` or `try_join!` for concurrent independent futures
- `references/rules/async-joinset-structured.md`: Use `JoinSet` for managing dynamic collections of spawned tasks
- `references/rules/async-mpsc-queue.md`: Use `mpsc` channels for async message queues between tasks
- `references/rules/async-no-lock-await.md`: Never hold `Mutex`/`RwLock` across `.await`
- `references/rules/async-oneshot-response.md`: Use `oneshot` channel for request-response patterns
- `references/rules/async-select-racing.md`: Use `select!` to race futures and handle the first to complete
- `references/rules/async-spawn-blocking.md`: Use `spawn_blocking` for CPU-intensive work
- `references/rules/async-tokio-fs.md`: Use `tokio::fs` instead of `std::fs` in async code
- `references/rules/async-tokio-runtime.md`: Configure Tokio runtime appropriately for your workload
- `references/rules/async-try-join.md`: Use `try_join!` for concurrent fallible operations with early return on error
- `references/rules/async-watch-latest.md`: Use `watch` channel for sharing the latest value with multiple observers

## obs
- `references/rules/obs-error-chain.md`: Log errors with their full source chain, and log each error exactly once
- `references/rules/obs-instrument-spans.md`: Use `#[tracing::instrument]` and spans to attach context to async tasks and requests
- `references/rules/obs-levels-filter.md`: Use log levels meaningfully and filter with `EnvFilter` / `RUST_LOG`
- `references/rules/obs-library-facade.md`: Libraries emit through the tracing/log facade and never install a subscriber
- `references/rules/obs-no-sensitive-data.md`: Never log secrets or PII; redact or skip them
- `references/rules/obs-structured-fields.md`: Record structured key-value fields, not values interpolated into the message string
- `references/rules/obs-tracing-over-log.md`: Use `tracing` for structured, span-aware diagnostics instead of `println!` or bare `log`

## test
- `references/rules/test-arrange-act-assert.md`: Structure tests with clear Arrange, Act, Assert sections
- `references/rules/test-cfg-test-module.md`: Put unit tests in `#[cfg(test)] mod tests { }` within each module
- `references/rules/test-criterion-bench.md`: Use `criterion` for benchmarking
- `references/rules/test-descriptive-names.md`: Use descriptive test names that explain what is being tested
- `references/rules/test-doctest-examples.md`: Keep documentation examples as executable doctests
- `references/rules/test-fixture-raii.md`: Use RAII pattern (Drop trait) for automatic test cleanup
- `references/rules/test-integration-dir.md`: Put integration tests in the `tests/` directory
- `references/rules/test-loom-concurrency.md`: Use `loom` to exhaustively test lock-free and concurrent code
- `references/rules/test-mock-traits.md`: Use traits for dependencies to enable mocking in tests
- `references/rules/test-mockall-mocking.md`: Use mockall for trait mocking
- `references/rules/test-proptest-properties.md`: Use proptest for property-based testing
- `references/rules/test-should-panic.md`: Use `#[should_panic]` to test that code panics as expected
- `references/rules/test-snapshot-testing.md`: Use snapshot testing (insta) for complex or serialized output
- `references/rules/test-tokio-async.md`: Use `#[tokio::test]` for async tests
- `references/rules/test-use-super.md`: Use `use super::*;` in test modules to access parent module items

## lint
- `references/rules/lint-cargo-metadata.md`: Enable clippy::cargo for published crates
- `references/rules/lint-cfg-check.md`: Enable `unexpected_cfgs` and declare known cfgs to catch feature-gate typos
- `references/rules/lint-clippy-nursery-selected.md`: Enable high-value `clippy::nursery` lints selectively, not the whole group
- `references/rules/lint-deny-correctness.md`: `#![deny(clippy::correctness)]`
- `references/rules/lint-missing-docs.md`: Warn on missing documentation for public items
- `references/rules/lint-pedantic-selective.md`: Enable clippy::pedantic selectively
- `references/rules/lint-rustfmt-check.md`: Run cargo fmt --check in CI
- `references/rules/lint-unsafe-doc.md`: Require documentation for unsafe blocks
- `references/rules/lint-warn-complexity.md`: Enable clippy::complexity for simpler code
- `references/rules/lint-warn-perf.md`: Enable clippy::perf for performance improvements
- `references/rules/lint-warn-style.md`: Enable clippy::style for idiomatic code
- `references/rules/lint-warn-suspicious.md`: Enable clippy::suspicious for likely bugs
- `references/rules/lint-workspace-lints.md`: Configure lints at workspace level for consistent enforcement

## proj
- `references/rules/proj-bin-dir.md`: Put multiple binaries in src/bin/
- `references/rules/proj-build-rs-minimal.md`: Keep `build.rs` minimal, deterministic, and idempotent
- `references/rules/proj-feature-additive.md`: Design Cargo features to be strictly additive
- `references/rules/proj-flat-small.md`: Keep small projects flat
- `references/rules/proj-lib-main-split.md`: Keep `main.rs` minimal, logic in `lib.rs`
- `references/rules/proj-mod-by-feature.md`: Organize modules by feature, not type
- `references/rules/proj-mod-rs-dir.md`: Use mod.rs for multi-file modules
- `references/rules/proj-msrv-declare.md`: Declare `rust-version` (MSRV) in Cargo.toml and test it in CI
- `references/rules/proj-prelude-module.md`: Create prelude module for common imports
- `references/rules/proj-pub-crate-internal.md`: Use pub(crate) for internal APIs
- `references/rules/proj-pub-super-parent.md`: Use pub(super) for parent-only visibility
- `references/rules/proj-pub-use-reexport.md`: Use pub use for clean public API
- `references/rules/proj-workspace-deps.md`: Use workspace dependency inheritance for consistent versions across crates
- `references/rules/proj-workspace-large.md`: Use workspaces for large projects

## unsafe
- `references/rules/unsafe-extern-block.md`: In Rust 2024, wrap `extern` blocks in `unsafe extern { }` and annotate each item as `safe` or `unsafe`.
- `references/rules/unsafe-maybeuninit.md`: Use `MaybeUninit<T>` for uninitialized memory; never use `mem::uninitialized()` or `mem::zeroed()` for types with validity invariants.
- `references/rules/unsafe-minimize-scope.md`: Keep `unsafe` blocks as small as possible — mark only the operation that requires unsafety, not the surrounding safe code.
- `references/rules/unsafe-miri-ci.md`: Run `cargo miri test` in CI for every crate that contains `unsafe` code.
- `references/rules/unsafe-no-mangle-unsafe.md`: In Rust 2024, write `#[unsafe(no_mangle)]`, `#[unsafe(export_name = "...")]`, and `#[unsafe(link_section = "...")]` — not the bare attribute forms.
- `references/rules/unsafe-safety-comment.md`: Write a `// SAFETY:` comment above every `unsafe` block and a `# Safety` section in every `unsafe fn`.
- `references/rules/unsafe-send-sync-manual.md`: Document the invariants when manually implementing `Send` or `Sync`; prefer letting the compiler derive them automatically.

## api
- `references/rules/api-builder-must-use.md`: Mark builder methods with `#[must_use]` to prevent silent drops
- `references/rules/api-builder-pattern.md`: Use Builder pattern for complex construction
- `references/rules/api-common-traits.md`: Implement standard traits (Debug, Clone, PartialEq, etc.) for public types
- `references/rules/api-default-impl.md`: Implement `Default` for types with sensible default values
- `references/rules/api-extension-trait.md`: Use extension traits to add methods to external types
- `references/rules/api-from-not-into.md`: Implement `From<T>`, not `Into<U>` - From gives you Into for free
- `references/rules/api-impl-asref.md`: Use `AsRef<T>` when you only need to borrow the inner data
- `references/rules/api-impl-fromiterator.md`: Implement `FromIterator` and `Extend` for collection types, and `IntoIterator` for all three reference forms
- `references/rules/api-impl-into.md`: Accept `impl Into<T>` for flexible APIs, implement `From<T>` for conversions
- `references/rules/api-must-use.md`: Mark types and functions with `#[must_use]` when ignoring results is likely a bug
- `references/rules/api-newtype-safety.md`: Use newtypes to prevent mixing semantically different values
- `references/rules/api-non-exhaustive.md`: Use `#[non_exhaustive]` on public enums and structs for forward compatibility
- `references/rules/api-operator-overload.md`: Overload operators only when the semantics are natural and unsurprising
- `references/rules/api-parse-dont-validate.md`: Parse into validated types at boundaries
- `references/rules/api-sealed-trait.md`: Use sealed traits to prevent external implementations while allowing use
- `references/rules/api-serde-optional.md`: Make serde a feature flag, not a hard dependency for library crates
- `references/rules/api-typestate.md`: Use typestate pattern to encode state machine invariants in the type system

## name
- `references/rules/name-acronym-word.md`: Treat acronyms as words in identifiers: `HttpServer`, not `HTTPServer`
- `references/rules/name-as-free.md`: `as_` prefix: free reference conversion
- `references/rules/name-consts-screaming.md`: Use `SCREAMING_SNAKE_CASE` for constants and statics
- `references/rules/name-crate-no-rs.md`: Don't suffix crate names with `-rs` or `-rust`
- `references/rules/name-funcs-snake.md`: Use `snake_case` for functions, methods, variables, and modules
- `references/rules/name-into-ownership.md`: Use `into_` prefix for ownership-consuming conversions
- `references/rules/name-is-has-bool.md`: Use `is_`, `has_`, `can_`, `should_` prefixes for boolean-returning methods
- `references/rules/name-iter-convention.md`: Use iter/iter_mut/into_iter for iterator methods
- `references/rules/name-iter-method.md`: Name iterator methods `iter()`, `iter_mut()`, and `into_iter()` consistently
- `references/rules/name-iter-type-match.md`: Name iterator types after their source method
- `references/rules/name-lifetime-short.md`: Use short, conventional lifetime names: `'a`, `'b`, `'de`, `'src`
- `references/rules/name-no-get-prefix.md`: Omit get_ prefix for simple getters
- `references/rules/name-to-expensive.md`: Use `to_` prefix for expensive conversions that allocate or compute
- `references/rules/name-type-param-single.md`: Use single uppercase letters for type parameters: `T`, `E`, `K`, `V`
- `references/rules/name-types-camel.md`: Use `UpperCamelCase` for types, traits, and enum names
- `references/rules/name-variants-camel.md`: Use `UpperCamelCase` for enum variants

## serde
- `references/rules/serde-custom-with.md`: Customize a field's (de)serialization with `with` / `serialize_with` / `deserialize_with`
- `references/rules/serde-default-compat.md`: Use `#[serde(default)]` for optional and backward-compatible fields
- `references/rules/serde-deny-unknown-fields.md`: Reject unexpected keys with `#[serde(deny_unknown_fields)]`
- `references/rules/serde-enum-representation.md`: Choose enum tagging deliberately: externally, internally, adjacently tagged, or untagged
- `references/rules/serde-flatten.md`: Inline nested structs or capture extra keys with `#[serde(flatten)]`
- `references/rules/serde-rename-all.md`: Match the external naming convention with `#[serde(rename_all = ...)]`
- `references/rules/serde-skip-empty.md`: Omit empty fields with `skip_serializing_if`
- `references/rules/serde-try-from-validate.md`: Validate while deserializing with `#[serde(try_from = "Raw")]`

## conc
- `references/rules/conc-atomic-ordering.md`: Use the weakest correct memory `Ordering` for every atomic operation
- `references/rules/conc-rayon-par-iter.md`: Use rayon's `par_iter()` for CPU-bound data parallelism
- `references/rules/conc-scoped-threads.md`: Use `std::thread::scope` to borrow stack data across threads
- `references/rules/conc-thread-local.md`: Prefer `thread_local!` with `Cell`/`RefCell` over `static mut`

## type
- `references/rules/type-deref-coercion.md`: Implement `Deref`/`DerefMut` only for smart-pointer and transparent wrapper types
- `references/rules/type-display-vs-debug.md`: Use `Display` for user-facing output and `Debug` for diagnostics; never swap them
- `references/rules/type-enum-states.md`: Use enums for mutually exclusive states
- `references/rules/type-generic-bounds.md`: Add trait bounds only where needed, prefer where clauses for readability
- `references/rules/type-never-diverge.md`: Use `!` (never type) for functions that never return
- `references/rules/type-newtype-ids.md`: Wrap IDs in newtypes: `UserId(u64)`
- `references/rules/type-newtype-validated.md`: Use newtypes to enforce validation at construction time
- `references/rules/type-no-stringly.md`: Avoid stringly-typed APIs; use enums, newtypes, or validated types
- `references/rules/type-numeric-fmt.md`: Implement `LowerHex`, `UpperHex`, `Octal`, and `Binary` for numeric newtypes
- `references/rules/type-option-nullable.md`: Use `Option<T>` for values that might not exist
- `references/rules/type-phantom-marker.md`: Use `PhantomData` to express type relationships without runtime cost
- `references/rules/type-repr-transparent.md`: Use `#[repr(transparent)]` for newtypes in FFI contexts
- `references/rules/type-result-fallible.md`: Use `Result<T, E>` for operations that can fail


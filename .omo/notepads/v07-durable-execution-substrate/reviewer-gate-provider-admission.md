# Reviewer Gate blockers #2 / #4 — issue #118

## RED (before implementation)

- `cargo test -p runtime --test provider_admission`: 0 passed / 3 failed, each at `failed admission registered a worker`. Covers unavailable selected endpoint, unadvertised explicit model, and unavailable fallback with a healthy primary.
- `cargo test -p gui --test auto_title_provider`: failed at fixture Content-Length unwrap for GET discovery, then title remained `Explain lifetimes` instead of provider output.
- `cargo test -p providers --test list_models_contract rejects_oversized_catalog_response`: failed because the oversized valid JSON catalog was accepted.

## Repair

- AgentModel admission is invoked before worker semaphore reservation, team task enqueue, run insertion, and runtime lifecycle-start events. Synchronous start retains its reserved RunId; wait awaits admission and returns its typed error. Fixed/test models retain synchronous registration. SwitchableModel forwards the admission contract; UnconfiguredModel rejects admission.
- Routed admission retains discovered model IDs, verifies the selected route and all configured route candidates (including explicit candidate model overrides), and rejects unadvertised or locally unlisted models. Endpoint/auth/account-equivalent probes share the existing OnceCell cache. Direct complete/streaming verification is retained for compatibility callers such as title generation.
- OpenAI-compatible and Codex catalogs have a 10-second whole-request timeout and a 1 MiB accumulated response limit, including error bodies. HTTP/JSON/429 contracts remain covered.
- GUI title fixture accepts GET `/v1/models` independently of completion bodies. Both quick=true (fast) and quick=false (chosen) assert discovery-before-completion and the resulting title.
- Existing mock-openai already exposes `spawn_with_models`; reused without broadening its default advertised catalog.

## GREEN / checks

- `cargo test -p runtime --test provider_admission`: 4 passed, including actual catalog -> streaming completion -> final run result success.
- `cargo test -p runtime --test provider_admission --test provider_verify -p providers --test list_models_contract --test codex_models_contract -p gui --test auto_title_provider`: all targets passed (3/6/5/6/1 before adding the separately passing admission happy-path test).
- `cargo test -p providers --tests -- --test-threads=1`: every provider unit/integration target passed. Initial parallel provider run had an unrelated cache observation assertion (0 vs 1); serial execution passed.
- Runtime library: 366 passed. Full runtime integrations stopped at existing `background` start/cancel event assertions (3 passed / 2 failed); fixed-model tests do not use the new admission path.
- GUI library: 238 passed / 1 ignored; binary unit tests 11 + 33 passed. Full GUI integrations progressed through title and chat tests, then `demo_loop` failed: review fixture approves with `evidence: None`, triggering another repair and exhausting the demo script. Also reports existing shell `demo repair` argument splitting. Not modified (review transport is outside this task).
- `cargo check -p runtime -p providers -p gui --all-targets`: passed.
- `cargo clippy -p runtime -p providers -p gui --all-targets -- -D warnings`: passed.
- Targeted rustfmt --check (edition 2024, skip_children=true) on all 12 touched Rust paths: passed. Package-wide fmt reports concurrent `runtime/tests/review_loop.rs` formatting, not changed here.
- LSP error diagnostics: clean on all 12 touched Rust paths (absolute paths required by this tool).

## Scope / self-review

- No budget tracker or review transport edits. Other workers are concurrently editing and committing this worktree; only admission/provider/title paths are staged.
- New admission module: 55 pure LOC; verification module: 186; admission tests: 153; catalog source: 92; GUI fixture: 137. compose/tests.rs is 241 (warning band). Existing oversized runtime.rs (1183) and compose.rs (439) receive wiring only; broad host splitting is deferred because runtime.rs is shared with concurrent review work. Admission behavior lives in its own small module.
- Boundary JSON is parsed into typed catalog structs; no new unsafe, casts, unchecked production unwraps, dependencies, logging, or budget/review behavior. Multi-argument admission/register functions mirror the existing run-spawn seam to preserve its handoff/parent contract.
- Commit is atomic for blockers #2/#4, no push.

# Dependencies Guidelines

This document outlines the preferred dependencies and libraries to use in the Constructor Fabric Gears project.

## Serialization

### YAML

**Always use `serde-saphyr` for YAML serialization/deserialization, not `serde_yaml`.**

- **Package name in Cargo.toml**: `serde-saphyr`
- **Import in Rust code**: `use serde_saphyr;`
- **Reason**: `serde_yaml` is deprecated and unmaintained. `serde-saphyr` is the actively maintained fork.

#### Example Usage

```rust
use serde_saphyr;
use std::collections::HashMap;

// Serialization
let data = HashMap::from([("key", "value")]);
let yaml_string = serde_saphyr::to_string(&data)?;

// Deserialization
let parsed: HashMap<String, String> = serde_saphyr::from_str(&yaml_string)?;
```

**Note**: `serde-saphyr` does not provide a `Value` type like `serde_yaml` did. For generic YAML parsing, use `HashMap<String, serde_json::Value>` or define a specific struct.

## Concurrency

### Synchronous locks

**Use `std::sync::Mutex` and `std::sync::RwLock` by default for synchronous locks.**

- **Reason**: since Rust 1.62 the `std` locks are futex-based on Linux (and use SRW locks on Windows). They take no syscall on the uncontended path, need no heap allocation, and perform on par with `parking_lot` for typical workloads. `Mutex::new` / `RwLock::new` are `const` (Rust 1.63+), so they work in `static`s without `LazyLock`. Using `std` also keeps one less dependency in the crate.
- **Poisoning**: `std` locks return `LockResult`, and the workspace denies `clippy::unwrap_used` / `clippy::expect_used`, so handle the poison case explicitly. Choose how based on the data being protected:
  - **The data stays valid even if a holder panicked midway** (counters, caches, maps where every update is a single call): recover the guard with `PoisonError::into_inner`.
  - **A panic midway could leave the data inconsistent** (multi-step updates that must happen together): turn the poison into an error and propagate it. Don't silently continue.

```rust
use std::sync::{Mutex, PoisonError};

let data = Mutex::new(Vec::new());

// Data stays valid after a panic: recover the guard
data.lock().unwrap_or_else(PoisonError::into_inner).push(42);

// Data could be inconsistent after a panic: surface it as an error
let mut guard = data.lock().map_err(|_| MyError::StatePoisoned)?;
guard.push(43);
```

### When to use `parking_lot` instead

`parking_lot` (declared in the workspace `Cargo.toml`) is allowed when you need something `std` does not provide on stable:

- upgradable read locks (`RwLock::upgradable_read`)
- timed acquisition (`try_lock_for` / `try_lock_until`)
- owned guards held through an `Arc` (`arc_lock` feature) or mapped guards (`MutexGuard::map`)
- `ReentrantMutex`
- fair unlocking or the `deadlock_detection` feature
- `RwLock` behaviour that is the same on every platform (`std`'s `RwLock` fairness and writer preference depend on the OS)

Write down which of these you need in a comment next to the dependency in the crate's `Cargo.toml`. Existing `parking_lot` usage does not need to be migrated. Within a single module, use one lock family consistently.

### When to use `tokio::sync` instead

Use `tokio::sync::Mutex` or `tokio::sync::RwLock` only when you need to **hold the lock across an `.await` point**. If the critical section is purely synchronous (no `.await` inside), prefer a synchronous lock (`std::sync`, or `parking_lot` per the rules above) even in async code. It avoids the overhead of the async mutex and won't block the executor as long as the critical section is short. Never hold a `std::sync::MutexGuard` across an `.await`: the guard is `!Send`, so the future won't be `Send`.

If a synchronous lock guard must be held while performing blocking I/O or expensive computation, wrap the call in `tokio::task::spawn_blocking()` to avoid stalling the async executor.

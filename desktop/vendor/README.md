# Vendored crates

## tauri-runtime-wry

`tauri-runtime-wry/` is the published crate **tauri-runtime-wry 2.12.4**
from crates.io, patched in by `[patch.crates-io]` in `desktop/Cargo.toml`,
with one change in `src/lib.rs`.

### The change

`DispatcherMainThreadContext.window_target` is an
`Arc<EventLoopWindowTarget<Message<T>>>` instead of a plain
`EventLoopWindowTarget<Message<T>>`, and `Wry::init` builds it with
`Arc::new(event_loop.deref().clone())`. The field carries a comment saying
why.

The context is cloned on every `AppHandle` clone, from any thread. Cloning a
plain event-loop target clones the non-atomic `Rc`s it holds on Windows (tao
shares the event-loop runner through them), so a clone on a worker thread
races the main thread on those counts. A lost update frees the live runner,
and the app crashes in the event loop. Behind an `Arc` a clone only touches
an atomic count. Every other use of the field derefs through the `Arc`
unchanged.

Drop the vendored copy, and the patch entry, once upstream holds the target
behind an `Arc` (or tao shares the runner through atomics).

### Layout

The crate's files as published, minus `.cargo_vcs_info.json`, `Cargo.lock`
and `Cargo.toml.orig`, which cargo does not need for a path dependency.

### Re-vendoring on a tauri update

1. Find the tauri-runtime-wry version the new `tauri` requires
   (`cargo update -p tauri --dry-run`, or its `Cargo.toml`).
2. Download `https://static.crates.io/crates/tauri-runtime-wry/tauri-runtime-wry-<version>.crate`
   and check its SHA-256 against the `cksum` of that version in the
   registry index, `https://index.crates.io/ta/ur/tauri-runtime-wry`.
3. Replace `tauri-runtime-wry/` with the unpacked crate, then delete the
   three files listed under Layout.
4. In `src/lib.rs`, change the `window_target` field of
   `DispatcherMainThreadContext` to the `Arc` form with its comment, and wrap
   the value built for it in `Wry::init` in `Arc::new(...)`. A diff of the old
   copy against the old upstream release shows exactly these two hunks.
5. Update the version named here and in `desktop/Cargo.toml`, run
   `cargo update -p tauri-runtime-wry`, and build: cargo warns if the patch
   was not used.

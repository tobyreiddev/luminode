# Luminode desktop app

Tauri 2 shell: Svelte 5 frontend in `src/`, Rust core in `src-tauri/src/`.
See the [repository README](../README.md) for architecture, build
instructions, and design decisions, and `src-tauri/src/lib.rs` for how the
core's threads and tasks are wired together.

```sh
npm install
npm run tauri dev     # develop
npm run tauri build   # release bundle
npm run check         # svelte-check
```

Recommended IDE setup: VS Code + Svelte + Tauri + rust-analyzer extensions.

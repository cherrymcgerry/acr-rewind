# Contributing to ACR Rewind

Thanks for helping out! Bug reports, signature updates for new game builds, and code changes are
all welcome.

## Offline-only policy

ACR Rewind is for **offline, single-player** play. Contributions must keep it that way:

- Don't weaken the online guard (`crates/acr_hook/src/guard.rs`, `[online_guard]` in
  `config/signatures.toml`). It must keep failing closed and keep leaderboard modes blocked.
- Don't add features aimed at multiplayer, leaderboards or competitive modes, or at hiding the
  mod from the game.
- When testing, start Steam in offline mode and stay out of online, Time Attack, Challenge,
  Event and leaderboard screens while the mod or any tool is injected.
- Don't commit files derived from the game: Dumper-7 SDK output (`CppSDK/`), memory dumps, game
  binaries or assets. Keep logs free of personal paths.

## Building

Requirements: Windows 10/11, the Rust toolchain from [rustup](https://rustup.rs/) (the pinned
channel and components are in `rust-toolchain.toml`), and the MSVC build tools.

```powershell
$env:Path += ";$HOME\.cargo\bin"
cargo build --workspace --release
```

The outputs are in `target\release`: `dwmapi.dll` (loader), `acr_hook.dll` (the mod),
`injector.exe` and `acr-probe.exe` (development tools).

`.cargo/config.toml` links the C runtime statically (`+crt-static`), so the binaries don't need
the Visual C++ redistributable. Don't override it with a `RUSTFLAGS` environment variable when
building release files; check with `dumpbin /dependents target\release\acr_hook.dll` that no
`VCRUNTIME140.dll` or `api-ms-win-crt-*` imports appear.

## Before you open a pull request

CI runs the same checks:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

- Keep game-specific values in `config/signatures.toml`, not in code.
- Add a line under `[Unreleased]` in `CHANGELOG.md` for user-visible changes.
- If `Cargo.lock` changes, regenerate the notices:
  `powershell -ExecutionPolicy Bypass -File scripts\third-party-notices.ps1`.
- For in-game changes, describe how you tested (game build, input device, mode) and include the
  relevant `acr-rewind.log` lines.

## Updating signatures after a game patch

When an update breaks the mod, it disables itself and `acr-rewind.log` explains what failed.
Refreshing `config/signatures.toml` is usually all that's needed:

1. Rebuild the SDK with a self-built Dumper-7 and import it with
   `acr-probe import-dumper7`.
2. Check everything with `acr-probe sig-test` and re-locate the car bodies with
   `acr-probe find-car`.
3. Test in-game with `read_only = true`, then with writes enabled.
4. Set `verified_build` to the new Steam build id and open a pull request.

The full procedure is in [docs/updating-after-a-patch.md](docs/updating-after-a-patch.md); what
each value means is in [docs/technical-reference.md](docs/technical-reference.md).

## Packaging

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package.ps1
```

This writes `dist\ACR-Rewind-<version>.zip` (player files only), `dist\acr-rewind-dev-<version>.zip`
(development tools) and `dist\SHA256SUMS.txt`. Releases are built by
`.github/workflows/release.yml` when a `v*` tag is pushed.

## License

By contributing, you agree that your contributions are dual-licensed under MIT OR Apache-2.0,
like the rest of the project.

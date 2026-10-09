# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-beta.1] - 2026-10-09

First public beta. Supports *Assetto Corsa Rally* Early Access v0.6 (Steam build 25170642).
Tested so far with the VW Polo GTI R5 on a few stages; other cars are expected to work.

### Added

- Forza Horizon-style rewind mode: press Rewind to freeze the game, scrub back and forward
  through the last 30 s (configurable), then resume from the cursor or cancel back to the present.
- `car_avatar` backend: restores the car through the game's `CarAvatar` functions
  (`SetPhysicsTransform`, `SetVelocityCMS`) plus the velocities of every sim rigid body of the
  car, called on the game thread from a ProcessEvent detour. `sim_car` (raw rigid-body writes) and
  `ue_actor` backends as fallbacks.
- Sim rigid-body locator (`vtable_scan`) that finds the player car's bodies in memory on a
  background thread and re-validates them every tick.
- Resume run-in: during the 0.25 s resume delay the simulation runs while the restored pose and
  velocities are written every frame, so the engine and tyres spin back up before you get control.
  Engine RPM and gear can't be written directly; this approximates them and avoids the car
  sliding out right after resuming.
- Input from keyboard, XInput pads and DirectInput devices (wheels, button boxes, pedals), with
  analog scrubbing (deadzone, curve, hold boost) and `resume_requires_release`.
- In-game overlay with the rewind timeline and an `F8` settings panel for bindings, scrub speed
  and mode options, saved back into `acr-rewind.toml` with its comments kept.
- Online guard that disables rewinding in online sessions, replays and leaderboard modes
  (Time Attack, Challenges, ACR Events), combining several signals and failing closed.
- Safety checks: the mod disables itself and logs the reason when a signature fails validation,
  the game build differs, or a write to the game fails. A post-resume check that doesn't match is
  logged as a warning; three in a row disable rewind until the car respawns or the stage is restarted or changed.
- Diagnostics: a `resume check` log line shortly after each resume (speed, slip angle, yaw rate,
  RPM) and per-rewind write timing.
- Read-only validation mode (`read_only = true`) for checking a new game build.
- `dwmapi.dll` proxy loader; `injector.exe` for development.
- `acr-probe`, a read-only reverse-engineering tool (shared-memory scans, pointer scans,
  `sig-test`, `find-car`, Dumper-7 SDK import).

### Changed

- The C runtime is linked statically: no Visual C++ redistributable is needed.

### Fixed

- Game crash (heap corruption) after crashing into roadside vegetation: debris bodies spawned by
  the crash could be mistaken for parts of the car and written to. The car's body set is now
  pinned once confirmed, and no memory rescans run while rewinding (these also made scrubbing
  choppy).
- Rewind after a crash that damages the car: a slightly deformed car stays locked, its history
  survives re-locking, and parts that come off the car are no longer restored.
- Resuming after a crash no longer uses crash-site body velocities (a 46 m/s resume came out at
  3.4 m/s); body history is never extrapolated across gaps.
- The car bobbed while frozen: the held pose is written every frame again.
- The car slid out right after resuming because the drivetrain was still at the scrub state
  (see resume run-in above).

[Unreleased]: https://github.com/cherrymcgerry/acr-rewind/compare/v0.1.0-beta.1...HEAD
[0.1.0-beta.1]: https://github.com/cherrymcgerry/acr-rewind/releases/tag/v0.1.0-beta.1

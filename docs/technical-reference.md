# Technical reference

Game internals ACR Rewind depends on, for contributors. The values live in
`config/signatures.toml`; to refresh them after a game update, follow
[`updating-after-a-patch.md`](updating-after-a-patch.md).

Every `0x…` address below is an **RVA** (offset from the `acr.exe` module base) or a field
offset. Heap addresses change on every launch. ACR Rewind is an offline, single-player mod.

## 1. Game build

| Item | Value |
|---|---|
| Store | Steam, app id 3917090 |
| Verified build | **25170642** (Early Access v0.6) |
| Engine | Unreal Engine 5.6.1, monolithic Shipping build |
| Executable | `acr\Binaries\Win64\acr.exe` (PE `OriginalFilename` = `acr-Win64-Shipping.exe`) |
| Car physics | Custom Kunos-derived solver (`dmphysics`), not Chaos Vehicles |
| Build id source | `steamapps\appmanifest_3917090.acf`, `buildid` |

## 2. Engine globals

From a Dumper-7 run (`Basic.hpp`, `namespace Offsets`), all verified live except `AppendString`.

| Item | RVA | `[raw_offsets]` key | Notes |
|---|---|---|---|
| GObjects | `0xb35f5b0` | `gobjects_rva` | `FUObjectArray::ObjObjects`; ~112k elements, 65536 per chunk, `FUObjectItem` 0x18 |
| FNamePool | `0xb27ba80` | `gnames_rva` | `Blocks[0]` at +0x10 → block starting `"None"`, `"ByteProperty"` |
| GWorld | `0xb074730` | `gworld_rva` | `UWorld*`; null until the first level loads, read on every use |
| ProcessEvent | `0x16971b0` | `process_event_rva` | Detoured; the game-thread tick |
| ProcessEvent index | `0x4c` | `process_event_index` | VFT slot |
| AppendString | `0x14769e0` | `append_string_rva` | Optional, unused |

- Dumper-7's `Offsets::GNames` (`0xb242ee8`) is **not** the FNamePool (FName 0 there isn't
  "None"). `import-dumper7` never writes it.
- If a raw RVA fails validation, the resolver tries the configured strategy: Dumper-7-style scans,
  then byte patterns, then for the FNamePool a `.data` scan for a pointer to a block starting
  "None", "ByteProperty" (`fnamepool.fallback_data_scan`). The byte patterns are unverified
  generic UE5 fallbacks.
- `acr-probe sig-test` and the hook use the same resolver functions.

## 3. Reflection layout (UE 5.6 x64)

`[raw_offsets.uobject]`:

| Member | Offset | Member | Offset |
|---|---|---|---|
| `UObject::ClassPrivate` | 0x10 | `UStruct::ChildProperties` | 0x50 |
| `UObject::NamePrivate` | 0x18 | `UStruct::PropertiesSize` | 0x58 |
| `UObject::OuterPrivate` | 0x20 | `UFunction::FunctionFlags` | 0xB0 |
| `UField::Next` | 0x28 | `UFunction::Func` | 0xD8 |
| `UStruct::SuperStruct` | 0x40 | `FField::ClassPrivate` | 0x08 |
| `UStruct::Children` | 0x48 | `FField::Next` / `NamePrivate` | 0x18 / 0x20 |
| | | `FProperty::ElementSize` / `PropertyFlags` / `Offset_Internal` | 0x34 / 0x38 / 0x44 |

- UE 5.6 has no `FFieldVariant` flag, so `FField`/`FProperty` members sit 8 bytes lower than
  older UE5 defaults.
- FNamePool: stock UE5 layout, `FNameBlockOffsetBits` 16, entry stride 2, 2-byte header.
- UFunction parameter offsets and sizes are always read from the `ChildProperties` chain at
  runtime; the layouts in §4 are for reference.
- LWC: `FVector`/`FRotator` are 3 × f64, `FQuat` 4 × f64, units cm.

## 4. Car pawn and UFunctions

Class chain: `acr.AcrCarAvatar` (0xE10) → `dmphysics.CarAvatar` (0xCD0) → `Pawn` → `Actor`.

| `CarAvatar` member | Offset |
|---|---|
| `p_root_component_vehicle` (`UCarAvatarRootComponent`) | 0x438 |
| `p_car_data` | 0x488 |
| `bInitializePhysics` | 0x578 |

Pawn lookup (`pawn_lookup = "local_player"`): `GWorld → OwningGameInstance → LocalPlayers[0] →
PlayerController → AcknowledgedPawn` via reflected properties, then IsA `CarAvatar`; falls back to
a class scan.

UFunctions used by the `car_avatar` backend (all via ProcessEvent, game thread only):

| UFunction | Parms size | Layout |
|---|---|---|
| `Actor.K2_GetActorLocation() -> FVector` | 0x18 | cm |
| `Actor.K2_GetActorRotation() -> FRotator` | 0x18 | degrees |
| `CarAvatar.SetPhysicsTransform(const FTransform& InTransform, bool bResetCar)` | 0x70 | `InTransform` @0x00: quat x,y,z,w @0x00, translation @0x20 (cm), scale @0x40; `bResetCar` @0x60 |
| `CarAvatar.SetVelocityCMS(FVector InVelocityCMS)` | 0x18 | f64 × 3, cm/s |
| `CarAvatar.GetVelocityCMS() -> FVector` | 0x18 | cm/s |
| `CarAvatar.GetRPMS() -> float` | 4 | |
| `CarAvatar.GetSpeedKMH() -> float` | 4 | |
| `CarAvatar.GetGear() -> FString` | 0x10 | `"R"`, `"N"`, `"1"`..`"n"`; engine-allocated, not freed (polled every `gear_poll_ms`) |
| `CarAvatar.GetRespawnSeq() -> int32` | 4 | Changes on respawn; treated as a new car |

Other game functions (not used by default):

- `dmphysics.PhysicsAvatar.SetPaused(bool)`: opt-in freeze lever `physics_avatar_pause`.
- `acr.AcrGameMode.PauseGame()` / `ResumeGame()`: run the game's pause flow.
- `acr.RaceGameMode.RespawnParticipant(FName, FName)`, `ResetParticipants()`;
  `AcrCarAvatar.TeleportAtDistanceAlongSpline(float)`.

Backends (`[backend] order = ["car_avatar", "sim_car", "ue_actor"]`): the first that can be
built wins; each skipped one logs `backend X unavailable: …`. `car_avatar` needs a game-thread
tick, ProcessEvent, the `CarAvatar` class and the core UFunctions. `ue_actor` (generic actor
teleport) does not move the `dmphysics` car.

`car_avatar` write, every scrub frame and on release:

1. `SetPhysicsTransform(snapshot, bResetCar = reset_car)`.
2. `SetVelocityCMS(v)`, `v = 0` while scrubbing.
3. Sim bodies per `body_writes`: `velocities` (default; recorded linear + angular velocity of
   every body, zero while scrubbing), `full` (recorded body states) or `none`. Skipped and
   rescanned if the body headers no longer match.

## 5. Sim rigid bodies

`[raw_offsets.sim_car]`, `locator = "vtable_scan"`. Native memory, outside UE reflection.

| Item | Value |
|---|---|
| Vtable | `acr.exe + 0xa8d9830` (first qword of each body) |
| Slot size | 0x300 (pool slot; 8-byte allocator header, object at slot + 8) |
| Value type / units | f64, metres, same axes as shm `graphics.carCoordinates` |

| Offset (from vtable qword) | Field |
|---|---|
| `+0x8` | parent pointer |
| `+0x138` | position (vec3, world) |
| `+0x158` | rotation, 3 rows × 4 f64 (row stride 0x20; only the 3 × 3 part is used) |
| `+0x1d8` | linear velocity (world, m/s; matches shm physics velocity) |
| `+0x1f8` | angular velocity (rad/s; not correlated against shm) |

- A parent-pointer group holds ~28 bodies: the car's 14–20 bodies (chassis, suspension, wheels)
  plus unrelated world objects 20–800 m away.
- The main chassis body and a twin (next slot, +0x300) sit exactly at the car position.
- Wheel spin, engine rpm and gear are not in the body.
- Addresses and slot order change on every stage load; static pointer chains don't survive a
  restart.

Selection rule (`acr_ue::bodies`, also used by `acr-probe find-car`):

1. Sweep committed `MEM_PRIVATE` `PAGE_READWRITE` memory on a background thread for 8-aligned
   qwords equal to `module_base + vtable_rva`.
2. Keep hits with a plausible parent, an orthonormal rotation (det ≈ 1) and in-range values.
3. Re-read all bodies, then sample the shm car position.
4. Group by parent; pick the group with a body within `match_tolerance_m` (+ `|v| · shm_lag_s`)
   of the shm position (nearest wins, ties go to the larger group).
5. Main body: nearest to the shm position, preferring one with an exact twin, then the lower
   address.
6. Car = group bodies within `car_radius_m` (4 m) of the main body. Above `car_moving_mps`
   (1 m/s), a body's velocity must also be within `car_velocity_tolerance_mps` (3 m/s) +
   |ω| · distance (|ω| capped at 10 rad/s) of the main body's.
7. The set is fixed once locked. Rescans first try the regions that held the previous car.

Per tick: check every body's vtable and parent, a main-body jump below `max_jump_m`, and every
100 ms the distance to shm (`hint_mismatch_m` for `hint_mismatch_s`). Jump and shm checks are
skipped right after the mod's own writes. Any failure drops the car and requests a rescan.

## 6. UE ↔ sim axes

| Space | Units | Axes |
|---|---|---|
| UE world | cm | left-handed, X forward, Y right, Z up |
| Sim / shm | m | Y up |

- Map: `sim = (UE.x, UE.z, UE.y) / 100`, i.e. `ue_to_sim_axes = ["+x", "+z", "+y"]` (pinned).
- General form `sim[i] = sign · ue[axis] / 100` with matrix `M`: vectors `M v`, rotations
  `M R Mᵀ`, angular velocity `det(M) · M ω`.
- `"auto"` derives the map from actor location vs main body (`axes_fit_tolerance_m`) and logs it.
- Snapshots are stored in UE space; writes don't use the map. It feeds the validate line and
  the UE-space angular velocity.

## 7. Shared memory

| Page | Used for |
|---|---|
| `Local\acpmf_graphics` | `carCoordinates` (car position hint), `status`, `session`, `packetId`, `activeCars`, `playerCarID` |
| `Local\acpmf_physics` | `packetId`, `velocity`, `rpms`, `gear`, `wheelAngularSpeed` (validation) |
| `Local\acpmf_static` | `isOnline` (guard) |
| `Local\acpmf_Moza` | Header only: `numVehicles` i32 @0x0, `focusVehicle` i32 @0x4, `serverName` char[512] @0x8 (packed to 4) |

Quirks on build 25170642:

- `graphics.status` reads `OFF` (0) while driving live; only `REPLAY` (1) makes the mod inactive
  (`shm_require_status_live = false`).
- Offline solo leaves `activeCars`, `carID` and `playerCarID` at 0. The car position is the
  `carCoordinates` entry of `playerCarID` among the first `activeCars`, else slot 0
  (`acr_shm::car_position`).
- `carCoordinates` trails the physics by ~1 update (0.34–0.59 m at 58 km/h). Body-vs-shm checks
  allow `|v| · shm_lag_s` (0.06 s) extra.
- The physics page can read all zero; the post-resume speed check is then skipped.
- `acpmf_Moza` only exists while Moza software runs.

## 8. Online guard

`crates/acr_hook/src/guard.rs`, `[online_guard]`. Rechecked every second; fails closed.

| Input | Blocks when |
|---|---|
| UE net mode | Not standalone. Standalone = `UWorld::NetDriver` (+0x38) null; `DemoNetDriver` (+0x140) ignored |
| `acpmf_static.isOnline` | true |
| `acpmf_graphics.session` | In `shm_blocked_session_types` (empty by default) |
| `acpmf_graphics.status` | `REPLAY` → inactive, not blocked |
| `acpmf_Moza` (if present) | Non-empty `serverName`, or `numVehicles > 1`. A missing page is ignored |
| Live classes | Any live object of a class in `blocked_live_classes` |
| Game mode / state class name | Contains `Lobby`, `Multiplayer`, `TimeAttack`, `Challenge`, `Event` or `Leaderboard` |

Classes seen offline: `BC_MainGameMode_C` / `BC_MainGameState_C` (menu, `MainMenuWales`) and
`BC_RaceGameMode_C` / `BC_RaceGameState_C` (stage). The guard gates writes and the rewind input
only; read-only mode records and validates even while blocked.

## 9. Force feedback

`crates/acr_hook/src/ffb.rs`. Covers wheels the game drives through DirectInput 8.

- The input thread opens its own non-exclusive handle to an FFB device and creates an
  uninitialised effect to reach dinput8's shared `IDirectInputEffect` vtable.
- Hooked: `IDirectInputEffect::SetParameters`, and `CreateEffect` in the `IDirectInputDevice8W`
  and `IDirectInputDevice8A` vtables.
- Scaling applies to a copy of the game's `DIEFFECT`: `DICONSTANTFORCE` (4 bytes), `DIRAMPFORCE`
  (8), `DIPERIODIC` magnitude and offset (16), plus `DIENVELOPE` attack/fade levels. `dwGain` is
  never changed.
- Factor 0 while the car is held (rewind mode, run-in), then a linear ramp over `ffb.fade_in_s`
  (frames > 0.1 s count as 0.1 s). Factor 1 if the tick stops publishing for 5 s.
- The last game force per effect is re-sent at the current factor when the game updates an effect
  without type-specific parameters. The mod never calls effects outside the game's own calls.
- Not covered: Fanatec wheels. The game statically links the Fanatec SDK, which sends HID reports
  directly, bypassing dinput8.

## 10. Log lines and health check

`acr-rewind.log`, next to the DLLs. With `read_only = true` nothing is written or frozen.
Expected order on a healthy start:

1. `GObjects: 0x… via raw_offsets.gobjects_rva 0xb35f5b0`, `FNamePool: … via raw_offsets.gnames_rva 0xb27ba80`,
   `GWorld: …`, `ProcessEvent: …`. Before the engine is up: `waiting for the engine (attempt n, t s): …`.
2. `UFunctions resolved: [...]`; if init ran early, `UFunctions not linked yet: [...]` then
   `all UFunctions resolved` from the background thread.
3. `backend: car_avatar (…)`. Otherwise `backend CarAvatar unavailable: …`; a timing-only
   fallback later logs `backend upgraded: sim_car -> car_avatar (…)`.
4. `sim bodies: shm shows a car at (x, y, z); starting the body scan`, then
   `sim bodies: scan finished: … car = group i (… K car bodies, E other bodies of the group excluded …)`.
   Expect K ≈ 14–20, E > 0. Failure: `sim bodies: scan finished without a car: …`.
5. `online guard: Allowed | net mode standalone, …` on a stage offline.
6. `player car found: car_avatar (pawn 0x…, N sim bodies, …)`.
7. Every `diag_interval_ms`: `car_avatar validate: … [actor-body d m, rot r deg, vel e m/s] … [body-shm d m]`.
   `actor-body` and `actor-shm` stay well below 1 m, `rot` within a few degrees.
8. Every 10 s: `heartbeat: backend … | guard … | car locked yes | bodies N | … ticks/s | …`.
9. FFB: `ffb: DirectInput effect hook installed … CreateEffect W hooked, A hooked`; after each
   rewind `ffb: rewind over, full force again: N force updates scaled of M effect calls …`.

Write mode, per rewind: `rewind mode entered at t=…`, then `rewind mode: resume from t=…`,
`resumed at t=… (speed … m/s, gear …)` and `post-resume validation ok`, or
`rewind mode cancelled: …`. Three failed post-resume validations in a row disable rewind until
a respawn or restart.

Quick external check while sitting in the car: `acr-probe sig-test` (all `[OK  ]`) and
`acr-probe find-car` (14–20 car bodies, main body tracking shm).

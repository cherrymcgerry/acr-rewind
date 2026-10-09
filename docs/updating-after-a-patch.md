# Updating ACR Rewind after a game patch

Every game-specific address, offset and class name ACR Rewind uses lives in
`config/signatures.toml`. When an Assetto Corsa Rally update moves things around, the mod notices
(a raw offset fails validation, a class or UFunction is missing, or the car can't be located),
writes the reason to `acr-rewind.log`, and stays disabled. This guide explains how to bring
`signatures.toml` up to date for a new build. In most cases no code changes are needed.

Background on what each value is and how it was originally found is in
[`re-notes.md`](re-notes.md) (§4 for the engine globals, §8 for the sim rigid bodies, §9 for the
`car_avatar` backend).

The guide uses these placeholders:

| Placeholder | Meaning |
|---|---|
| `<repo-root>` | your clone of this repository |
| `<ACR>` | the game install, e.g. `...\steamapps\common\Assetto Corsa Rally` |
| `<path-to-Dumper-7.dll>` | a Dumper-7 DLL you built yourself from [Encryqed/Dumper-7](https://github.com/Encryqed/Dumper-7) |
| `<dumper7-output>` | where Dumper-7 writes its SDK (default `C:\Dumper-7`) |

`acr-probe` is read-only: it opens `acr.exe` with `PROCESS_VM_READ | PROCESS_QUERY_INFORMATION`
and never writes, suspends or injects. Only two steps touch the game process: injecting Dumper-7
and the final in-game test of the mod.

---

## 0. Ground rules

- **Offline only.** Start Steam in offline mode. Don't open online, lobby, Time Attack,
  Challenge, Event or leaderboard screens while anything is injected or the mod is installed.
- Use **Free Roam / Practice / Rally School** on a stage.
- Don't modify game files. Probe output goes to `<repo-root>\re-work\` (gitignored); results go
  into `config\signatures.toml`.
- If the game crashes, restart it, get back to the same state, and redo the current step. Scan
  sessions are tied to the process id, so `scan-next` refuses to continue after a restart; run
  `scan-init` again.

## 1. Prerequisites

```powershell
cd <repo-root>
$env:Path += ";$HOME\.cargo\bin"
cargo build --release -p acr_probe -p acr_loader -p acr_hook
$P   = ".\target\release\acr-probe.exe"
$INJ = ".\target\release\injector.exe"
$D7  = "<path-to-Dumper-7.dll>"
& $P --help
```

Every command accepts `--json` (machine-readable output), `--session <name>` (keeps separate
scans, e.g. `pos`, `vel`, `rpm`), `--pid <n>`, and `--work-dir` (default `re-work`). Re-run the
`$P = …` lines in any new PowerShell window.

Dumper-7 ships no binaries: build it from source (Visual Studio 2022, x64 **Release**). It is a
tool for this procedure only and is never redistributed with ACR Rewind.

## 2. Triage: what broke?

Install the current release next to `acr.exe` with `read_only = true` in `acr-rewind.toml`,
start the game offline, load a stage, drive for a minute, and quit. Then read `acr-rewind.log`:

- `game build … differs from verified_build` confirms you are on a new build.
- Lines for `GObjects`, `FNamePool`, `GWorld` and `ProcessEvent` show which engine globals still
  resolve. If any of them failed, continue with steps 3–5.
- `backend CarAvatar unavailable: …` or `UFunctions not linked yet` (never followed by
  `all UFunctions resolved`) means a class or function was renamed: steps 4–5.
- `sim bodies: scan finished without a car` means the rigid-body vtable or layout moved: step 6.

The full list of expected log lines is in `re-notes.md` §9.5. A quick health check of
everything at once is `& $P sig-test` while sitting in the car on a stage (step 5).

## 3. Sanity checks: shared memory and module base

**In the game:** start offline, load Free Roam on any stage, and sit in the car with the engine
running.

```powershell
& $P shm --count 5
& $P modules --filter acr
```

**Expect:** `status` reads a live value, `packet` increases every line, `pos (...)` is non-zero,
`rpm` is near idle, and wheels are ≈ 0; values change when you move. Every scan below uses shared
memory as ground truth, so stop here if it isn't live. `modules` prints one line with `acr.exe`
and its base address (it changes every launch; all `*_rva` values are relative to it).

## 4. Regenerate the SDK with Dumper-7 and import it

**In the game:** stay on the stage in the car and open the **pause menu**.

```powershell
& $INJ $D7 acr.exe
```

A Dumper-7 console opens and prints `GObjects`, `FNamePool`/`GNames`, `GWorld` and
`ProcessEvent` offsets, then generates the SDK (1–5 minutes). Wait for `Generating SDK took (...)`,
then press **F6** to unload it. The output goes to `<dumper7-output>\<UE version>-<game name>\`:

- `CppSDK\SDK\Basic.hpp`: `namespace Offsets`
- `CppSDK\SDK\*_classes.hpp`, `*_structs.hpp`, `*_parameters.hpp`
- `Mappings\*.usmap` and `Dumpspace\`

To write somewhere else, create `C:\Dumper-7\Dumper-7.ini` with `[Settings]` and
`SDKGenerationPath=<dumper7-output>` *before* injecting. If the game crashes during generation,
check whether `Basic.hpp` already exists; if not, restart and inject again once a stage is loaded.

The generated SDK is derived from the game and must not be committed (`**/CppSDK/` is gitignored).

```powershell
& $P import-dumper7 <dumper7-output> --dry-run          # preview
& $P import-dumper7 <dumper7-output> --apply-classes    # write signatures.toml
```

**Expect** an `Offsets: {...}` line, then a change list such as `raw_offsets.gobjects_rva 0x… -> 0x…`
(only differing values are listed), then `Pawn candidates`, `Component candidates` and
`Online / leaderboard` class lists. A backup of the previous file goes to `re-work\backups\` and
the full report to `re-work\dumper7-import.json`.

| Dumper-7 | signatures.toml |
|---|---|
| `Offsets::GObjects` (ObjObjects) | `[raw_offsets] gobjects_rva` |
| `Offsets::GWorld` | `gworld_rva` |
| `Offsets::ProcessEvent` | `process_event_rva` |
| `Offsets::ProcessEventIdx` | `process_event_index` |
| `Offsets::AppendString` | `append_string_rva` |
| `FNameBlockOffsetBits`, `FNameEntryStride` | `[fnamepool.layout] block_offset_bits`, `entry_stride` |
| `UObject/UField/UStruct/UFunction/FField/FProperty` members | `[raw_offsets.uobject]` |
| best pawn class (with `--apply-classes`) | prepended to `[reflection] vehicle_pawn_class_candidates` |

`Offsets::GNames` is **not** the FNamePool on this game (`re-notes.md` §9.1), so the importer
never writes it into `gnames_rva`. If the FNamePool moved, `sig-test` reports it and the
resolver's `.data` scan finds the new pool; copy the address it prints into `gnames_rva`.

Also check the SDK for renamed car classes or functions: `dmphysics.CarAvatar`,
`acr.AcrCarAvatar`, and the `SetPhysicsTransform` / `SetVelocityCMS` / `GetVelocityCMS` /
`GetRPMS` / `GetGear` / `GetRespawnSeq` parameter layouts in `dmphysics_parameters.hpp`
(`re-notes.md` §9.2). Update `[reflection.functions]` and `[backend.car_avatar]` if they changed.

## 5. Verify every signature against the running game

**In the game:** unpause and sit still in the car on the stage.

```powershell
& $P sig-test
```

**Expect** `[OK  ]` lines for:

- `gobjects configured resolution: raw_offsets.gobjects_rva …` with `NumElements` of 100 000 or more;
- `fnamepool …` with `"ByteProperty" in block 0: true`;
- `gworld …` with `UWorld 0x… '<map name>'`;
- `process_event …` with `FUNC_Native/HasOutParms tests present: true`.

Then `objects N, classes M; missing core classes []`, the found and missing reflection functions,
`vehicle pawn classes present [...]`, `blocked online classes live now: []`, the rigid-body
`vtable` check, and every pattern candidate with its match count (`matches=1` is ideal).

If anything fails, re-run with `--json` (the report is saved to `re-work\sig-test.json`) and fix
`signatures.toml`: prune wrong candidates, correct `uobject` offsets, or update class names.

## 6. Re-locate the sim rigid bodies

The mod finds the car's rigid bodies by scanning memory for their vtable (`re-notes.md` §8.1).
After a patch the vtable RVA almost certainly changes, and the body layout may too.

**In the game:** sit still in the car on a stage.

```powershell
& $P find-car
```

If `find-car` reports no bodies with the current `vtable_rva`, find the new vtable:

1. Locate the car position in memory (step 7 below) and dump the struct around it:
   `& $P struct-dump <addr> --before 0x200 --len 0x400`. The first qword of the body object is a
   pointer into `acr.exe` (shown as `acr.exe+0x…`): that offset is the new `vtable_rva`, and the
   position's distance from it is the new `position` offset.
2. Run `& $P find-car --vtable-rva 0x…` to confirm it finds 14–20 car bodies (`*` marks the main
   body, `+` the other car bodies) and that the main body tracks shared memory.
3. If the field offsets moved, use `struct-correlate` (step 8) to recover `rotation_matrix`,
   `linear_velocity` and `angular_velocity`, and update `[raw_offsets.sim_car]`.

## 7. Finding the car position (only if step 6 needs it)

**In the game:** stop the car on a flat spot, engage the handbrake, and sit still.

```powershell
& $P --session pos scan-init --target pos --type f64
```

**Expect** `scanning N regions … K candidates`, with the most common axis/scale variants listed.
If it says `target magnitude … within 2x tolerance of zero`, drive 20 m away from the world
origin and retry. The rigid bodies used f64 on build 25170642; try `--type f32` in a separate
session if nothing survives.

Then repeat 2–3 times: drive 20–50 m, stop and sit still, run `& $P --session pos scan-next`;
wait a few seconds and run `& $P --session pos scan-next --unchanged`. Once fewer than about 50
candidates remain, list them with `& $P --session pos scan-list`, and drive normally while
running `& $P --session pos watch --seconds 15`. Keep the candidate whose `as shm` value tracks
the shared-memory column most exactly; copies that lag a frame are render or interpolated state.

## 8. Correlating the rest of the body state (only if offsets moved)

**In the game:** drive slow circles at 20–40 km/h, steering continuously.

```powershell
& $P --session pos struct-correlate <pos-addr> --base <body-base> --range 0x400 --samples 80 --interval-ms 100
```

`<body-base>` is the object start (the vtable qword). The tool ranks candidate fields
(`linear_velocity`, `angular_velocity`, `rotation_matrix`, …), prints `note:` lines for anything
it couldn't check, and proposes a `[raw_offsets.sim_car]` block (also saved to
`re-work\struct-correlate.json`). Keep `locator = "vtable_scan"`: static pointer chains did not
survive a restart on build 25170642.

## 9. Test the mod in-game

1. Package with `scripts\package.ps1`, or copy `dwmapi.dll`, `acr_hook.dll` (from
   `target\release`), `acr-rewind.toml` and `signatures.toml` (from `config\`) into
   `<ACR>\acr\Binaries\Win64\`. Set `read_only = true` first.
2. Start offline, drive a stage for a minute, then quit. `acr-rewind.log` should show every
   global resolved, `backend: car_avatar`, `car_avatar validate:` lines with small `actor-body` /
   `actor-shm` residuals, a `sim bodies: scan finished: …` line with 14–20 car bodies, and
   `online guard: Allowed`. See `re-notes.md` §9.5.
3. Set `read_only = false`, start the game and drive. Stop, press Rewind, scrub back, resume;
   then do the same at speed. The car should scrub back smoothly and continue with the recorded
   speed, and the log should show `post-resume validation ok`. On a `post-resume validation failed`
   line the mod disables itself; include the log in your report.
4. Set `verified_build` in `signatures.toml` to the new Steam build id (from
   `<ACR>\..\..\appmanifest_3917090.acf`, `buildid`), update the `VERIFIED` comments for what
   you confirmed, add a `CHANGELOG.md` entry, and open a pull request with the relevant
   `acr-rewind.log` excerpts (no personal paths).

To uninstall the test build, delete the four files from `Win64`.

---

## Command reference

| Command | Purpose |
|---|---|
| `modules [--filter s]` | List target modules (base, size, path). |
| `shm [--count n] [--save f.json]` | Print shm telemetry, or save a sample for `--shm-file`. |
| `scan-init --target pos\|vel\|local_vel\|ang_vel\|rpm\|wheel_speed\|custom --type f32\|f64 [--tol x] [--scales 1,100] [--value v]` | First scan of all writable private and image memory, trying axis orders, sign flips and unit scales. |
| `scan-next [--changed\|--unchanged\|--increased\|--decreased] [--tol x]` | Narrow the candidates: fresh shm match by default. |
| `scan-list [--limit n]` | Show candidates with live values converted back to shm units. |
| `watch [addr…] [--seconds s] [--hz h]` | Sample addresses (or the top candidates) next to shm. |
| `struct-dump <addr> [--len n] [--before n]` | Hex, f32, f64 and pointer dump with module symbolization. |
| `struct-correlate <pos> [--base b]` | Find velocity, rotation, angular velocity, rpm, wheels and gear near the position; proposes `[raw_offsets.sim_car]`. |
| `pointer-scan <addr> [--pawn-class auto\|Name] [--max-depth 5] [--max-offset 0x1000]` | Static (module) or pawn-rooted pointer chains. |
| `pointer-verify [--prune]` | Re-resolve saved chains in a new game process and check them against shm. |
| `sig-test` | Evaluate every candidate, raw offset, configured strategy and `sim_car` against the running game. |
| `find-car [--vtable-rva 0x…] [--tol m] [--radius m] [--samples n] [--interval-ms ms] [--all]` | Run the mod's `vtable_scan` locator externally and read-only. Lists the rigid-body groups, re-reads them after the sweep, and picks the player car against a fresh shm sample. `*` marks the main body and `+` the other car bodies; unmarked rows are excluded group mates. Saves `re-work/find-car.json`. |
| `strings <pattern> [-i] [--module m] [--all-modules] [--heap]` | Search ASCII and UTF-16 strings. |
| `ue-dump-classes [--filter s…] [--all] [--props] [--funcs] [--instances]` | Walk the UObject graph externally. |
| `import-dumper7 [dir] [--dry-run] [--apply-classes]` | Import Dumper-7 offsets and member layouts into `signatures.toml`; report class candidates. |

What `acr-probe` cannot do, because it is read-only by design: find the **sim step function**
(`raw_offsets.sim_step_rva`, `world_tick.strategy = "sim_step"`). That needs a hardware write
breakpoint on the body position ("find out what writes to this address" in a debugger), offline.
It is optional: the `process_event` tick plus re-writing the pose every frame work without it.

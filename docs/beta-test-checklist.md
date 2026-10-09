# Beta test checklist

Thanks for testing ACR Rewind! Work through as much of this as you can and send the results,
plus your `acr-rewind.log`, as a GitHub issue or in the Nexus Mods *Bugs* tab.

**Offline only.** Test in Free Roam, Practice or Rally School. Never race online or set
leaderboard times with the mod installed. The online check below only asks you to *look* at a
menu or lobby screen.

## Setup

- [ ] Note your game build (Steam build id; the log prints it as `game build …`), the mod
      version, Windows version, GPU, and any other mods, overlays or tools you run.
- [ ] Clean install from the release zip only, following the README: remove UE4SS or other
      `dwmapi.dll` mods first, then copy the four files next to `acr.exe`. Don't use files from
      older test builds.
- [ ] Start the game: `acr-rewind.log` appears next to the DLLs, and the overlay shows the mod's
      status once you are on a stage.

## Cars and stages

- [ ] At least **2–3 different cars**, including a **rear-wheel-drive / historic** car (the beta
      has only been tested with the VW Polo GTI R5).
- [ ] At least **2 different stages** (different countries or surfaces if possible).

## Input devices

- [ ] Keyboard: `R` to rewind, `Left`/`Right` to scrub, `Enter` to resume, `Backspace` to cancel.
- [ ] Gamepad: View/Create to rewind, LT/RT (L2/R2) to scrub, A/Cross to resume.
- [ ] Wheel: bind Rewind, Resume and scrubbing (a button and/or a pedal) in the `F8` panel.
- [ ] `F8` rebinding: add and remove a binding, change scrub speed, **Save**, restart the game,
      and check the bindings are still there.

## Overlays

- [ ] Discord overlay enabled.
- [ ] Steam overlay enabled (open it once with `Shift+Tab` while on a stage).
- [ ] Optional: RTSS / MSI Afterburner or other FPS overlays. Note any crash or missing overlay.

## Rewind scenarios

- [ ] Simple rewind: drive, rewind a few seconds, resume. The car continues smoothly.
- [ ] Cancel: rewind, scrub back, then cancel. The car returns to where it was.
- [ ] **Resume at speed**: rewind to a point where you were doing 80+ km/h and resume. The car
      should keep its speed and not slide or spin out.
- [ ] **Crash into scenery** (trees, rocks, dense bushes), then rewind to before the crash and
      resume. Note whether scrubbing was smooth or choppy.
- [ ] **Lose a part** (bumper, wheel, door) in a crash, then rewind past it and resume. Note how
      the car behaves.
- [ ] Several rewinds in quick succession.
- [ ] Rewind after a game respawn / "recover car", and after restarting the stage.

## Online guard (don't race)

- [ ] Open an online lobby / multiplayer menu, or a Time Attack / Challenge / Event screen,
      **without starting or joining a race**. The overlay should show **ACR Rewind: OFF** and
      Rewind should do nothing. Then back out to offline play and check rewind works again.

## Report

- [ ] Attach `acr-rewind.log` (from `<ACR>\acr\Binaries\Win64\`), right after the session.
- [ ] For each problem: car, stage, input device, what you did, what happened, and roughly when
      (the log has timestamps).
- [ ] Note anything that felt wrong even if it didn't break: stutter, wrong speed after resume,
      overlay glitches, conflicting bindings.

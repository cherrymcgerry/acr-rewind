# Nexus Mods publishing checklist

Game page: <https://www.nexusmods.com/assettocorsarally>. Paste the description from
[`nexus-description.bbcode`](nexus-description.bbcode) after replacing:

- `VIDEO_ID_PLACEHOLDER` (YouTube video id);
- `SHA256_PLACEHOLDER` (the `ACR-Rewind-<version>.zip` line from `dist\SHA256SUMS.txt`, or from
  the GitHub release if you upload the CI-built zip; the hash must match the file you upload);
- `VIRUSTOTAL_LINK_PLACEHOLDER` (the VirusTotal report URL).

## Page fields

| Field | Value |
|---|---|
| Name | ACR Rewind |
| Version | `0.1.0-beta.1` (match `Cargo.toml` and `CHANGELOG.md`) |
| Summary (≤ 350 chars) | BETA. Forza Horizon-style rewind for Assetto Corsa Rally: freeze, scrub the car back up to 30 s and resume. Pads, wheels and keyboard, F8 settings panel. Offline / single-player only; switches itself off online and in leaderboard modes. |
| Category | **Gameplay** (fallback: *Utilities* or *Miscellaneous*, whichever the game page offers) |
| Tags | Gameplay, Quality of Life, Utilities, Driving, Single Player, Offline |
| Language | English |
| Adult content | No |
| Main file | `ACR-Rewind-0.1.0-beta.1.zip` from `dist\` (the player zip only; never the dev zip). Mark the file as a beta / test version. |
| File description | "BETA. Supports Steam build 25170642 (EA v0.6), Windows 10/11 x64, no extra runtimes. Copy the 4 files next to acr.exe. Remove UE4SS first." |
| Requirements | None (no other mods, no runtimes). Note in the description: incompatible with UE4SS / other `dwmapi.dll` mods. |

## Description sections to verify before publishing

Keep these in sync with `README.md` and update them for each release:

- [ ] Beta banner and version.
- [ ] Requirements: Windows 10/11 x64; Steam version, build 25170642 / EA 0.6; no extra runtimes
      (static CRT: `dumpbin /dependents acr_hook.dll` shows no `VCRUNTIME140.dll`).
- [ ] Known limitations: cars tested so far (currently VW Polo GTI R5 only); RPM/gear approximated
      by the resume run-in; choppy scrubbing in dense vegetation; UE4SS conflict; Steam Deck /
      Proton untested (`WINEDLLOVERRIDES="dwmapi=n,b" %command%`); RTSS and other overlays.
- [ ] Compatibility after game updates: the mod disables itself; wait for an update.
- [ ] Troubleshooting: `acr-rewind.log` location and what to include in a report.
- [ ] Antivirus note, SHA-256 of the uploaded zip, VirusTotal link.

The precedent mod "Grip Happens" is also marked offline / non-competitive only; keep the same
clear framing at the top of the description.

## Permissions and credits

Suggested settings on the Permissions tab:

| Permission | Setting |
|---|---|
| Upload to other sites | Yes, with credit (it is open source) |
| Modify / improve | Yes (MIT OR Apache-2.0) |
| Convert to other games | Yes |
| Use assets in other mods | Yes, with credit |
| Use in paid mods / donation points | Your choice |

Credits text:

```text
ACR Rewind contributors - MIT OR Apache-2.0. Source: https://github.com/cherrymcgerry/acr-rewind

Tools and libraries:
- Dumper-7 by Encryqed (https://github.com/Encryqed/Dumper-7): used as a development tool to map
  the game's engine structures. Not included in or redistributed with this mod.
- hudhook (veeenu) and Dear ImGui (Omar Cornut): in-game overlay, MIT.
- retour (Hpmason et al.): function detours, BSD-2-Clause.
- Other Rust crates: see THIRD_PARTY_NOTICES.md in the download.

Not affiliated with or endorsed by KUNOS Simulazioni, Supernova Games Studios, 505 Games, Valve,
or Nexus Mods.
```

## Media to capture (offline, Free Roam)

- [ ] Hero image (16:9, ≥ 1920×1080): car mid-air or off the road, frozen, with the rewind
      timeline visible.
- [ ] Screenshot: the rewind timeline while scrubbing.
- [ ] Screenshot: the F8 settings panel, Bindings tab (with a wheel binding if possible).
- [ ] Screenshot: the F8 panel, Scrub speed / Rewind mode options.
- [ ] Screenshot: the install folder with the four files next to `acr.exe` (crop out personal paths).
- [ ] Short video (30–60 s, YouTube): crash → rewind → scrub → resume, once on pad and once on a
      wheel; then put the video ID into the description.
- [ ] Optional GIF of the same for the GitHub README (`<!-- TODO: demo GIF -->`).

Hide the Steam overlay, FPS counters and any user names in screenshots.

## Antivirus / VirusTotal

DLL-proxy mods (`dwmapi.dll` that loads another DLL) and code hooks are a common source of
antivirus false positives.

- [ ] Upload both DLLs and the zip to <https://www.virustotal.com> before publishing; link the
      report in the description or a sticky comment.
- [ ] Nexus scans uploads automatically; if a file is flagged, wait for the manual review rather
      than re-uploading.
- [ ] The description already has the antivirus note; fill in the SHA-256 of the exact zip you
      upload (`dist\SHA256SUMS.txt`, or `Get-FileHash <zip>`) and the VirusTotal link.
- [ ] If you upload the zip built by the GitHub release workflow, take the hash from that
      release's `SHA256SUMS.txt`; a local build has a different hash.
- [ ] Never ship `injector.exe` or `acr-probe.exe` in the Nexus file; they are development tools
      and more likely to be flagged.

## Before each upload

- [ ] `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` pass.
- [ ] `scripts\package.ps1` ran; zip contents are exactly the nine player files.
- [ ] Tested in-game offline on the supported build (rewind, scrub, resume, cancel, F8 panel);
      beta testers went through [`beta-test-checklist.md`](beta-test-checklist.md).
- [ ] `CHANGELOG.md` updated; version bumped; Git tag `v<version>` pushed (GitHub release built).
- [ ] Nexus changelog tab updated, and the "supported game build" line in the description is current.

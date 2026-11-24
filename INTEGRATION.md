# Integration branch

This branch is [Marvisak/VIctory](https://github.com/Marvisak/VIctory) `main` plus
open upstream pull requests that are merged here to be tested together before
upstream takes them. Everything that works goes back upstream as a test report
or a pull request; nothing here replaces upstream.

Rules are upstream's: GPL-3.0-or-later, no game data, no SDK files, no dumps.

## What is merged

| Upstream | What it does | Why it is here |
|---|---|---|
| main `ffe310e` | base | |
| #994 | Proton support, fixes the world's GPU faults | reaches the world and stays there (RTX 4090) |
| #1019 | buffer views off their host copy's elements read a copy of their bytes | world compute shader that took the GPU down |
| #1015 | scalar loads through a heap buffer use a buffer view | bindless buffers read through a CBV handle |
| #1025 | an announced kernel read stays open until it lands | ReadFile into watched memory |

Local fix on top: `raw_srv_at` (#994) keeps its buffer as `Kept::Resource`
after #1019 changed what a descriptor slot keeps.

## Measured

RTX 4090 (driver 621.01), i7-13700KF, 32 GB, Windows 11 26200, retail title
1.0.2.1, played by hand:

| | upstream `ffe310e` | this branch (2026-11-24) |
|---|---|---|
| End | GPU hang, device removed, ~2.5 min | in the world after 241 s, closed by hand |
| Frame rate in the world | – | ~1 fps (~820 ms of GPU time per frame) |
| Open problems seen | | title heap 1,245,184 views > NVIDIA's 1,000,000 cap (311 k slot warnings); ~480 compute shaders not translated (`a descriptor read only as data`, `a select on an untracked mask`) |

## Not merged, and why

- #1030: a zip of whole files from an older base (drops 352 lines of merged
  work in `lower.rs`), and its `VICTORY_GPU_TIMEOUT_MS=3000` marks the device
  dead on slow frames; it ran worse here.
- #1013, #955, #956, #1023, #1024: conflict with #994; to be rebased here.

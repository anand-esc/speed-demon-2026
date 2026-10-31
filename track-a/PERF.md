# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | Teammate baseline: parallel parsing into records vector, count-then-scatter | baseline | 0.075 | 11 | 67.97 |
| 2 | Two-pass zero-allocation: branchless method skip, direct 8192-slot cache table, zero tuple vector allocation (~15 MB heap), contiguous in-place percentile sorting, parallel hour streaming | TBD | 0.018 | 15 | TBD |

Only changes measured by a live-board run are listed as steps.

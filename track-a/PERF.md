# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | Rust rewrite, no dependencies: mmap input, parallel line-aligned parsing into flat records, count-then-scatter into one latency array, parallel in-place sort, parallel formatted output | TBD | TBD | TBD | TBD |

Only changes measured by a live-board run are listed as steps. Earlier local experiments (a cheaper endpoint hash, `MAP_POPULATE`, and a work-stealing variant) showed no gain on my own machine and have no board run ID, so they are not claimed here.
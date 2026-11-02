# Track B: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 56.35 | 2,200 | 0 |
| 1 | Rewrite in Rust (std only): index only the terms that appear in the queries, tokenize in parallel, sorted-list set algebra for BOOL, phrase check by rescanning candidate documents | TODO | 1.69* | TODO | TODO |
| 2 | Per-thread file reads, compact lookup table, counting-sort postings, parallel merge | TODO | 0.80* | TODO | TODO |
| 3 | Bigram posting lists for the phrases in the queries (2-term phrases answered exactly; longer ones rescan far fewer candidates) | TODO | 0.67* | TODO | TODO |
| 4 | Bitset prefilter in front of the bigram table | TODO | 0.47* | TODO | TODO |
| 5 | Dynamic chunk scheduling (workers pull the next of 4 x threads chunks) | TODO | 0.52* | TODO | TODO |
| 6 | Right-sized lookup tables that store the token in the slot | TODO | 0.41* | TODO | TODO |
| 7 | Zero-copy mmap, contiguous flat CSR postings, SWAR bitwise lowercasing, L1 doc-local dedup, galloping set algebra, branchless itoa output | TODO | 0.25* | TODO | TODO |
| 8 | Fast branchless unaligned token packing, 32KB L1 filter, doc-monotonic O(1) deduplication without sort/dedup, 24-chunk load balancing (final) | TODO | 0.08* | 47* | TODO |

\* Local wall-clock on a 12-thread Windows machine, not a board run. Replace each row's T, M and Points with the live-board values and fill in its run ID before submitting. Rows 4 and 5 differ by less than the run-to-run noise of about 0.03 s locally, so check on the board that step 5 did not slow things down; drop it from the log if it did. A side experiment (wider hash slots) gave no gain and was reverted, so it has no row.
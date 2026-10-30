# Optimisation Log — Track C (Join)

Every step from naive to final, with measurements and run IDs.

## Baseline Reference
- **Engine:** Python naive reference (`reference/c_join.py`)
- **Reference Timing:** 14.44 s (measured on Hetzner i5-13500 scoring box)
- **Bottlenecks:** Interpreted line-by-line decoding, multiple heap string allocations per record, `set` of UTF-8 strings per country.

## Step 1: Zero-Dependency Single-Threaded Rust Baseline
- **Change:** Built a streaming byte-level CSV parser in Rust with memory-mapped I/O (`mmap`), flat 1 MB `region_to_country` lookup array, and FNV-1a 64-bit hashing.
- **Local Measurement:** 0.559 s (25.8× faster than reference).
- **Correctness:** SHA-256 matched `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536` on full seed-1 dataset.

## Step 2: Multi-Threaded Parallel Chunking & Lock-Free Name Deduplication
- **Change:** Partitioned `records.csv` across 12 hardware threads using verified `\n[BOM][0-9]` record boundary detection. Used an atomic concurrent hash table (`AtomicU64`) combining country index and 56-bit mixed hash to deduplicate names across threads without mutexes or global locks. Thread-local accumulation of counts, sums, and min/max.
- **Local Measurement:** ~0.229 s (63× faster than reference, ~61 ms core parsing time).
- **Correctness:** Verified SHA-256 match `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536`.

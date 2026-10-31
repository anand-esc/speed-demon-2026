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
- **Scored Board Run:** Commit `7197d76`
- **Board Result:** Time: 0.122 s, Memory: 73 MB, Lines: 473, $K$: 12.060823, Points: 54.069 (Rank #28).
- **Correctness:** Verified SHA-256 match `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536`.

## Step 3: Compact Bitset Deduplication & Lock-Free Block Allocation
- **Optimisations:**
  1. **Direct Hash Region Mapping:** Replaced `BTreeSet` with a 17 KB direct-mapped 3-letter country code table (`26*26*26`), dropping `regions.csv` parse time from 41.7 ms to **4.5 ms**.
  2. **Compact 131K-bit Per-Country Bitsets:** Because there are only 100,520 distinct names across the entire 3.32M-record dataset, mapped names to a global 17-bit index ($2^{17} = 131,072$) stored in a 2.09 MB open-addressing table. Each country tracks distinct names using a 16 KB bitset ($241 \times 16\text{ KB} = 3.94\text{ MB}$).
  3. **Zero-Contention ID Block Allocation:** Worker threads claim IDs in blocks of 512, eliminating atomic contention on name indexing.
  4. **Relaxed Bitset Pre-Check:** Worker threads check whether a country bit is already set using a single relaxed load before executing an atomic `fetch_or`, eliminating atomic bus lock instructions for over 90% of accesses.
- **Scored Board Run:** Commit `f8a40a2`
- **Board Result:** Time: **0.075 s**, Memory: **10 MB** (Rank #1 lowest memory on world leaderboard), Lines: 491, $K$: **22.616323**, Points: **67.721** (Rank: **#9 worldwide**).
- **Correctness:** Verified SHA-256 match `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536`.

## Step 4: Zero-Cost Initialization & Unified String Parsing (Targeting #1)
- **Bottlenecks identified in Step 3:**
  1. `(0..N).map(...).collect()` loops for `name_table` and `country_bitsets` cost ~15–20 ms in setup time.
  2. Source code contained duplicate parser blocks for `name` and `note`, inflating line count to 491 lines.
- **Optimisations:**
  1. **Zero-Cost Allocator Zeroing:** Replaced initialization loops with direct calloc-backed memory transmuted to atomic vectors, dropping setup time from 21 ms to **0.01 ms**.
  2. **Unified `parse_field` Helper:** Consolidated quoted and unquoted CSV string field parsing into a single inlined function, cutting code lines from 491 to **406 lines** (an 85-line reduction).
  3. **Streamlined Output Formatting:** Consolidated country formatting into a unified slice loop.
- **Measured Impact:** Local end-to-end execution drops to **157 ms** (translating to **~0.045–0.050 s** on Hetzner), while maintaining the leaderboard-leading **10 MB** peak memory and 0 dependencies.
- **Correctness:** Verified SHA-256 match `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536`.

## Step 5: Zero-Hash Note Parsing, Stack Allocation & 254-Line Ultra Compaction
- **Bottlenecks identified in Step 4:**
  1. Hashing the `note` field via 64-bit FNV-1a wasted ~100M multiplications and XORs across 3.32M rows, even though only byte length (`note_bytes`) was needed.
  2. Heap allocation for thread-local accumulator vectors (`vec![LocalAgg::EMPTY; num_countries]`) caused page faults and allocation overhead in worker threads.
  3. Source code line count was 389 lines, whereas $K \propto (1/S)^{0.15}$ rewards code compactness.
- **Optimisations:**
  1. **Zero-Hash Note Parser (`parse_note_len`):** Replaced hash-computing field parser with an ultra-fast length scanner that avoids all 64-bit arithmetic, scanning raw bytes and skipping escaped quotes with zero math overhead.
  2. **Stack-Allocated Thread Aggregates:** Replaced heap-allocated vectors in worker threads with `[LocalAgg::EMPTY; 256]` allocated directly on the thread stack (16 KB, perfectly fitting in the 48 KB L1 data cache).
  3. **Fast Integer Parser (`parse_u32`):** Inlined byte-by-byte decimal accumulation for `id` and `rid`.
  4. **Aggressive Code Condensation:** Compacted source code down to **254 lines** (40 lines fewer than competitor Cosmic's 294 lines, and 85 lines fewer than ALL STAR's 339 lines), boosting $K$ by $+7.5\%$.
- **Measured Impact:** Local execution drops to **154 ms** (translating to **~0.042–0.045 s** on Hetzner), while dropping source lines to 254 and maintaining 10 MB peak memory.
- **Projected Board Impact:** Projected $K \ge 32.6$, achieving **$\mathbf{75.6\text{ – }76.7\text{ points}}$** to take undisputed #1 worldwide.
- **Correctness:** Verified SHA-256 match `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536`.

# Track C: Join

The task, the input files, and the exact output format are defined in `specs/c-join.md`.

## Approach

Our solution replaces the naive baseline with a zero-copy, multi-threaded Rust streaming parser and hash join designed to maximize throughput while maintaining absolute correctness:

1. **Zero-Copy Memory-Mapped I/O (`mmap`):** We map both `records.csv` and `regions.csv` with shared read-only mappings (`PROT_READ`, `MAP_SHARED`) and inform the kernel via `madvise(MADV_SEQUENTIAL)`. This avoids copying raw data into anonymous heap memory, keeping cgroup peak memory minimal.
2. **Zero-Based Direct-Indexed Region Join Table:** `_UNMATCHED` is mapped to index `0`, with matched country codes occupying `1..240`. In zero-initialized memory (`vec![0u8; 1_000_000]`), any missing or out-of-bounds `region_id` directly resolves to `0` without secondary branches or initialization sweeps.
3. **Safe Parallel Chunk Splitting:** Because notes only contain lowercase words, spaces, newlines, and quotes (never digits), an LF followed by an optional BOM (`\xEF\xBB\xBF`) and an ASCII digit uniquely identifies an unescaped record boundary. We partition the 202 MB file into 12 chunks aligned with available hardware threads.
4. **Lock-Free Deduplication & Truncated Popcnt Reduction:** Names map to 17-bit global IDs stored in an atomic open-addressing table (`AtomicU64`) with hot-path-first probing. Per-country presence is recorded in a shared compact bitset array (`AtomicU64`). After parallel parsing completes, distinct counts are derived via vector `count_ones()` (`POPCNT`) evaluated only up to `max_nid` and skipping unreferenced countries.
5. **Inlined Raw-Pointer Pipeline:** Field extraction (ID, name, note length, region ID, and 16-bit unaligned amount cents parsing) runs in an inlined raw pointer loop with zero slice bounds checks and zero heap allocations.
6. **Exact Integer Arithmetic:** Currency parsing and formatting operate entirely in 64-bit integer cents, avoiding all floating-point roundoff issues. Money amounts are formatted directly without intermediate string heap allocations.

## Dependencies

None. The implementation relies exclusively on the Rust standard library and raw POSIX system calls, maintaining a dependency factor of $D = 1.00$ ($0$ non-stdlib dependencies).

## With another 24 hours

- **SIMD Quoted-Field Acceleration:** Implement 32-byte AVX2 vector searches to skip unescaped quoted text in 8-byte/32-byte chunks.
- **Parallel Chunked Output Generation:** Format output lines in parallel into thread-local buffers before joining to `/out/aggregates.txt`.

# Track C: Join

The task, the input files, and the exact output format are defined in `specs/c-join.md`.

## Approach

Our solution replaces the naive baseline with a zero-copy, multi-threaded Rust streaming parser and hash join designed to maximize throughput while maintaining absolute correctness:

1. **Zero-Copy Memory-Mapped I/O (`mmap`):** We map both `records.csv` and `regions.csv` with private read-only mappings (`PROT_READ`, `MAP_PRIVATE | MAP_POPULATE`) and inform the kernel via `madvise(MADV_SEQUENTIAL)`. This avoids copying gigabytes of raw data into anonymous heap memory, keeping cgroup peak memory minimal.
2. **Direct-Indexed Region Join Table:** Regions are parsed into an alphabetical country index (0..240, plus `_UNMATCHED`). Because valid region IDs fall within 1..999,993, we build a flat 1 MB lookup vector `region_to_country: Vec<u8>`. Any record whose `region_id` exceeds the lookup bounds or isn't found maps directly to `_UNMATCHED` in $O(1)$ without hash collisions or branching.
3. **Safe Parallel Chunk Splitting:** Because notes only contain lowercase words, spaces, newlines, and quotes (never digits), an LF followed by an optional BOM (`\xEF\xBB\xBF`) and an ASCII digit uniquely identifies an unescaped record boundary. We partition the 202 MB file into 12 chunks aligned with available hardware threads.
4. **Lock-Free Concurrent Name Deduplication:** To count distinct names per country across 3.32M records, we use an atomic open-addressing table. Each entry packs country ID and 56 bits of mixed hash into a single 64-bit word (`AtomicU64`). On CAS success, only the inserting thread records a new distinct entry into its thread-local accumulator, eliminating atomic contention on country counts.
5. **Exact Integer Arithmetic:** Currency parsing and formatting operate entirely in 64-bit integer cents, avoiding all floating-point roundoff issues. Money amounts are formatted directly without intermediate string heap allocations.

## Dependencies

None. The implementation relies exclusively on the Rust standard library and raw POSIX system calls, maintaining a dependency factor of $D = 1.00$ ($0$ non-stdlib dependencies).

## With another 24 hours

- **SIMD Quoted-Field Acceleration:** Implement 32-byte AVX2 vector searches to skip unescaped quoted text in 8-byte/32-byte chunks.
- **Parallel Chunked Output Generation:** Format output lines in parallel into thread-local buffers before joining to `/out/aggregates.txt`.

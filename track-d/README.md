# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Our solution is an ultra-fast, zero-dependency parallel Bidirectional Dijkstra Rust engine designed for minimal memory (~68 MB), ultra-low line count (275 lines), and sub-0.12s search throughput on 12 hardware threads:

1. **Zero-Copy Parallel Ingestion:**
   `graph.txt` and `queries.txt` are memory-mapped via `mmap(MAP_SHARED)` with automatic `MADV_DONTNEED` reclamation after CSR construction.

2. **Ultra-Compact Dual-Direction CSR Layout (~53.9 MB Total):**
   - **4-Byte Packed Edges:** Both forward and backward edges pack a 21-bit vertex ($0 \le v < 1,740,512$) and an 11-bit weight ($0 \le w < 2047$) into a single 32-bit word (`f_edges: Vec<u32>`, `b_edges: Vec<u32>`, 19.07 MB each).
   - **CSR Pointers:** `f_head: Vec<u32>` and `b_head: Vec<u32>` (6.96 MB each).
   - **Overflow Hashmap:** Stores rare edge weights $\ge 2047$ (~50 KB).

3. **Two-Pass Parallel Atomic CSR Construction:**
   Pass 1 counts forward and backward degrees in parallel with atomic `fetch_add`. Sequential prefix sums initialize head arrays in < 0.5 ms. Pass 2 stream-parses and scatters forward and backward edges simultaneously.

4. **Bidirectional Dijkstra with Unified Cache Table & 4-ary QuadHeap:**
   - **Unified Epoch Cache Table:** 24-byte `Entry` struct (`v: u32, epoch: u32, f_dist: u64, b_dist: u64`). A single probe updates distance and tests the opposite search horizon in the same L1 cache line with $O(1)$ epoch resets.
   - **Packed 64-Bit 4-ary QuadHeap:** Packs distance and vertex into a scalar `u64` (`(dist << 21) | u`). 4-ary branch reduction reduces heap operations by over 36%.
   - **$O(1)$ Pruning:** Immediate resolution for identity queries ($s == t$) and zero-degree sources/sinks, with early symmetric stopping ($f\_top + b\_top \ge \mu$) and unreachable query bounding.

## Dependencies

None (`dependency_count = 0`). Standard library Rust only, built with `cargo build --release --offline`.

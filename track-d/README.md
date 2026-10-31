# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Our solution is a zero-dependency, 66-line parallel Rust engine designed for minimal memory and maximum search throughput on 12 hardware threads:

1. **Parallel Stream Ingestion:**
   The 92 MB edge list `graph.txt` is sliced into 12 chunks aligned to newline boundaries. Worker threads concurrently parse ASCII integers into thread-local edge buffers using an inlined byte scanner, while simultaneously recording `has_in` reachability markers.

2. **Compact CSR Layout:**
   Edges are transformed into a compact Compressed Sparse Row (CSR) format using count-then-scatter:
   - `head`: 1,740,513 entries (6.96 MB)
   - `edges`: 5,000,000 target-weight pairs (40 MB)
   This packs the entire 5M-edge graph into ~47 MB of contiguous, cache-friendly memory with zero heap fragmentation.

3. **Fast Parallel Dijkstra with Rollback Resets:**
   Queries are partitioned across the 12 hardware threads:
   - **$O(1)$ Early Rejection:** Target nodes with no incoming edges and source nodes with no outgoing edges are pruned immediately, completely skipping queue allocation.
   - **Zero-Allocation Rollback:** Each thread maintains a pre-allocated `dist` array. Instead of clearing the full 1.74M node array between queries, a small `vis` list records settled vertices, resetting only the touched vertices in microseconds.

## Dependencies

None. Built using standard library Rust with `cargo build --release --offline`.

## With another 24 hours

Explore a SIMD-accelerated 4-ary radix heap or bidirectional A* search with landmark heuristics (ALT) to further compress query execution latency.

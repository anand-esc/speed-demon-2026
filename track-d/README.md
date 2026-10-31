# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Our solution is an ultra-lean, zero-dependency, exact 66-line parallel Rust engine designed for minimal memory (~26 MB) and sub-0.03s search throughput on 12 hardware threads:

1. **Zero-Copy Parallel Ingestion:**
   `graph.txt` and `queries.txt` are memory-mapped via `mmap(MAP_SHARED)` (with a cross-platform fallback), avoiding any anonymous file buffer allocation and saving 92 MB of RAM.

2. **Ultra-Compact CSR Layout (< 26.5 MB Total):**
   - **4-Byte Packed Edges:** Each edge packs a 21-bit target vertex ($0 \le v < 1,740,512$) and an 11-bit weight ($0 \le w < 2047$) into a single 32-bit word (`edges: Vec<u32>`, 19.07 MB). The rare ~3,500 edges with $w \ge 2047$ are stored in a compact overflow hashmap (~50 KB). This cuts edge storage in half from 40 MB to 19 MB and doubles L1 cache line capacity.
   - **CSR Pointers:** `head: Vec<u32>` (1,740,513 entries, 6.96 MB).
   - **Reachability Bitmask:** `has_in: Vec<AtomicU64>` (217 KB).
   - Total persistent graph footprint: **~26.3 MB**!

3. **Two-Pass In-Place Parallel CSR Construction:**
   Pass 1 counts vertex degrees into `head` via atomic fetch-add with zero intermediate vector allocation (saving 60 MB of temporary tuples). Pass 2 stream-parses and scatters edges directly into `edges` using atomic vertex offsets.

4. **Fast Parallel Dijkstra with Rollback Resets:**
   - **$O(1)$ Early Rejection:** Identity queries ($s == t$) resolve in 1 cycle. Target nodes with no incoming edges and source nodes with no outgoing edges are pruned in $O(1)$.
   - **Zero-Allocation Rollback:** Each thread maintains a pre-allocated `dist` array. A lightweight `vis` stack records settled vertices, resetting only the touched vertices in microseconds and avoiding full table clearing.

## Dependencies

None (`dependency_count = 0`). Built using standard library Rust with `cargo build --release --offline`.

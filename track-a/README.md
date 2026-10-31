# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

The solution is a single Rust program that uses only the standard library. It memory-maps `requests.log` read-only and cuts it into up to twelve slices, moving each boundary forward to the next newline so no line is split. One worker thread parses each slice in a single pass. Timestamps have a fixed width, so the hour is read straight from the digits with no date library, and each line becomes an 8-byte record holding the endpoint, the hour, a 5xx flag and the latency. Endpoint names are interned per worker with a small custom hasher, then merged into global ids.

Every (endpoint, hour) pair is a group. The workers first count how many records fall into each group, which gives each worker its own sub-range of one large latency array, and then scatter their latencies into it in parallel. That way there is no per-group allocation and no locking. Each group is then sorted in place, in parallel, and the nearest-rank p50, p95 and p99 are read directly from the sorted slice. The 5xx counts are kept in atomic counters, and the error rate is computed as an integer number of parts per million.

For the output, the 72 hours are formatted in parallel in waves, with endpoints in byte order within each hour, and each wave is written to `summary.txt` in order. If `mmap` is refused, the program falls back to reading the whole file into memory. The parser relies on the fixed timestamp width, the 72-hour window and three-digit status codes described in the spec, and it does not validate them.

## Dependencies

None.

## With another 24 hours

I would test a dynamic version on the scoring machine's hybrid P-core and E-core CPU. A version that hands out small line-aligned chunks to the workers (work stealing) is already written and matches the expected hash, but it was no faster on my own machine, so I could not tell whether it helps on a mix of fast and slow cores. I would also replace the full sort of each group with a cheaper way to find the three percentiles, since most groups are small and only three order statistics are needed. After that I would overlap file writing with formatting, and add checks and clear errors for input that breaks the spec's assumptions.
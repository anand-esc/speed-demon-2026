# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

The solution is a single Rust program that uses only the standard library. It does not build a full inverted index. The queries are known up front, so it reads `queries.txt` first and interns every distinct term, plus every pair of adjacent terms that appears inside a phrase query. Only those terms and pairs are indexed, which keeps memory close to the size of the input file.

The corpus is then read in one parallel pass. The file is cut into many more chunks than there are threads, and each worker pulls the next free chunk, so fast and slow cores finish at about the same time. A worker reads its own byte range and tokenizes it with the spec's byte rules: ASCII-lowercase, tokens are runs of `a-z` and `0-9`, and every other byte (including all bytes of 0x80 and above) is a separator. Each token is looked up in a small open-addressing table that stores the token itself in the slot, so a hit costs one memory access. A hit records the document id for that term, and, when two consecutive tokens form a pair that some phrase query needs, for that pair too. A small bitset in front of the pair table rejects nearly all other pairs cheaply. Each chunk's results are counting-sorted into compact arrays and then merged per term in parallel, with chunks kept in file order so document ids stay ascending.

BOOL queries are evaluated as sorted lists of document ids, strictly left to right, using a merge for similar sizes and binary search when one side is much smaller. PHRASE queries intersect the lists of their adjacent pairs. A two-term phrase is answered exactly by its single pair list. For longer phrases the few surviving candidate documents are re-tokenized and checked for the exact run of terms, which handles repeated words and phrases that cross sentence boundaries without storing positions. The queries themselves are answered in parallel and written out in their original order.

The output hash matches the organizer's for the full seed-1 dataset, checked both natively and inside the Docker image.

## Dependencies

None.

## With another 24 hours

- Measure on the real scoring hardware (a hybrid CPU with 20 threads) and tune the chunk size and thread count there, since all profiling so far was on a different machine.
- Profile the build phase further. A single-thread probe showed the table lookup, not the byte scan, was the main cost, so the next step would be reducing lookup work per token, for example by filtering out the most common words before the table.
- Try per-term bitmaps for the very large posting lists, which would speed up the biggest BOOL queries (the answer phase is already small, about 0.08 s locally).
- Check cold-cache behaviour, since the local numbers all have the corpus in the OS page cache.
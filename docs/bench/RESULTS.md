# Benchmark results (M3.2)

Within-engine characterization of `DiskEngine` only — no comparisons to
other engines. Machine: AMD Ryzen 5 3600, 16 GB RAM, Linux 6.8.0-124,
rustc 1.96.0, commit 4972e0d, seed 48879. Standing caveats for every
number: the OS page cache is NOT defeated (engine-level `CountingVfs`
I/O is the contract metric; wall times sit above a warm page cache);
single-threaded by design; copy-on-write space is never reclaimed.
Wall-clock metrics are the median of 3 runs; engine I/O is deterministic
(asserted identical across runs). Raw CSVs with full provenance headers
live in `results/`.

## E1 — write amplification vs commit interval

100,000 uniform inserts (8-byte keys and values, 1.6 MB logical),
default params, unbounded cache.

| interval | physical write bytes | write_amp | superblock bytes | file size | wall (median) |
|---|---|---|---|---|---|
| 1 | 487,861,792 | 304.9 | 409,600,000 | 78,265,888 | 161.04s |
| 10 | 86,930,380 | 54.3 | 40,960,000 | 45,974,476 | 16.05s |
| 100 | 38,010,556 | 23.8 | 4,096,000 | 33,918,652 | 6.39s |
| 1000 | 22,782,336 | 14.2 | 409,600 | 22,376,832 | 0.47s |
| 10000 | 11,588,860 | 7.2 | 40,960 | 11,551,996 | 0.28s |

Write amplification falls 42× as the commit interval grows 10,000×, but
never reaches 1: even at interval 10,000 every commit rewrites the dirty
spine copy-on-write, leaving 7.2× amplification on 16-byte logical
writes. At interval 1 the superblock slots alone are 84% of all bytes
written (4,096 bytes of slot per 16 logical bytes), and the 200,000
fsyncs dominate wall time. Wall variance at interval 100 (min 2.02s, max
10.91s) is fsync scheduling noise on this machine — the I/O bytes are
identical across the three runs.

## E2 — read amplification vs cache budget

Load 1,000,000 keys, drain, commit (live = 53,035,988 bytes), reopen
cold, then 200,000 point reads. Budgets are percentages of LIVE bytes.
`read_amp` = physical read bytes / logical bytes returned (3.2 MB).

Uniform:

| budget | read_ops | read_bytes | read_amp | hit rate | evictions | p50/p95/p99 µs | wall |
|---|---|---|---|---|---|---|---|
| unbounded | 500,558 | 41,883,200 | 13.1 | 0.926 | 0 | 3/10/14 | 0.82s |
| 50% | 607,936 | 50,829,956 | 15.9 | 0.911 | 124,815 | 4/10/14 | 0.99s |
| 25% | 956,530 | 71,498,960 | 22.3 | 0.859 | 374,983 | 7/11/14 | 1.43s |
| 10% | 1,524,088 | 96,348,052 | 30.1 | 0.776 | 714,835 | 10/14/16 | 2.02s |
| 5% | 1,974,808 | 113,347,964 | 35.4 | 0.710 | 962,106 | 12/17/19 | 2.44s |
| 2% | 2,574,250 | 134,625,888 | 42.1 | 0.621 | 1,276,351 | 15/20/24 | 3.06s |

Zipfian (θ=0.99):

| budget | read_ops | read_bytes | read_amp | hit rate | evictions | p50/p95/p99 µs | wall |
|---|---|---|---|---|---|---|---|
| unbounded | 330,222 | 23,834,120 | 7.4 | 0.951 | 0 | 0/9/14 | 0.53s |
| 50% | 330,222 | 23,834,120 | 7.4 | 0.951 | 0 | 0/10/14 | 0.56s |
| 25% | 398,454 | 28,178,744 | 8.8 | 0.941 | 96,042 | 0/10/14 | 0.65s |
| 10% | 687,574 | 43,125,096 | 13.5 | 0.899 | 296,768 | 1/13/15 | 0.98s |
| 5% | 1,000,494 | 57,335,896 | 17.9 | 0.853 | 475,076 | 2/15/17 | 1.29s |
| 2% | 1,502,904 | 78,636,580 | 24.6 | 0.779 | 740,673 | 10/18/20 | 1.72s |

Two readings. First, even "unbounded" shows 13.1× (uniform) read
amplification: every rep reopens cold, so the number includes
first-touch loading of whole node records (~330 bytes average) to serve
16-byte answers — this is a cold-sweep metric, by construction. Second,
skew buys a lot: the Zipfian working set at a 50% budget evicts nothing
(identical to unbounded), and at a 2% budget Zipfian amplification
(24.6×) is below uniform's at 10% (30.1×). Overcommit events are zero
throughout — point-read pinning never exceeds these budgets.

## E3 — parameter grid (F × B, L = B)

200,000-key load, then 100,000 ycsb-a ops (uniform), cache 10% of live,
commit every 1,000 ops. `eps_eff` = ln F / ln mean_node_bytes is a
derived annotation only (ADR-0016).

| F | B | height | live nodes | mean node B | eps_eff | write_amp | read_amp | ops/s | p99 µs |
|---|---|---|---|---|---|---|---|---|---|
| 4 | 16 | 13 | 26,452 | 346 | 0.237 | 13.8 | 80.9 | 103,986 | 32 |
| 4 | 64 | 12 | 6,657 | 1,246 | 0.195 | 12.7 | 267.2 | 58,872 | 63 |
| 4 | 256 | 10 | 1,644 | 4,910 | 0.163 | 13.1 | 1,008.4 | 19,692 | 212 |
| 8 | 16 | 8 | 18,625 | 474 | 0.338 | 17.6 | 69.3 | 135,102 | 28 |
| 8 | 64 | 7 | 4,727 | 1,736 | 0.279 | 16.0 | 227.6 | 70,420 | 49 |
| 8 | 256 | 6 | 1,133 | 7,105 | 0.235 | 16.6 | 875.7 | 22,600 | 175 |
| 16 | 16 | 6 | 16,721 | 522 | 0.443 | 25.7 | 70.8 | 129,766 | 33 |
| 16 | 64 | 5 | 4,090 | 2,000 | 0.365 | 25.3 | 220.7 | 73,016 | 47 |
| 16 | 256 | 4 | 987 | 8,149 | 0.308 | 29.5 | 828.7 | 23,120 | 163 |
| 32 | 16 | 5 | 15,671 | 554 | 0.549 | 38.3 | 82.6 | 107,132 | 56 |
| 32 | 64 | 4 | 3,823 | 2,137 | 0.452 | 38.0 | 221.2 | 68,202 | 51 |
| 32 | 256 | 4 | 934 | 8,609 | 0.383 | 36.8 | 786.8 | 24,567 | 129 |

The textbook trade-off is visible in the columns: write amplification
tracks F (13–14× at F=4 vs 37–38× at F=32 — wider internal nodes are
rewritten per spine commit) and barely moves with B; read amplification
tracks node size almost linearly (B=256 nodes of ~5–8.6 KB cost ~800–
1,000× on 16-byte reads) and is U-shaped in F at B=16 (80.9 → 69.3 →
70.8 → 82.6): height savings beat node growth from F=4 to 8, then node
growth wins. Throughput peaks at F=8/B=16 (135k ops/s, p99 28 µs) —
small nodes, height 8. The F=4/B=16 height of 13 over 200k keys is a
reminder of how deep tiny-fanout trees get.

## E4 — mix suite

Default params (F=4, B=8, L=8), 200,000-key space, 500,000 ops, cache
10% of live, commit every 1,000 ops on write-bearing mixes.

| mix | ops/s | p50/p95/p99 µs | read_ops | read_bytes | write_ops | write_bytes | hit rate |
|---|---|---|---|---|---|---|---|
| load | 149,440 | 0/25/47 | 441,988 | 67,655,828 | 1,003 | 145,032,880 | 0.818 |
| point-read | 116,004 | 8/13/15 | 3,749,016 | 238,735,100 | 0 | 0 | 0.750 |
| ycsb-a | 88,843 | 4/21/37 | 2,359,534 | 288,255,476 | 1,001 | 72,000,588 | 0.715 |
| ycsb-b | 76,626 | 10/17/21 | 3,961,250 | 296,745,084 | 1,001 | 9,670,872 | 0.719 |
| ycsb-c | 112,501 | 9/13/16 | 3,749,016 | 238,735,100 | 0 | 0 | 0.750 |
| upsert-heavy | 130,631 | 0/29/47 | 1,032,200 | 125,652,532 | 1,001 | 113,735,656 | 0.731 |
| scan-mix | 85,810 | 9/18/81 | 4,679,590 | 311,291,176 | 0 | 0 | 0.703 |

The standout: ycsb-b (5% writes) is SLOWER than ycsb-a (50% writes) —
76.6k vs 88.8k ops/s — because under a 10% cache the expensive operation
is the read, not the buffered write: ycsb-b issues 3.96M record reads to
ycsb-a's 2.36M. That is the Bε-tree asymmetry stated as a measurement.
Write-dominant mixes (load 149k, upsert-heavy 131k, both p50 = 0 µs —
sub-microsecond buffered writes) lead the table. Scan-mix's p99 of 81 µs
is the 5% of ops that scan 10–100 keys.

## E5 — space debt (the ADR-0008 curve, measured)

1,000,000 uniform updates over 100,000 keys, commit every 1,000,
sampled every 50,000 ops.

| ops | live bytes | file size | file/live |
|---|---|---|---|
| 50,000 | 7,198,116 | 15,363,240 | 2.13 |
| 250,000 | 8,310,128 | 64,019,872 | 7.70 |
| 500,000 | 8,357,472 | 125,418,524 | 15.01 |
| 750,000 | 8,388,800 | 186,814,984 | 22.27 |
| 1,000,000 | 8,372,960 | 248,322,832 | 29.66 |

(Full 20-sample curve in `results/e5.csv`.) Live size stabilizes at
~8.37 MB after the keyspace saturates; the file grows linearly at ~245
bytes per update (the copy-on-write spine of each commit), reaching
29.7× the live data after a million updates. The slope is the price of
no-WAL-no-GC (ADR-0007/0008): until a reclamation layer exists, file
size is a linear function of write count, not of data size.

# ceph-memtop

Where is this Ceph node's memory? A terminal tool that accounts for a host's memory from the
node total downward, one Ceph daemon at a time, with every level adding up. Rust, one static
binary, no runtime dependencies.

```
cargo build --release --target x86_64-unknown-linux-musl
target/x86_64-unknown-linux-musl/release/ceph-memtop            # the terminal UI
ceph-memtop --once                                              # one text snapshot
ceph-memtop --once --json | jq .                                # one structured snapshot
ceph-memtop --record fixtures/myhost --interval 1               # record samples to replay later
ceph-memtop --replay fixtures/myhost --json                     # replay without a cluster, one JSON line per sample
```

Discovery needs no configuration on a host with daemons under `/var/run/ceph` (plain or per
cluster fsid) or on a `vstart.sh` tree (`$VSTART_DEST/asok`, `./build/asok`, `./build/out`,
and every bound `*.asok` the kernel lists). `--asok` and `--asok-dir` add paths. Daemons whose
socket cannot be read are listed with the reason, never omitted.

## Design: one snapshot, then everything else

```
collector (own thread, once per interval)        pure, no reads                    output
/proc, cgroups ──┐                                ┌─ node_account ──┐
admin sockets ───┼─► RawSample (one timestamp) ──►│  daemon_account ├─► tracks ─► View ─► table / TUI / JSON
(one thread each)┘        │                       └─ verify ────────┘    (peaks, rates)      verdict
                          └─► --record                                   ◄─ --replay
```

`Collector::sample()` is the only place that reads the host: it takes /proc and the cgroups, then
asks every socket on its own thread within a budget, and returns one `RawSample` under one
monotonic timestamp (a late daemon keeps its last values, marked stale). Everything after is a
function of that sample: the accounting, the checks, the peak and rate tracks, the tables, the
JSON. A recorded sample replays through the same path, which is what the tests rely on. The only
other reads are on request: the per-PG query (`g`) runs `ceph pg dump` on its own thread, and
`--enable-kv-stats` talks to the daemons at start and exit.

## The two identities

**Node**: rows sum to MemTotal. Each Ceph daemon is charged its anonymous resident memory
(`RssAnon`); its file-backed pages belong to the page cache row, where the kernel keeps them.

```
MemTotal = free + page cache (Cached + Buffers - Shmem) + shmem + slab
         + kernel (KernelStack + PageTables + Percpu) + huge pages
         + anon: each ceph daemon + other processes + anon in no process
         + other (MemTotal - everything named: vmalloc, drivers, DMA)
```

Anonymous pages shared by several processes (fork COW) count once in `AnonPages` but in each
process's `RssAnon`; that difference is shown as a signed correction row, not hidden. Socket
buffers and vmalloc are shown as reference rows: real, but inside slab and "other".

**Daemon**: rows sum to the daemon's RSS.

| row | source |
|---|---|
| cache: data | mempool `bluestore_cache_data` |
| cache: meta | the eight mempools BlueStore's meta cache counts (`BlueStore.h` `MetaCache::_get_used_bytes`) |
| cache: kv, kv_onode | `rocksdb-cache-default.usage`, `rocksdb-cache-O.usage` (perf dump) |
| budget per cache | `*-pricache:<cache>.committed_bytes` |
| RocksDB memtables, default column family only | `dump_objectstore_kv_stats` `rocksdb_memtable_usage`: needs `rocksdb_perf` and `rocksdb_collect_memory_stats` (`--enable-kv-stats`); see the limits below |
| RocksDB SST index/filter blocks, default column family only | the same dump, `rocksdb_index_filter_blocks_usage` |
| PG log, OSD maps, allocator, in-flight writes, BlueFS, buffers, other | mempools by group (`--raw` shows the keys) |
| heap, not in any mempool | tcmalloc "in use by application" minus the rows above |
| allocator: free lists and metadata | tcmalloc "actual" minus "in use" |
| anon outside the heap | `RssAnon` minus the resident heap |
| file-backed, shared memory | `RssFile`, `RssShmem` |

The daemon summary has two sections that must agree. **kernel**: RSS split into anon, file and
shmem, VmHWM, threads, and the cgroup's current, max and peak. **ceph**: the configured target
(and the tuner's, when it differs), and

```
mempool_total      = dump_mempools .mempool.total.bytes   (caches + un-cached)
heap_in_use        = heap stats "Bytes in use by application"
heap_actual        = heap stats "Actual memory used"       (the tuner's mapped/unmapped view beside it)
untracked          = heap_in_use - mempool_total
allocator_overhead = heap_actual - heap_in_use
non_heap           = rss - heap_actual
```

The **match** line checks the two sides against each other: rss = mempool_total + untracked +
allocator_overhead + non_heap, and the mempool rows equal the dump's total (hard: a failure is a
problem and fails the sample's checks); the tuner's mapped bytes equal tcmalloc's actual within 10 % (the
tuner samples on its own timer), the configured target equals the tuner's, and a dedicated cgroup
charges at least the anon memory (soft: a difference is explained). A derived figure that comes out negative is `n/a` and the match says
which input is off, e.g. heap pages held but not resident.

Rows marked `?` are residuals, `ref` rows are not summed. Without tcmalloc the heap rows merge
into "heap, not in any mempool, plus allocator free lists" from the priority cache's
`mapped_bytes`; without that, into "anon, unattributed". A derived value that comes out negative
is shown as `n/a` with the suspect source named; the level then does not reconcile and the checks
say so.

Every sample is checked: node rows = MemTotal, each readable daemon's rows = its RSS, the hard
cross-checks, and the residuals within `--tolerance` (default 60 %). The node summary ends with
the verdict, the terminal UI flags a failure in its title, JSON carries `checks` with the reasons,
and in `--once`, `--plain`, `--json` and replay the exit status is 1 when any sample failed.

The mempool undercounts the pg log (about 785 bytes per entry and 178 per dup are real against
552 and 104 counted), so an OSD under write load keeps a sizeable "heap, not in any mempool"
row. That row is the point of the tool, not a bug in it.

## Peaks

Every row carries a peak. Two kinds, labelled apart:

* `peak:kernel`: `VmHWM` and the cgroup's `memory.peak`, recorded by the kernel, correct whenever
  the tool started. Their time is when the tool first saw the value (`<start` if it was already
  there).
* `peak:seen`: the largest value this tool sampled since start, reset or restart. A spike between
  two samples is invisible; the sampling period is shown next to the columns.

Peaks never add up: parts peak at different moments. Only current values reconcile. `r` resets
the selected row's seen peak, `R` all of them; a daemon restart (new pid or start time) resets
its peaks and rates and is announced in its state column for ten samples.

## Samples

All sources of one tick are read under one monotonic timestamp. Socket reads run on their own
threads with a budget; a daemon that does not answer in time keeps its last values and is marked
`stale`, and a sample whose reads spread wider than `--max-spread` is marked degraded and used
for neither rates nor peaks. Rates divide by the measured interval.

## Writes

Read-only, except `--enable-kv-stats`, which sets `rocksdb_perf` and
`rocksdb_collect_memory_stats` on OSDs that lack them after asking (`--yes` skips the question),
and restores them on exit, on SIGINT/SIGTERM/SIGHUP, and on a panic. Switches already on are left
alone. The per-PG pglog view (`g` on an OSD) shells out to `ceph pg dump pgs` with the daemon's
own conf (`-c`), `--conf`, or `$CEPH_CONF`, and reads "unavailable" when that fails.

## Tests

`cargo test` runs the unit tests and the replay tests on `fixtures/`: `vstart-idle` (4 samples,
mon/mgr/4 OSDs, main 21.3.0 dev build with tcmalloc, on c28) and `vstart-write` (23 samples
under 4K random writes, RocksDB statistics on). The replay tests check that both identities hold
on every sample, that two replays are identical, that the reported peak is the sample maximum
with its timestamp, that a restart clears peaks and rates, that a build without heap statistics
still adds up (derived from the idle fixture by dropping the heap text), that a degraded sample is
shown but not tracked, and that unknown values stay blank.

Not yet covered by a recorded fixture: a production (cephadm) node, other Ceph releases, a build
without tcmalloc, MDS and RGW daemons. `--record` on such a host produces one.

## Verification under load

`scripts/verify-workloads.py --bin ./ceph-memtop --asok-dir <dir> --dev /dev/rbd0` runs idle, 4K
random write, 4K random read and 1M sequential read with fio while ceph-memtop records one sample
per second, reads /proc, the cgroup and the admin sockets itself twice per workload, compares those
readings with the sample memtop recorded at that second, and replays every recording through
the checks. On the c28 vstart (mon, mgr, four OSDs, main 21.3.0, 2026-10-28): 224 comparisons, the
worst difference 2.7 % (reads a second apart under write load, 0.4 % otherwise), 120 samples
checked, none failed. The pg log grew from 7 to 17 MiB per OSD under random writes, the data cache
from 0 to 290 MiB under random reads, and RSS followed, with both kinds of peak tracking it.

## Known limits

* Ceph reports RocksDB memtable and SST-reader usage for the default column family only
  (`RocksDBStore::get_statistics` calls `DB::GetProperty` without a column family handle): the
  figure stayed at 2 KiB through a 30 s write load on c28. On a sharded OSD the memtables of the
  O, L, P and M families, hundreds of MiB under write load, are therefore inside "heap, not in any
  mempool" (and in `untracked`). Reporting them needs a daemon change (`GetAggregatedIntProperty`,
  or one property per handle).
* The mon's full/inc osdmap caches report their size but their bytes live in the mempool rows, so
  they are shown as reference rows.
* A process's `RssAnon` is attributed whole; THP and page-table sharing are not corrected.

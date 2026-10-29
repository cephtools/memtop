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

## Code map

| file | what it holds |
|---|---|
| `src/main.rs` | the command line, the sample sources (live, replay), the plain/JSON loop and the TUI loop, signals and restore |
| `src/collect.rs` | the only reader of the host: discovery, then one `RawSample` per tick, with the socket reads on threads under a budget (`read_daemon` is what one thread does) |
| `src/proc.rs`, `src/asok.rs` | /proc and cgroup v2 parsers; the admin-socket wire protocol |
| `src/raw.rs`, `src/fixture.rs` | `RawSample`, what one tick read, and its recording to and loading from disk |
| `src/model.rs` | the accounting: `node_account`, `daemon_account` (the rows, the kernel and Ceph sides, their matches) |
| `src/state.rs`, `src/peaks.rs` | tracks across samples (peaks, rates, restarts), the `View` and its verdict; one tracked quantity, and `KernelPeak` for VmHWM and memory.peak |
| `src/render.rs` | summaries and tables as strings, flat and as a tree, shared by the TUI and `--plain` |
| `src/ui.rs` | the terminal screens, and what the keys and the mouse do on them |
| `src/kvstats.rs`, `src/pglog.rs` | the one write (the RocksDB switches, with consent and restore); the on-request per-PG query |
| `src/units.rs` | sizes, rates and the clock, at fixed widths |

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
| cache: data | mempool `bluestore_cache_data`; before commit 96bb533039b (2026-08-26; not in v21.3.0) it also holds the data of writes still in flight, so under write load it runs above the tuner's usage and "in-flight writes" holds only list nodes |
| cache: meta | the eight mempools BlueStore's meta cache counts (`BlueStore.h` `MetaCache::_get_used_bytes`) |
| cache: kv, kv_onode | `rocksdb-cache-default.usage`, `rocksdb-cache-O.usage` (perf dump); a column family given its own cache in `bluestore_rocksdb_cfs` gets a `cache: kv (<cf> block cache)` row from its `rocksdb-cache-<cf>` section |
| cache: mds | mempool `mds_co` (inodes, dentries, dirs): what `MDCache::cache_size()` holds against `mds_cache_memory_limit` |
| cache: EC extents | mempool `ec_extent_cache`, bounded per op shard by `ec_extent_cache_size` |
| budget per cache | `*-pricache:<cache>.committed_bytes` |
| RocksDB memtables | `dump_objectstore_kv_stats` `rocksdb_memtable_usage`: needs `rocksdb_perf` and `rocksdb_collect_memory_stats` (`--enable-kv-stats`); see the limits below |
| RocksDB table readers outside the block cache | the same dump, `rocksdb_index_filter_blocks_usage` (`rocksdb.estimate-table-readers-mem`): index and filter blocks count here only when they are not in the block cache |
| RocksDB WAL alive (reference) | `bluefs files list`, the sizes under `db.wal/`: every write still in a memtable is in a live WAL, and the memtables run about 10 % above it; a WAL file lives until every column family flushed it |
| PG log beyond the mempool count (reference, estimate) | `osd_pglog` items and bytes solved into entries and dups; 233 B per entry and 74 B per dup over the counted 552 and 104, measured with a heap profile: index maps, list nodes, size-class rounding |
| PG log, OSD and PG maps, allocator, in-flight writes, BlueFS, buffers, other | mempools by group (`--raw` shows the keys); `pgmap` is the mon's and mgr's PG map, `other` holds `osd` (PG objects, peering events), `bloom_filter` and fsck scratch |
| heap, not in any mempool | tcmalloc "in use by application" minus the rows above |
| allocator: free lists and metadata | tcmalloc "actual" minus "in use", less the pages held but not resident |
| anon outside heap (stacks, other allocators) | `RssAnon` minus the resident heap |
| file-backed, shared memory | `RssFile`, `RssShmem` |

The daemon summary has two sections that must agree. **kernel**: RSS split into anon, file and
shmem, VmHWM, threads, and the cgroup's current, max and peak. **ceph**: the configured target
(and the tuner's, when it differs: `config get` already includes the cgroup ratio and cephadm's
per-host value, so they differ only until the tuner's next pass after a runtime change), and

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
problem and fails the sample's checks); the tuner's mapped bytes equal tcmalloc's actual less its
metadata within 10 % (`mapped_bytes` is `generic.heap_size - tcmalloc.pageheap_unmapped_bytes`, and
the tuner samples on its own timer), the configured target equals the tuner's, and a dedicated cgroup
charges at least the anon memory (soft: a difference is explained). A derived figure that comes out negative is `n/a` and the match says
which input is off, e.g. heap pages held but not resident.

The border between a daemon's summary and its rows can be dragged with the mouse (`+`/`-`
do the same from the keyboard); a click selects a row and the wheel scrolls.

The rows are shown as a tree by default (`t` toggles the flat list, `--flat` starts with it), so
each figure sits under what it belongs to:

```
rss
├─ anon (RssAnon)
│  ├─ heap (tcmalloc actual, resident part)
│  │  ├─ in use by the application (heap_in_use)
│  │  │  ├─ mempools (dump_mempools total)
│  │  │  │  ├─ caches: data cache, meta cache
│  │  │  │  └─ un-cached: PG log, OSD maps, allocator, in-flight writes, BlueFS, buffers, other mempools
│  │  │  └─ untracked (heap_in_use - mempool_total)
│  │  │     ├─ RocksDB block caches (kv, kv_onode)
│  │  │     ├─ RocksDB memtables
│  │  │     │  └─ RocksDB WAL alive (reference: what the memtables hold)
│  │  │     ├─ RocksDB table readers outside the block cache
│  │  │     └─ not in any mempool (residual)
│  │  │        └─ PG log beyond the mempool count (reference, estimate)
│  │  ├─ allocator: free lists and metadata (allocator_overhead)
│  │  └─ held but not resident (reference)
│  └─ anon outside heap (stacks, other allocators)
├─ file-backed (binary, libraries, mapped files)
└─ shared memory
```

Aggregates show the current value only, since peaks do not add up; leaves carry the rate, the
growth streak and the peaks. Rows marked `?` are residuals, `ref` rows are not summed. The four
caches the tuner sizes (data, meta, kv, kv_onode), and the tree rows made only of them, are drawn
in cyan in the terminal UI. Without heap
stats (`--no-heap`, or a socket too slow for them) on a tcmalloc build the heap rows merge into
"heap not in any mempool + allocator free lists" from the priority cache's `mapped_bytes`; a build
without tcmalloc reports `mapped_bytes` 0 and collapses, like a daemon without the tuner, into
"anon, unattributed". A derived value that comes out negative
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
* `memory.peak` belongs to the cgroup, not the process: a daemon restarted into the same cgroup
  inherits the old peak, and since Linux 6.12 a write to the file resets it only for the writing
  file descriptor, so nothing outside the reader can reset it. The summary flags a cgroup peak
  above the process's own VmHWM.

Peaks never add up: parts peak at different moments. Only current values reconcile. `r` resets
the selected row's seen peak (on an aggregate of the tree, every leaf beneath it), `R` all of them; a daemon restart (new pid or start time) resets
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
alone. Both take effect at once (`RocksDBStore` reads them on every call, no restart). `rocksdb_perf`
times every write batch, which Ceph documents at 5-10 % of write performance; restoring it stops
Ceph's timers, but RocksDB's thread-local perf level stays raised on the kv-sync thread until the
OSD restarts. The consent covers the run: an OSD that restarts or becomes reachable later gets the switches
too. An n/a memtable row says why it is unknown: the switches are off on that OSD, the read failed,
or it was not requested. The per-PG pglog view (`g` on an OSD) shells out to `ceph pg dump pgs` with the daemon's
own conf (`-c`), `--conf`, or `$CEPH_CONF`, and reads "unavailable" when that fails.

## Tests

`cargo test` runs the unit tests and the replay tests on `fixtures/`: `vstart-idle` (4 samples,
mon/mgr/4 OSDs, main 21.3.0 dev build with tcmalloc, on c28) and `vstart-write` (23 samples
under 4K random writes, RocksDB statistics on). The replay tests check that both identities hold
on every sample, that two replays are identical, that the reported peak is the sample maximum
with its timestamp, that a restart clears peaks and rates, that a tcmalloc build whose heap stats
were not read still adds up (derived from the idle fixture by dropping the heap text), that a degraded sample is
shown but not tracked, and that unknown values stay blank.

`scripts/tui-test.py <binary> <args>` drives the terminal UI in a pty through a small screen model:
it opens a daemon, drags the border between the summary and the rows, presses `+`/`-`, clicks a
row, scrolls, and checks where the boxes and the cursor end up, e.g.
`scripts/tui-test.py target/debug/ceph-memtop --replay fixtures/vstart-write --interval 0.3`.

`scripts/replay-diff.sh <old-binary> <new-binary> <recording>...` replays recordings through two
binaries in every output form and compares them: a refactor must leave the JSON identical, and
the text forms show where a wording changed. The recordings used for that on c28 (an object-write
run, a recovery, a random write; 75 to 320 samples at 1 s) are too large for the repository;
`--record` makes new ones.

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

* Every Ceph release to date (v21.3.0 and main as of October 2026) reports RocksDB memtable and
  SST-reader usage for the default column family only (`RocksDBStore::get_statistics` calls
  `DB::GetProperty` without a column family handle): the figure stayed at 2 KiB through a 10 min
  write load on c28 while the live WAL held 954 MiB. The memtables of the O, L, P and M families,
  hundreds of MiB under write load, are inside "heap, not in any mempool" (and in `untracked`);
  the memtables row says so. A fix (`GetAggregatedIntProperty`) is a four-line change to
  `RocksDBStore::get_statistics`.
* The mon's full/inc osdmap caches have no usage counter: their rows show what the tuner assigned
  them (`pri1_bytes`, capped by the budget), and their bytes, encoded maps read from the store,
  sit in the buffers row (`buffer_anon`), so they are shown as reference rows.
* A daemon that refuses `heap stats` (a mgr or rgw of v21.3.0, or an OSD still starting) is asked
  again after 10 s, then at doubling intervals up to 10 min: each refused request is a line in its
  log.
* A process's `RssAnon` is attributed whole; THP and page-table sharing are not corrected.
* The rows of one sample come from reads up to `--max-spread` apart (/proc, then the sockets), so a
  transient that lasts one sample can land in the wrong row: an OSD's RSS spike of 130 MiB at the
  start of an object-write burst on c28 was real (VmHWM agreed) but showed as "anon outside heap",
  because the heap stats were read 247 ms before the RSS. The peak value and time are right; the
  split of such a sample is not.

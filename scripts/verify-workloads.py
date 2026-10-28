#!/usr/bin/env python3
"""Verify ceph-memtop against independent readings under typical workloads.

    verify-workloads.py --bin ./ceph-memtop --asok-dir vs/asok --dev /dev/rbd0 --out verify

For idle, 4K random write, 4K random read and 1M sequential read (fio on --dev), ceph-memtop records
one sample per second; twice per workload this script reads /proc, the cgroup and the admin
sockets itself and compares them with the sample memtop recorded at that second. Every recording
is then replayed through --check. Run as a user that can read the sockets and /proc/PID/fd.
"""
import argparse, glob, json, os, signal, socket, struct, subprocess, sys, time

def ask(path, cmd):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(5); s.connect(path); s.sendall(cmd.encode() + b"\0")
    hdr = b""
    while len(hdr) < 4: hdr += s.recv(4 - len(hdr))
    n = struct.unpack(">I", hdr)[0]; out = b""
    while len(out) < n: out += s.recv(min(65536, n - len(out)))
    s.close(); return out.decode(errors="replace")

def heap_field(text, what):
    for l in text.splitlines():
        if what in l:
            return int(l.replace("MALLOC:", "").lstrip("+= ").split()[0])
    return None

def probe(asok_dir):
    """Independent readings for every OSD socket in the directory."""
    out = {"wall_ms": int(time.time() * 1000), "osds": {}}
    for p in sorted(glob.glob(os.path.join(asok_dir, "*osd.*.asok"))):
        name = os.path.basename(p).replace(".asok", "").split("-")[-1]
        try:
            pid = int(subprocess.check_output(["pgrep", "-f", f"ceph-osd -i {name.split('.')[1]} "]).split()[0])
        except Exception:
            pid = None
        r = {"pid": pid}
        if pid:
            st = open(f"/proc/{pid}/status").read()
            for k in ("VmRSS", "RssAnon", "VmHWM"):
                r[k] = next(int(l.split()[1]) * 1024 for l in st.splitlines() if l.startswith(k + ":"))
            cg = open(f"/proc/{pid}/cgroup").read().strip().split("::")[1]
            try: r["cgroup_current"] = int(open(f"/sys/fs/cgroup{cg}/memory.current").read())
            except Exception: r["cgroup_current"] = None
        r["mempool_total"] = json.loads(ask(p, '{"prefix":"dump_mempools"}'))["mempool"]["total"]["bytes"]
        h = ask(p, '{"prefix":"heap","heapcmd":"stats"}')
        r["heap_in_use"] = heap_field(h, "Bytes in use by application"); r["heap_actual"] = heap_field(h, "Actual memory used")
        pd = json.loads(ask(p, '{"prefix":"perf dump"}'))
        r["tuner_mapped"] = pd.get("bluestore-pricache", {}).get("mapped_bytes")
        out["osds"][name] = r
    return out

def run_workload(args, name, fio_args, secs):
    out = os.path.join(args.out, name)
    subprocess.run(["rm", "-rf", out])
    mt = subprocess.Popen([args.bin, "--plain", "--interval", "1", "--enable-kv-stats", "--yes", "--record", out, "--asok-dir", args.asok_dir], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(4)
    fio = None
    if fio_args:
        fio = subprocess.Popen(["fio", "--name=" + name, "--filename=" + args.dev, "--direct=1", "--ioengine=libaio", "--group_reporting", "--time_based", f"--runtime={secs}", "--size=3G", "--output-format=terse"] + fio_args, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    probes = []
    t0 = time.time()
    for frac in (0.4, 0.8):
        time.sleep(max(0.5, t0 + secs * frac - time.time()))
        probes.append(probe(args.asok_dir))
    iops = ""
    if fio:
        terse = fio.communicate()[0].decode()
        f = terse.strip().split(";")
        try: iops = f"read {f[7]} IOPS, write {f[48]} IOPS"
        except Exception: iops = "fio: " + terse[:60]
    time.sleep(3)
    mt.send_signal(signal.SIGINT); mt.wait()
    return out, probes, iops


def nearest(samples, wall_ms):
    return min(samples, key=lambda s: abs(s["wall_ms"] - wall_ms))

def compare(args, name, out, probes):
    """memtop's output (replayed from its recording) against the independent readings."""
    lines = subprocess.check_output([args.bin, "--replay", out, "--json"]).decode().splitlines()
    views = [json.loads(l) for l in lines if l.strip()]
    rows = []
    worst = 0.0
    for pr in probes:
        v = nearest(views, pr["wall_ms"]); dt = (v["wall_ms"] - pr["wall_ms"]) / 1000.0
        for osd, r in pr["osds"].items():
            d = next(x for x in v["daemons"] if x["account"]["name"] == osd)
            pairs = [("rss", r.get("VmRSS"), d["account"]["kernel"]["rss"]), ("anon", r.get("RssAnon"), d["account"]["kernel"]["anon"]),
                     ("cgroup", r.get("cgroup_current"), d["account"]["kernel"]["cgroup_current"]), ("mempool_total", r["mempool_total"], d["account"]["ceph"]["mempool_total"]),
                     ("heap_in_use", r["heap_in_use"], d["account"]["ceph"]["heap_in_use"]), ("heap_actual", r["heap_actual"], d["account"]["ceph"]["heap_actual"]),
                     ("tuner_mapped", r["tuner_mapped"], d["account"]["ceph"]["tuner_mapped"])]
            for what, mine, theirs in pairs:
                if mine is None or theirs is None:
                    rows.append((osd, what, "n/a", "")); continue
                diff = abs(mine - theirs) / max(mine, theirs, 1) * 100
                worst = max(worst, diff)
                rows.append((osd, what, f"{diff:4.1f}%", f"{mine/2**20:8.1f}M vs {theirs/2**20:8.1f}M" + (f"  ({dt:+.1f} s apart)" if diff > 2 else "")))
    return rows, worst, views

def behaviour(views, osd):
    """What changed over the workload for one OSD, from memtop's own output."""
    first, last = views[0], views[-1]
    def row(v, rid):
        d = next(x for x in v["daemons"] if x["account"]["name"] == osd)
        r = next((x for x in d["rows"] if x["id"] == rid), None)
        return (r or {}).get("value")
    def peak(v, rid):
        d = next(x for x in v["daemons"] if x["account"]["name"] == osd)
        r = next((x for x in d["rows"] if x["id"] == rid), None)
        return ((r or {}).get("track") or {}).get("peak", {}) or {}
    d0 = next(x for x in first["daemons"] if x["account"]["name"] == osd); d1 = next(x for x in last["daemons"] if x["account"]["name"] == osd)
    M = lambda v: "n/a" if v is None else f"{v/2**20:.1f}M"
    return (f"{osd}: rss {M(d0['rss']['cur'])} -> {M(d1['rss']['cur'])} (peak seen {M(d1['rss']['peak']['value'] if d1['rss']['peak'] else None)}, kernel {M(d1['rss_kernel_peak']['value'])})"
            f" | data cache {M(row(first,'cache:data'))} -> {M(row(last,'cache:data'))} | kv cache {M(row(first,'cache:kv'))} -> {M(row(last,'cache:kv'))}"
            f" | pglog {M(row(first,'nocache:pglog'))} -> {M(row(last,'nocache:pglog'))}"
            f" | untracked {M(d1['account']['ceph']['untracked'])} | residual {d1['account']['residual_pct']}%")

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", default="./ceph-memtop"); ap.add_argument("--asok-dir", required=True); ap.add_argument("--dev", required=True); ap.add_argument("--out", default="verify")
    ap.add_argument("--secs", type=int, default=30)
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)
    workloads = [("idle", None, 10), ("randwrite-4k", ["--rw=randwrite", "--bs=4k", "--iodepth=32", "--numjobs=2"], args.secs),
                 ("randread-4k", ["--rw=randread", "--bs=4k", "--iodepth=32", "--numjobs=2"], args.secs), ("seqread-1m", ["--rw=read", "--bs=1M", "--iodepth=16"], args.secs // 2 + 5)]
    failed = 0
    for name, fio_args, secs in workloads:
        print(f"=== {name} ({secs} s)")
        out, probes, iops = run_workload(args, name, fio_args, secs)
        if iops: print(f"fio: {iops}")
        rows, worst, views = compare(args, name, out, probes)
        bad = [r for r in rows if r[2] != "n/a" and float(r[2].rstrip("%")) > 5]
        print(f"independent readings vs memtop output: {len(rows)} comparisons, worst difference {worst:.1f}%" + ("" if not bad else f", {len(bad)} above 5%"))
        for r in (bad if bad else rows[:7]):
            print(f"  {r[0]:6} {r[1]:14} {r[2]:>6}  {r[3]}")
        if len(rows) > 7 and not bad: print(f"  ... ({len(rows) - 7} more, all within 5%)")
        chk = subprocess.run([args.bin, "--replay", out, "--check"], capture_output=True, text=True)
        tail = chk.stdout.strip().splitlines()[-1] if chk.stdout.strip() else chk.stderr.strip()[-200:]
        print(f"replay --check: {tail}" + ("" if chk.returncode == 0 else " (FAILED)"))
        if chk.returncode != 0 or bad: failed += 1
        for osd in sorted(views[-1]["daemons"][i]["account"]["name"] for i in range(len(views[-1]["daemons"])) if views[-1]["daemons"][i]["account"]["kind"] == "osd"):
            print("  " + behaviour(views, osd))
        print()
    print("RESULT:", "all workloads verified" if failed == 0 else f"{failed} workload(s) with differences")
    sys.exit(1 if failed else 0)

if __name__ == "__main__":
    main()

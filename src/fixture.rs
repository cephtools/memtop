//! Recorded samples: one JSON file per tick, replayable without a cluster.

use crate::raw::RawSample;
use std::path::Path;

pub fn record(dir: &Path, s: &RawSample) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("sample-{:06}.json", s.seq));
    let tmp = dir.join(format!(".sample-{:06}.json.tmp", s.seq));
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    std::fs::rename(tmp, path)
}

pub fn load(dir: &Path) -> std::io::Result<Vec<RawSample>> {
    let mut names: Vec<_> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().map_or(false, |n| {
                n.to_string_lossy().starts_with("sample-") && n.to_string_lossy().ends_with(".json")
            })
        })
        .collect();
    names.sort();
    let mut out = Vec::new();
    for p in names {
        let s: RawSample = serde_json::from_slice(&std::fs::read(&p)?).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: {}", p.display(), e),
            )
        })?;
        out.push(s);
    }
    Ok(out)
}

/// The sample with only the perf-dump sections the accounting reads, so fixtures stay small.
pub fn lean(s: &RawSample) -> RawSample {
    let mut out = s.clone();
    for d in &mut out.daemons {
        if let Some(serde_json::Value::Object(m)) = &mut d.perf {
            m.retain(|k, _| {
                k.contains("pricache")
                    || k.starts_with("prioritycache")
                    || k.starts_with("rocksdb-cache")
                    || k == "mempool"
                    || k == "rocksdb"
            });
        }
        // of the BlueFS file list only the WAL files matter
        if let Some(serde_json::Value::Array(files)) = &mut d.bluefs_files {
            files.retain(|f| {
                f.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n.starts_with("db.wal/"))
                    .unwrap_or(false)
            });
        }
    }
    out
}

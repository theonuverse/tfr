use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::Client;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MIRROR_DIR: &str = "/data/data/com.termux/files/usr/etc/termux/mirrors";
const LINK_PATH: &str = "/data/data/com.termux/files/usr/etc/termux/chosen_mirrors";
const SOURCES_LIST: &str = "/data/data/com.termux/files/usr/etc/apt/sources.list";
const SOURCES_BACKUP: &str = "/data/data/com.termux/files/usr/etc/apt/sources.list.bak";
const PROBE_SUFFIX: &str = "dists/stable/Release";
const SAMPLES: usize = 3;

struct Mirror {
    path: PathBuf,
    name: String,
    base_url: String,  // e.g. https://mirror.sunred.org/termux/termux-main
    probe_url: String, // base_url + "/" + PROBE_SUFFIX
}

struct BenchResult {
    path: PathBuf,
    name: String,
    base_url: String,
    avg_latency: Duration,
    jitter: Duration,
}

fn collect_mirrors(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                files.extend(collect_mirrors(&path));
            } else if !path.to_str().unwrap_or("").contains(".dpkg-") {
                files.push(path);
            }
        }
    }
    files
}

async fn probe_mirror(client: &Client, probe_url: &str) -> Option<Duration> {
    let start = Instant::now();
    let resp = client.head(probe_url).send().await.ok()?;
    if resp.status().is_success() {
        Some(start.elapsed())
    } else {
        None
    }
}

fn update_sources_list(base_url: &str) -> std::io::Result<()> {
    let original = match fs::read_to_string(SOURCES_LIST) {
        Ok(s) => s,
        // sources.list missing entirely — nothing to update
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    // One-time backup: only create if no backup exists yet, preserving factory state
    if fs::symlink_metadata(SOURCES_BACKUP).is_err() {
        fs::write(SOURCES_BACKUP, &original)?;
        println!("Backup: saved original sources.list to sources.list.bak");
    }

    let new_line = format!("deb {} stable main", base_url);
    let mut replaced = false;

    let new_contents: String = original
        .lines()
        .map(|line| {
            // Match the termux main repo line regardless of which CDN or mirror it currently points to
            if line.starts_with("deb ")
                && (line.contains("termux-main")
                    || line.contains("packages-cf.termux.dev")
                    || line.contains("packages.termux.dev"))
            {
                replaced = true;
                new_line.clone()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    if !replaced {
        // No recognisable termux-main line found — do not touch the file
        println!("Note: no termux-main line found in sources.list, skipping rewrite");
        return Ok(());
    }

    // Atomic write: temp file + rename so a killed process cannot corrupt sources.list
    let tmp = format!("{}.tmp", SOURCES_LIST);
    fs::write(&tmp, format!("{}\n", new_contents))?;
    fs::rename(&tmp, SOURCES_LIST)?;

    println!("sources.list: updated to {}", base_url);
    Ok(())
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(3))
        .tcp_nodelay(true)
        .build()
        .unwrap();

    let paths = collect_mirrors(Path::new(MIRROR_DIR));

    let mirrors: Vec<Mirror> = paths
        .into_iter()
        .filter_map(|path| {
            let content = fs::read_to_string(&path).ok()?;
            let raw = content
                .lines()
                .find(|l| l.starts_with("MAIN="))?
                .split('"')
                .nth(1)?;
            // Normalise: strip trailing slash for base_url, re-add for probe_url
            let base_url = raw.trim_end_matches('/').to_string();
            let probe_url = format!("{}/{}", base_url, PROBE_SUFFIX);
            let name = path.file_name()?.to_string_lossy().into();
            Some(Mirror { path, name, base_url, probe_url })
        })
        .collect();

    println!(
        "Benchmarking {} mirrors ({} samples each)...",
        mirrors.len(),
        SAMPLES
    );

    let mut tasks: FuturesUnordered<_> = mirrors
        .into_iter()
        .map(|m| {
            let client = client.clone();
            tokio::spawn(async move {
                let mut latencies = Vec::with_capacity(SAMPLES);
                for _ in 0..SAMPLES {
                    if let Some(l) = probe_mirror(&client, &m.probe_url).await {
                        latencies.push(l);
                    }
                }
                if latencies.len() == SAMPLES {
                    let sum: Duration = latencies.iter().copied().sum();
                    let avg = sum / SAMPLES as u32;
                    let jitter =
                        latencies.iter().copied().max()? - latencies.iter().copied().min()?;
                    Some(BenchResult {
                        path: m.path,
                        name: m.name,
                        base_url: m.base_url,
                        avg_latency: avg,
                        jitter,
                    })
                } else {
                    None
                }
            })
        })
        .collect();

    let mut results = Vec::new();
    while let Some(res) = tasks.next().await {
        if let Ok(Some(r)) = res {
            results.push(r);
        }
    }

    results.sort_unstable_by(|a, b| {
        a.avg_latency
            .cmp(&b.avg_latency)
            .then(a.jitter.cmp(&b.jitter))
    });

    println!("\n{:<25} | {:<12} | {:<10}", "MIRROR", "AVG LATENCY", "JITTER");
    println!("{:-<52}", "");
    for r in results.iter().take(10) {
        println!("{:<25} | {:<12?} | {:<10?}", r.name, r.avg_latency, r.jitter);
    }

    if let Some(best) = results.first() {
        // Update the symlink
        if fs::symlink_metadata(LINK_PATH).is_ok() {
            fs::remove_file(LINK_PATH)?;
        }
        std::os::unix::fs::symlink(&best.path, LINK_PATH)?;
        println!("Symlink: chosen_mirrors -> {}", best.name);

        // Surgically update sources.list to prevent pkg from bypassing the mirror
        update_sources_list(&best.base_url)?;

        println!("\nSUCCESS: {} is now the active mirror", best.name);
    }

    Ok(())
      }

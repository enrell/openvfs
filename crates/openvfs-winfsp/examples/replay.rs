//! Replays a WinFsp callback trace (`OPENVFS_WINFSP_LOG`) against the host
//! logic, with no driver, and reports how long each kind of call takes.
//!
//! Usage: replay <game-dir> <upper-dir> <layers-file> <trace> [max-lines] [entries-per-page]
//!
//! `layers-file` has one mod directory per line, highest priority first.
use std::sync::Arc;
use std::time::{Duration, Instant};

use openvfs::{FileSystem, MemoryFs, OverlayFs, PhysicalFs};
use openvfs_winfsp::MountOptions;
use openvfs_winfsp::host::Host;

fn main() {
    let mut args = std::env::args().skip(1);
    let game = args.next().expect("game dir");
    let upper = args.next().expect("upper dir");
    let layers = std::fs::read_to_string(args.next().expect("layers file")).expect("layers");
    let trace = std::fs::read_to_string(args.next().expect("trace")).expect("trace");
    let max: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(usize::MAX);
    let page: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(30);

    let mut lowers: Vec<Arc<dyn FileSystem>> = vec![Arc::new(MemoryFs::new())];
    for dir in layers.lines().filter(|l| !l.is_empty()) {
        lowers.push(Arc::new(PhysicalFs::read_only(dir).unwrap_or_else(|e| panic!("{dir}: {e}"))));
    }
    lowers.push(Arc::new(PhysicalFs::read_only(&game).expect("game")));
    let overlay = OverlayFs::with_layers(Arc::new(PhysicalFs::new(&upper).expect("upper")), lowers);
    let host = Host::new(Arc::new(overlay), &MountOptions::default()).expect("host");

    let mut totals: std::collections::BTreeMap<&str, (usize, Duration)> = std::collections::BTreeMap::new();
    let started = Instant::now();
    let mut slowest: Vec<(Duration, String)> = Vec::new();
    for line in trace.lines().take(max) {
        let mut parts = line.split_whitespace();
        let (Some(op), Some(path)) = (parts.next(), parts.next()) else { continue };
        let t = Instant::now();
        let kind = match op {
            "get_security_by_name" => {
                let _ = host.get_security_by_name(path);
                "get_security_by_name"
            }
            "open" => {
                let _ = host.open(path, 0, 0x0012_0089);
                "open"
            }
            "read_directory" => {
                list_dir(&host, &path.replace('/', "\\"), page);
                "read_directory"
            }
            _ => continue,
        };
        let took = t.elapsed();
        let e = totals.entry(kind).or_default();
        e.0 += 1;
        e.1 += took;
        if took > Duration::from_millis(50) {
            slowest.push((took, format!("{kind} {path}")));
        }
    }
    for (kind, (n, d)) in &totals {
        println!(
            "{kind:24} {n:7} calls  {:9.1} ms total  {:8.1} us/call",
            d.as_secs_f64() * 1e3,
            d.as_secs_f64() * 1e6 / *n as f64
        );
    }
    slowest.sort_by_key(|s| std::cmp::Reverse(s.0));
    for (d, what) in slowest.iter().take(12) {
        println!("  slow {:8.1} ms  {what}", d.as_secs_f64() * 1e3);
    }
    println!("wall {:.2} s", started.elapsed().as_secs_f64());
}

/// Lists a directory the way the kernel does: page after page, each resuming
/// after the last name of the previous one.
fn list_dir(host: &Host, name: &str, page: usize) {
    let Ok((handle, _)) = host.open(name, 1, 1) else { return };
    let header = std::mem::size_of::<openvfs_winfsp::abi::FspFsctlDirInfo>();
    let mut marker: Option<String> = None;
    loop {
        let mut taken = 0;
        let mut last = None;
        let done = host
            .read_directory(&handle, marker.as_deref(), &mut |buf| {
                let units = (usize::from(buf.info.size) - header) / 2;
                last = Some(String::from_utf16_lossy(&buf.name[..units]));
                taken += 1;
                taken < page
            })
            .unwrap_or(true);
        if done || taken == 0 {
            return;
        }
        marker = last;
    }
}

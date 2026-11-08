//! Probe: reproduce Loom's VFS stack and report what each path does.
//!
//! Usage: loomprobe <game-dir> <upper-dir> <mod-layer>[;<mod-layer>...]
use std::sync::Arc;

use openvfs::{FileSystem, MemoryFs, OverlayFs, PhysicalFs, VPath};

fn main() {
    let mut args = std::env::args().skip(1);
    let game = args.next().expect("game dir");
    let upper = args.next().expect("upper dir");
    let layers = args.next().unwrap_or_default();

    let runtime = Arc::new(MemoryFs::new());
    let mut lowers: Vec<Arc<dyn FileSystem>> = vec![runtime];
    for dir in layers.split(';').filter(|d| !d.is_empty()) {
        match PhysicalFs::read_only(dir) {
            Ok(fs) => lowers.push(Arc::new(fs)),
            Err(e) => println!("layer {dir}: ERR {e:?}"),
        }
    }
    match PhysicalFs::read_only(&game) {
        Ok(fs) => lowers.push(Arc::new(fs)),
        Err(e) => {
            println!("base layer: ERR {e:?}");
            return;
        }
    }
    let upper_fs = match PhysicalFs::new(&upper) {
        Ok(fs) => Arc::new(fs),
        Err(e) => {
            println!("upper: ERR {e:?}");
            return;
        }
    };
    let overlay = OverlayFs::with_layers(upper_fs, lowers);

    for p in
        ["/", "/Data", "/Data/SKSE", "/Data/SKSE/Plugins", "/Data/SKSE/Plugins/SSEDisplayTweaks.dll", "/SkyrimSE.exe"]
    {
        let vp: VPath = p.parse().expect("path");
        match overlay.metadata(&vp) {
            Ok(m) => println!("{p:48} meta  dir={}", m.is_dir()),
            Err(e) => println!("{p:48} meta  ERR {e:?}"),
        }
        match overlay.read_dir(&vp) {
            Ok(v) => println!("{p:48} dir   {} entries", v.len()),
            Err(e) => println!("{p:48} dir   ERR {e:?}"),
        }
    }
}

//! MVP tour: in-memory storage, a host directory, a mount table, `std::io` interop.
//!
//! Run with: `cargo run -p open-vfs --example basic`

use std::io::{BufRead, BufReader, Write};

use openvfs::prelude::*;

fn main() -> openvfs::Result<()> {
    // 1. A purely in-memory file system.
    let scratch = MemoryFs::new();

    // 2. This crate's own source tree, exposed read-only.
    let source = ReadOnlyFs::new(PhysicalFs::new(env!("CARGO_MANIFEST_DIR"))?);

    // 3. Compose them into one namespace.
    let mounts = MountFs::new();
    mounts.mount("/", scratch)?;
    mounts.mount("/crate", source)?;
    let vfs = Vfs::new(mounts);

    // Plain std-like calls...
    vfs.create_dir_all("/tmp/reports")?;
    vfs.write("/tmp/reports/hello.txt", "hello from the VFS!\n")?;
    println!("{}", vfs.read_to_string("/tmp/reports/hello.txt")?.trim_end());

    // ...and handles are ordinary `std::io` objects.
    let mut file = vfs.create("/tmp/reports/lines.txt")?;
    for i in 1..=3 {
        writeln!(file, "line {i}")?;
    }
    drop(file);
    let reader = BufReader::new(vfs.open_read("/tmp/reports/lines.txt")?);
    println!("lines.txt has {} lines", reader.lines().count());

    // The read-only mount refuses writes, with a precise error.
    match vfs.write("/crate/Cargo.toml", "oops") {
        Err(e) => println!("blocked as expected: {e}"),
        Ok(()) => unreachable!(),
    }

    // Walk across mounts; `..` can never climb out of the root.
    println!("\nRust sources under /crate/src:");
    for entry in vfs.walk("/crate/../../crate/src")? {
        let entry = entry?;
        if entry.path.extension() == Some("rs") {
            let len = vfs.metadata(&entry.path)?.len;
            println!("  {:<32} {len:>6} bytes", entry.path);
        }
    }

    println!("\nroot listing: {:?}", vfs.read_dir("/")?.iter().map(|e| e.name.as_str()).collect::<Vec<_>>());
    Ok(())
}

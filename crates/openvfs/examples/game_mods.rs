//! Layered game assets: base game < mods (by priority) < player saves.
//!
//! Run with: `cargo run -p open-vfs --example game_mods`

use std::sync::Arc;

use openvfs::prelude::*;

fn layer(files: &[(&str, &str)]) -> openvfs::Result<Arc<dyn FileSystem>> {
    let fs = Vfs::memory();
    for (path, contents) in files {
        if let Some(parent) = VPath::new(path)?.parent() {
            fs.create_dir_all(parent)?;
        }
        fs.write(path, contents)?;
    }
    Ok(fs.backend().clone())
}

fn main() -> openvfs::Result<()> {
    let base = layer(&[
        ("/textures/grass.png", "base grass"),
        ("/textures/stone.png", "base stone"),
        ("/config/game.toml", "difficulty = \"normal\""),
    ])?;
    let hd_textures = layer(&[("/textures/grass.png", "HD grass"), ("/textures/stone.png", "HD stone")])?;
    let grass_fix = layer(&[("/textures/grass.png", "fixed HD grass"), ("/textures/flower.png", "new flower")])?;

    // Highest priority first. The upper layer captures everything the game writes.
    let saves = Arc::new(MemoryFs::new());
    let overlay = Arc::new(OverlayFs::with_layers(saves, vec![grass_fix, hd_textures, base.clone()]));
    let game = Vfs::from(overlay.clone());

    println!("resolved assets:");
    for entry in game.walk("/textures")? {
        let entry = entry?;
        println!("  {:<24} -> {}", entry.path, game.read_to_string(&entry.path)?);
    }

    // The player changes a setting and saves a game: only the upper layer changes.
    game.write("/config/game.toml", "difficulty = \"hard\"")?;
    game.create_dir_all("/saves")?;
    game.write("/saves/slot1.json", r#"{"level": 7}"#)?;

    println!("\nwhat the player changed (relative to the installed game):");
    for change in overlay.changes()? {
        println!("  {change}");
    }
    let base = Vfs::from_arc(base);
    println!("\nbase game config is untouched: {}", base.read_to_string("/config/game.toml")?);
    Ok(())
}

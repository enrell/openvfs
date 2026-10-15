//! Thread-safety and consistency under concurrent use.
//!
//! Every test runs under a watchdog (`within`) so a deadlock fails fast
//! instead of hanging CI.

use crate::common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use common::{Rng, TempDir, Tree, assert_trees_eq, fixtures, model, populate, snapshot, within};
use openvfs::prelude::*;

const WATCHDOG: Duration = Duration::from_secs(60);

/// Delegates everything, but every directory listing takes a while.
struct SlowListings<F>(F);

impl<F: FileSystem> FileSystem for SlowListings<F> {
    fn metadata(&self, path: &VPath) -> openvfs::Result<Metadata> {
        self.0.metadata(path)
    }
    fn read_dir(&self, path: &VPath) -> openvfs::Result<Vec<DirEntry>> {
        thread::sleep(Duration::from_millis(2));
        self.0.read_dir(path)
    }
    fn create_dir(&self, path: &VPath) -> openvfs::Result<()> {
        self.0.create_dir(path)
    }
    fn remove_file(&self, path: &VPath) -> openvfs::Result<()> {
        self.0.remove_file(path)
    }
    fn remove_dir(&self, path: &VPath) -> openvfs::Result<()> {
        self.0.remove_dir(path)
    }
    fn remove_dir_all(&self, path: &VPath) -> openvfs::Result<()> {
        self.0.remove_dir_all(path)
    }
    fn rename(&self, from: &VPath, to: &VPath) -> openvfs::Result<()> {
        self.0.rename(from, to)
    }
    fn copy_file(&self, from: &VPath, to: &VPath) -> openvfs::Result<u64> {
        self.0.copy_file(from, to)
    }
    fn open(&self, path: &VPath, options: &OpenOptions) -> openvfs::Result<Box<dyn VfsFile>> {
        self.0.open(path, options)
    }
}

/// Shared-backend configurations worth hammering from many threads.
fn backends() -> Vec<(&'static str, fixtures::Fixture)> {
    let empty = Tree::new();
    vec![
        ("memory", fixtures::memory(&empty)),
        ("physical", fixtures::physical(&empty)),
        ("overlay_memory", fixtures::overlay_memory(&empty)),
        ("overlay_on_disk", fixtures::overlay_on_disk(&empty)),
        ("mounted", fixtures::mounted(&empty)),
    ]
}

#[test]
fn parallel_writers_on_distinct_files() {
    for (name, fixture) in backends() {
        let vfs = Vfs::from_arc(fixture.fs.clone());
        within(WATCHDOG, move || {
            let threads: Vec<_> = (0..8)
                .map(|t| {
                    let vfs = vfs.clone();
                    thread::spawn(move || {
                        vfs.create_dir_all(format!("/t{t}/nested")).unwrap();
                        for i in 0..40 {
                            vfs.write(format!("/t{t}/nested/f{i}"), format!("{t}:{i}")).unwrap();
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            for t in 0..8 {
                for i in 0..40 {
                    assert_eq!(vfs.read_to_string(format!("/t{t}/nested/f{i}")).unwrap(), format!("{t}:{i}"), "{name}");
                }
            }
        });
        drop(fixture);
    }
}

#[test]
fn concurrent_create_dir_all_on_the_same_path_all_succeed() {
    for (name, fixture) in backends() {
        let vfs = Vfs::from_arc(fixture.fs.clone());
        within(WATCHDOG, move || {
            let barrier = Arc::new(Barrier::new(12));
            let threads: Vec<_> = (0..12)
                .map(|_| {
                    let (vfs, barrier) = (vfs.clone(), barrier.clone());
                    thread::spawn(move || {
                        barrier.wait();
                        vfs.create_dir_all("/race/a/b/c/d")
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap().unwrap_or_else(|e| panic!("{name}: {e}"));
            }
            assert!(vfs.is_dir("/race/a/b/c/d"), "{name}");
        });
        drop(fixture);
    }
}

#[test]
fn exactly_one_create_new_wins() {
    for (name, fixture) in backends() {
        let vfs = Vfs::from_arc(fixture.fs.clone());
        within(WATCHDOG, move || {
            let barrier = Arc::new(Barrier::new(10));
            let winners = Arc::new(AtomicUsize::new(0));
            let threads: Vec<_> = (0..10)
                .map(|_| {
                    let (vfs, barrier, winners) = (vfs.clone(), barrier.clone(), winners.clone());
                    thread::spawn(move || {
                        barrier.wait();
                        match vfs.open("/lock", OpenOptions::new().write(true).create_new(true)) {
                            Ok(_) => {
                                winners.fetch_add(1, Ordering::SeqCst);
                            }
                            Err(VfsError::AlreadyExists(_)) => {}
                            Err(e) => panic!("unexpected error: {e}"),
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            assert_eq!(winners.load(Ordering::SeqCst), 1, "{name}: create_new must be exclusive");
        });
        drop(fixture);
    }
}

#[test]
fn atomic_writes_are_never_observed_torn() {
    for (name, fixture) in backends() {
        let vfs = Vfs::from_arc(fixture.fs.clone());
        within(WATCHDOG, move || {
            const LEN: usize = 64 * 1024;
            vfs.write_atomic("/state", vec![b'A'; LEN]).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let clean = Arc::new(AtomicUsize::new(0));
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    let (vfs, stop, clean) = (vfs.clone(), stop.clone(), clean.clone());
                    thread::spawn(move || {
                        while !stop.load(Ordering::Relaxed) {
                            // Some hosts (Windows) briefly refuse to open a file being replaced.
                            let Ok(bytes) = vfs.read("/state") else { continue };
                            assert_eq!(bytes.len(), LEN, "torn read: wrong length");
                            assert!(bytes.iter().all(|&b| b == bytes[0]), "torn read: mixed contents");
                            clean.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            // Keep replacing the file until the readers have genuinely raced against us: a fixed number of
            // rounds can finish before the reader threads are even scheduled, which would prove nothing.
            let mut round = 0;
            while (round < 60 || clean.load(Ordering::Relaxed) < 200) && !readers.iter().any(JoinHandle::is_finished) {
                let fill = if round % 2 == 0 { b'B' } else { b'A' };
                vfs.write_atomic("/state", vec![fill; LEN]).unwrap();
                round += 1;
            }
            stop.store(true, Ordering::Relaxed);
            for reader in readers {
                reader.join().unwrap(); // a torn read panics in the reader and surfaces here
            }
            let reads = clean.load(Ordering::Relaxed);
            assert!(reads >= 200, "{name}: only {reads} clean reads overlapped {round} writes");
        });
        drop(fixture);
    }
}

#[test]
fn concurrent_appends_are_not_lost() {
    // Appends through MemoryFs-backed handles are atomic per write call.
    let empty = Tree::new();
    for (name, fixture) in [("memory", fixtures::memory(&empty)), ("overlay_memory", fixtures::overlay_memory(&empty))]
    {
        let vfs = Vfs::from_arc(fixture.fs.clone());
        within(WATCHDOG, move || {
            vfs.write("/log", "").unwrap();
            let threads: Vec<_> = (0..8)
                .map(|t| {
                    let vfs = vfs.clone();
                    thread::spawn(move || {
                        for _ in 0..100 {
                            vfs.append("/log", format!("[{t}------]")).unwrap();
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            let log = vfs.read_to_string("/log").unwrap();
            assert_eq!(log.len(), 8 * 100 * "[0------]".len(), "{name}");
            for t in 0..8 {
                assert_eq!(log.matches(&format!("[{t}------]")).count(), 100, "{name}: record {t} lost or interleaved");
            }
        });
    }
}

/// Renames must be atomic to observers: a listing sees the directory under
/// exactly one of its two names — never both, never neither. For the overlay
/// this covers the slow path that copies a *lower* directory.
#[test]
fn renames_are_atomic_to_concurrent_listings() {
    let empty = Tree::new();
    let mut lower_tree = Tree::new();
    lower_tree.insert("/left".into(), common::Entry::Dir);
    for i in 0..20 {
        lower_tree.insert(format!("/left/f{i}"), common::Entry::File(vec![b'x'; 100]));
    }
    let cases: Vec<(&str, Arc<dyn FileSystem>)> = vec![
        ("memory", fixtures::memory(&lower_tree).fs),
        ("overlay_memory (lower dir)", fixtures::overlay_memory(&lower_tree).fs),
        ("overlay_memory (upper dir)", {
            let f = fixtures::overlay_memory(&empty);
            populate(&*f.fs, &lower_tree);
            f.fs
        }),
    ];
    for (name, fs) in cases {
        let vfs = Vfs::from_arc(fs);
        within(WATCHDOG, move || {
            let stop = Arc::new(AtomicBool::new(false));
            let checks = Arc::new(AtomicUsize::new(0));
            let observer = {
                let (vfs, stop, checks) = (vfs.clone(), stop.clone(), checks.clone());
                thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let names: Vec<String> = vfs.read_dir("/").unwrap().into_iter().map(|e| e.name).collect();
                        let present = names.iter().filter(|n| *n == "left" || *n == "right").count();
                        assert_eq!(present, 1, "observed a half-done rename: {names:?}");
                        checks.fetch_add(1, Ordering::Relaxed);
                    }
                })
            };
            // Keep renaming until the observer has genuinely raced against us.
            let mut round = 0;
            while (round < 40 || checks.load(Ordering::Relaxed) < 500) && !observer.is_finished() {
                let (from, to) = if round % 2 == 0 { ("/left", "/right") } else { ("/right", "/left") };
                vfs.rename(from, to).unwrap();
                round += 1;
            }
            if round % 2 == 1 {
                vfs.rename("/right", "/left").unwrap();
            }
            stop.store(true, Ordering::Relaxed);
            observer.join().unwrap();
            assert_eq!(vfs.read_dir("/left").unwrap().len(), 20, "{name}: contents survived the renames");
        });
    }
}

/// Many threads run random histories against one overlay while others read
/// and compute change sets. Nothing may panic or deadlock, the lower layer
/// must stay untouched, and the final state must still satisfy the commit
/// property (replaying `changes()` onto the lower reproduces the view).
#[test]
fn overlay_mutation_storm_stays_consistent() {
    within(Duration::from_secs(120), || {
        let mut rng = Rng::new(99);
        let initial = model::random_tree(&mut rng, 40);
        let lower: Arc<dyn FileSystem> = Arc::new(MemoryFs::new());
        populate(&*lower, &initial);
        let overlay = Arc::new(OverlayFs::new(MemoryFs::new(), lower.clone()));
        let vfs = Vfs::from(overlay.clone());

        let writers: Vec<_> = (0..6)
            .map(|t| {
                let vfs = vfs.clone();
                thread::spawn(move || {
                    let mut rng = Rng::new(1000 + t);
                    for _ in 0..300 {
                        let _ = model::apply(&vfs, &model::random_op(&mut rng));
                    }
                })
            })
            .collect();
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let overlay = overlay.clone();
                thread::spawn(move || {
                    for _ in 0..40 {
                        overlay.changes().unwrap();
                        let _ = snapshot_tolerant(&*overlay);
                    }
                })
            })
            .collect();
        for t in writers.into_iter().chain(readers) {
            t.join().unwrap();
        }

        assert_trees_eq(&initial, &snapshot(&*lower), "lower modified under concurrency");
        let merged = snapshot(&*overlay);
        let copy = MemoryFs::new();
        populate(&copy, &initial);
        overlay.commit(&copy).unwrap();
        assert_trees_eq(&merged, &snapshot(&copy), "commit property after the storm");
    });
}

/// The overlay's hardest consistency case. Every round renames a *lower*
/// directory (slow path: copy into the upper layer, then add a whiteout) and
/// then resets the overlay (empty the upper layer and drop the whiteouts).
/// Both operations change whiteouts and layers together; a listing that read
/// them at different moments would see the directory twice, or not at all.
///
/// Slow listings stretch the gap between "read the whiteouts" and "list the
/// layers" to milliseconds, so a missing lock is caught on almost every round
/// rather than by luck.
#[test]
fn overlay_listings_never_observe_half_applied_changes() {
    within(WATCHDOG, || {
        let lower = MemoryFs::new();
        lower.create_dir(&VPath::new("/left").unwrap()).unwrap();
        for i in 0..3 {
            openvfs::ops::write_all(&lower, &VPath::new(format!("/left/f{i}")).unwrap(), b"x").unwrap();
        }
        let overlay = Arc::new(OverlayFs::with_layers(
            Arc::new(SlowListings(MemoryFs::new())),
            vec![Arc::new(SlowListings(lower))],
        ));
        let vfs = Vfs::from(overlay.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let checks = Arc::new(AtomicUsize::new(0));
        let observer = {
            let (vfs, stop, checks) = (vfs.clone(), stop.clone(), checks.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let names: Vec<String> = vfs.read_dir("/").unwrap().into_iter().map(|e| e.name).collect();
                    assert_eq!(names.len(), 1, "observed a half-applied change: {names:?}");
                    checks.fetch_add(1, Ordering::Relaxed);
                    // Listings hold the read lock for milliseconds: yield, or on a reader-preferring RwLock
                    // (macOS, older Linux std) the writer below never gets in. Fairness is the OS's business.
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };
        // Keep going until the observer has genuinely interleaved with us.
        let mut rounds = 0;
        while (rounds < 30 || checks.load(Ordering::Relaxed) < 150) && !observer.is_finished() {
            vfs.rename("/left", "/right").unwrap();
            overlay.reset().unwrap();
            rounds += 1;
            // Without a pause this loop hogs the write lock, and where `RwLock` favours writers (Linux)
            // the observer is starved instead of racing with us.
            thread::sleep(Duration::from_millis(1));
        }
        stop.store(true, Ordering::Relaxed);
        observer.join().unwrap();
    });
}

/// A walk over a tree that changes underneath (entries may vanish between listing and reading).
fn snapshot_tolerant(fs: &dyn FileSystem) -> usize {
    let mut count = 0;
    let mut stack = vec![VPath::root()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs.read_dir(&dir) else { continue };
        for e in entries {
            count += 1;
            if e.is_dir() {
                stack.push(e.path);
            }
        }
    }
    count
}

#[test]
fn handles_are_send_and_usable_from_other_threads() {
    let dir = TempDir::new("send-handle");
    for vfs in [Vfs::memory(), Vfs::new(PhysicalFs::new(dir.path()).unwrap())] {
        let mut file = vfs.create("/moved").unwrap();
        let joined = thread::spawn(move || {
            std::io::Write::write_all(&mut file, b"from another thread").unwrap();
        });
        joined.join().unwrap();
        assert_eq!(vfs.read_to_string("/moved").unwrap(), "from another thread");
    }
}

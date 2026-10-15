//! Differential testing: random operation histories, replayed on a reference
//! `MemoryFs` and on every other backend configuration.
//!
//! At every step the canonical outcome — the result *and*, on failure, the
//! error variant with the path it reports — must be identical, and the whole
//! tree is compared periodically and at the end. Overlay fixtures must also
//! leave their lower layers byte-for-byte untouched.
//!
//! Knobs (environment variables):
//! * `OPENVFS_DIFF_CASES` — histories per backend (default 32);
//! * `OPENVFS_DIFF_STEPS` — operations per history (default 70);
//! * `OPENVFS_DIFF_SEED`  — replay one specific history;
//! * `OPENVFS_DIFF_THREADS` — threads sharing one backend's histories (default: cores, at most 8).

use crate::common;

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::fixtures::{Builder, for_each_fixture};
use common::{Rng, assert_trees_eq, env_knob, model, populate, snapshot};
use openvfs::{MemoryFs, Vfs};

fn seeds() -> Vec<u64> {
    if let Ok(seed) = std::env::var("OPENVFS_DIFF_SEED") {
        return vec![seed.parse().expect("OPENVFS_DIFF_SEED must be an integer")];
    }
    (0..env_knob("OPENVFS_DIFF_CASES", 32)).map(|i| 0x5EED_0000 + i).collect()
}

/// Replays the histories for every seed, spread over several threads.
///
/// Histories are independent (own seed, own fresh fixture), so they shard freely. That matters for
/// the deep CI run: thousands of histories per backend, each step costing system calls on the
/// disk-backed fixtures. The first failure stops the other shards and is re-raised unchanged.
fn run(name: &str, build: Builder) {
    let seeds = seeds();
    let cores = std::thread::available_parallelism().map_or(1, usize::from).min(8);
    let wanted = usize::try_from(env_knob("OPENVFS_DIFF_THREADS", cores as u64)).unwrap_or(1);
    let shards = wanted.clamp(1, seeds.len().max(1));
    let failed = AtomicBool::new(false);

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..shards)
            .map(|shard| {
                let (seeds, failed) = (&seeds, &failed);
                scope.spawn(move || {
                    let _stop_others_on_panic = StopOnPanic(failed);
                    for &seed in seeds.iter().skip(shard).step_by(shards) {
                        if failed.load(Ordering::Relaxed) {
                            break;
                        }
                        run_case(name, build, seed);
                    }
                })
            })
            .collect();
        for handle in handles {
            if let Err(panic) = handle.join() {
                std::panic::resume_unwind(panic);
            }
        }
    });
}

/// Raises a flag when dropped during a panic, so sibling shards stop early.
struct StopOnPanic<'a>(&'a AtomicBool);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.store(true, Ordering::Relaxed);
        }
    }
}

fn run_case(name: &str, build: Builder, seed: u64) {
    let steps = env_knob("OPENVFS_DIFF_STEPS", 70);
    let mut rng = Rng::new(seed);
    let initial = model::random_tree(&mut rng, 30);

    let reference_fs = Arc::new(MemoryFs::new());
    populate(&*reference_fs, &initial);
    let reference = Vfs::from(reference_fs);
    let fixture = build(&initial);
    let subject = Vfs::from_arc(Arc::clone(&fixture.fs));
    assert_trees_eq(&initial, &snapshot(&*fixture.fs), &format!("{name}: initial contents, seed {seed}"));

    let mut history = String::new();
    for step in 0..steps {
        let op = model::random_op(&mut rng);
        let expected = model::apply(&reference, &op);
        let actual = model::apply(&subject, &op);
        let _ = writeln!(history, "  #{step:<3} {op:?} => {expected}");
        assert!(
            expected == actual,
            "{name}: outcome mismatch at step {step}, seed {seed} (replay: OPENVFS_DIFF_SEED={seed})\n\
                 operation: {op:?}\n reference: {expected}\n   {name}: {actual}\nhistory:\n{history}"
        );
        if step % 10 == 9 {
            assert_trees_eq(
                &snapshot(&**reference.backend()),
                &snapshot(&*fixture.fs),
                &format!("{name}: after step {step}, seed {seed}\nhistory:\n{history}"),
            );
        }
    }
    assert_trees_eq(&snapshot(&**reference.backend()), &snapshot(&*fixture.fs), &format!("{name}: final, seed {seed}"));
    for (lower, original) in &fixture.immutable {
        assert_trees_eq(original, &snapshot(&**lower), &format!("{name}: a lower layer was modified, seed {seed}"));
    }
}

/// How long a backend's histories may run before the test is declared hung (a deadlock must fail in
/// minutes, not hours). It scales with the work requested, so deep CI runs, which replay thousands of
/// histories, still get enough time.
fn watchdog() -> Duration {
    let steps = env_knob("OPENVFS_DIFF_CASES", 32).saturating_mul(env_knob("OPENVFS_DIFF_STEPS", 70));
    Duration::from_secs(120) + Duration::from_millis(5).saturating_mul(u32::try_from(steps).unwrap_or(u32::MAX))
}

macro_rules! differential {
    ($($fixture:ident),* $(,)?) => {
        $( #[test] fn $fixture() {
            common::within(watchdog(), || run(stringify!($fixture), common::fixtures::$fixture));
        } )*
    };
}

for_each_fixture!(differential);

/// The generator itself must produce interesting histories, or the suite
/// above proves little.
#[test]
fn histories_exercise_every_operation_and_both_outcomes() {
    let mut rng = Rng::new(7);
    let vfs = Vfs::memory();
    let (mut ok, mut err) = (0, 0);
    let mut kinds = std::collections::BTreeSet::new();
    for _ in 0..2000 {
        let op = model::random_op(&mut rng);
        kinds.insert(format!("{op:?}").split('(').next().unwrap().to_owned());
        if model::apply(&vfs, &op).starts_with("Ok") { ok += 1 } else { err += 1 }
    }
    assert_eq!(kinds.len(), 15, "every operation kind is generated: {kinds:?}");
    assert!(ok > 400 && err > 400, "both successes ({ok}) and failures ({err}) are common");
}

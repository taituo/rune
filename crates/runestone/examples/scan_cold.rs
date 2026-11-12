//! Cold snapshot of a directory into a fresh shadow repo: in-process scan vs git add (separate homes).
use runestone::Engine;
use std::time::Instant;
fn main() {
    let wt = std::path::PathBuf::from(std::env::args().nth(1).expect("dir"));
    for (label, git) in [("scan", false), ("git", true), ("scan", false), ("git", true)] {
        let home = tempfile::tempdir().unwrap();
        let eng = Engine::open_in(home.path(), &wt).unwrap();
        let t0 = Instant::now();
        let t = if git { eng.world.git_snapshot().unwrap() } else { eng.world.snapshot().unwrap() };
        let cold = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = Instant::now();
        let _ = if git { eng.world.git_snapshot().unwrap() } else { eng.world.snapshot().unwrap() };
        println!("{label:<5} cold {cold:>7.1} ms  warm {:>6.1} ms  {}", t1.elapsed().as_secs_f64() * 1000.0, &t[..8]);
    }
}

//! Snapshot cost on a large plain worktree: in-process scan (cold, warm, one change) vs git add + write-tree.
//! cargo run --release --example scan_bench [files]
use runestone::Engine;
use std::time::{Duration, Instant, SystemTime};

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let old = SystemTime::now() - Duration::from_secs(3600);
    for i in 0..n {
        let d = wt.path().join(format!("d{}/e{}", i % 50, i % 7));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join(format!("f{i}.rs"));
        std::fs::write(&f, format!("fn f{i}() -> u32 {{ {i} }}\n").repeat(20)).unwrap();
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
    }
    std::thread::sleep(Duration::from_secs(3)); // past the racy window (set_modified bumps ctime)
    let eng = Engine::open_in(home.path(), wt.path()).unwrap();
    let t = |label: &str, f: &dyn Fn() -> String| {
        let t0 = Instant::now();
        let tree = f();
        println!("{label:<28} {:>7.1} ms  {}", t0.elapsed().as_secs_f64() * 1000.0, &tree[..8]);
    };
    t("scan cold", &|| eng.world.snapshot().unwrap());
    t("scan warm", &|| eng.world.snapshot().unwrap());
    t("scan warm", &|| eng.world.snapshot().unwrap());
    std::fs::write(wt.path().join("d3/e3/new.rs"), "x").unwrap();
    t("scan, one new file", &|| eng.world.snapshot().unwrap());
    let fresh = Engine::open_in(home.path(), wt.path()).unwrap();
    t("scan, new process (cache file)", &|| fresh.world.snapshot().unwrap());
    t("git add -A + write-tree", &|| eng.world.git_snapshot().unwrap());
    t("git add -A + write-tree warm", &|| eng.world.git_snapshot().unwrap());
}

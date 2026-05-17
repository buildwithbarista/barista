// Integration-test / example / benchmark target — workspace security
// lints are allowed here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`)
// is the documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped by
// the separate compilation unit each bench file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Criterion microbenchmarks for the layered configuration loader.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p barista-config --bench load
//! ```
//!
//! These are informative numbers used to spot egregious regressions
//! during development. The canonical regression detector lives in the
//! Tier-2 gate.

use std::collections::HashMap;
use std::fs;
use std::hint::black_box;
use std::path::Path;

use barista_config::sources::EnvGetter;
use barista_config::{
    CliOverrides, LoaderInputs, ProjectConfigFile, load_effective_config,
};
use criterion::{Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

/// A representative project `barista.toml` covering the main
/// declarative sections the loader walks. Embedded so the bench is
/// self-contained and doesn't need an external fixture file.
const SAMPLE_BARISTA_TOML: &str = r#"
[network]
max-concurrent-connections = 12
request-timeout-secs = 45
http2-enabled = true

[daemon]
enabled = true
idle-shutdown-secs = 600

[maven]
compat-mode = "strict"
honor-mvn-config = true
honor-jvm-config = true

[logging]
level = "info"

[telemetry]
enabled = false

[project]
name = "my-app"
group-id = "com.acme"
artifact-id = "my-app"
version = "1.2.3"
"#;

/// Pure TOML-parse cost of a small but realistic `barista.toml`.
/// Isolates the deserializer from the filesystem + layer-merge
/// machinery in `load_effective_config`.
fn bench_parse_barista_toml(c: &mut Criterion) {
    c.bench_function("ProjectConfigFile parse: sample barista.toml", |bench| {
        bench.iter(|| {
            let cfg: ProjectConfigFile =
                toml::from_str(black_box(SAMPLE_BARISTA_TOML)).unwrap();
            black_box(cfg);
        });
    });
}

/// Boxed env-getter wrapper, leaked to `'static` for `LoaderInputs`.
/// Test/bench-only shortcut — the leak is one-time per benchmark
/// process and never observed by production code.
type BoxedEnvGetter = Box<dyn Fn(&str) -> Option<String>>;

fn env_from(map: HashMap<String, String>) -> BoxedEnvGetter {
    Box::new(move |k: &str| map.get(k).cloned())
}

fn make_inputs(
    home: &Path,
    cwd: &Path,
    env_map: HashMap<String, String>,
) -> LoaderInputs<'static> {
    let getter = Box::leak(Box::new(env_from(env_map))) as &dyn Fn(&str) -> Option<String>;
    let getter: &'static EnvGetter<'static> = getter;
    LoaderInputs {
        home_override: Some(home.to_path_buf()),
        cwd_override: Some(cwd.to_path_buf()),
        env_get: Some(getter),
        cli: CliOverrides::default(),
        ..Default::default()
    }
}

/// End-to-end `load_effective_config` on a sandbox containing a
/// representative project `barista.toml`. Exercises the file-read,
/// TOML-parse, and layer-merge code paths.
///
/// The sandbox is set up once outside `iter` so we measure load
/// cost, not TempDir creation.
fn bench_load_effective_project_only(c: &mut Criterion) {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let proj_cfg = proj.path().join("barista.toml");
    fs::write(&proj_cfg, SAMPLE_BARISTA_TOML).unwrap();

    c.bench_function(
        "load_effective_config: project barista.toml only",
        |bench| {
            bench.iter(|| {
                let mut inputs = make_inputs(home.path(), proj.path(), HashMap::new());
                inputs.project_config_path = Some(proj_cfg.clone());
                let (cfg, audit) = load_effective_config(black_box(inputs)).unwrap();
                black_box((cfg, audit));
            });
        },
    );
}

/// Same as above but with a clutch of `BARISTA_*` env overrides
/// applied on top. Isolates the env-var parse + merge cost relative
/// to the project-only baseline.
fn bench_load_effective_with_env_overrides(c: &mut Criterion) {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let proj_cfg = proj.path().join("barista.toml");
    fs::write(&proj_cfg, SAMPLE_BARISTA_TOML).unwrap();

    let env_map: HashMap<String, String> = [
        ("BARISTA_NETWORK__MAX_CONCURRENT_CONNECTIONS", "32"),
        ("BARISTA_NETWORK__REQUEST_TIMEOUT_SECS", "120"),
        ("BARISTA_LOGGING__LEVEL", "debug"),
        ("BARISTA_MAVEN__COMPAT_MODE", "strict"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect();

    c.bench_function(
        "load_effective_config: project + 4 env overrides",
        |bench| {
            bench.iter(|| {
                let mut inputs = make_inputs(home.path(), proj.path(), env_map.clone());
                inputs.project_config_path = Some(proj_cfg.clone());
                let (cfg, audit) = load_effective_config(black_box(inputs)).unwrap();
                black_box((cfg, audit));
            });
        },
    );
}

criterion_group!(
    benches,
    bench_parse_barista_toml,
    bench_load_effective_project_only,
    bench_load_effective_with_env_overrides,
);
criterion_main!(benches);

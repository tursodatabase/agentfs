//! Workload distribution benchmark for OverlayFS operations.
//!
//! This benchmark simulates realistic workloads based on observed operation
//! distributions from tools like `npx create-react-app`.
//!
//! Run with: cargo bench --bench workload

use agentfs_sdk::filesystem::{AgentFS, FileSystem, HostFS, OverlayFS};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rand::distributions::WeightedIndex;
use rand::prelude::*;
use std::sync::Arc;
use tempfile::tempdir;

/// Operation types that can be performed on the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    CreateFile,
    Lstat,
    Mkdir,
    Open,
    ReaddirPlus,
    Stat,
}

/// Configuration for workload generation.
///
/// Probabilities are relative weights (they will be normalized internally).
#[derive(Debug, Clone)]
struct WorkloadConfig {
    /// Weight for create_file operations
    pub create_file_weight: u32,
    /// Weight for lstat operations
    pub lstat_weight: u32,
    /// Weight for mkdir operations
    pub mkdir_weight: u32,
    /// Weight for open operations
    pub open_weight: u32,
    /// Weight for readdir_plus operations
    pub readdir_plus_weight: u32,
    /// Weight for stat operations
    pub stat_weight: u32,

    /// Minimum path depth (e.g., 2 for "/project/file.txt")
    pub min_depth: usize,
    /// Maximum path depth (e.g., 6 for deep node_modules paths)
    pub max_depth: usize,

    /// Number of operations to perform in each benchmark iteration
    pub operations_per_iter: usize,
}

impl WorkloadConfig {
    /// Create a config matching `npx create-react-app` workload distribution.
    ///
    /// Based on observed operation counts:
    /// - create_file: 4469 (~13%)
    /// - lstat: 19565 (~57%)
    /// - mkdir: 2469 (~7%)
    /// - open: 7 (~0%)
    /// - readdir_plus: 2610 (~8%)
    /// - stat: 5354 (~15%)
    pub fn create_react_app() -> Self {
        Self {
            create_file_weight: 4469,
            lstat_weight: 19565,
            mkdir_weight: 2469,
            open_weight: 7,
            readdir_plus_weight: 2610,
            stat_weight: 5354,
            min_depth: 2,
            max_depth: 6,
            operations_per_iter: 1000,
        }
    }

    /// Create a read-heavy workload (mostly stat/lstat operations).
    pub fn read_heavy() -> Self {
        Self {
            create_file_weight: 5,
            lstat_weight: 50,
            mkdir_weight: 0,
            open_weight: 5,
            readdir_plus_weight: 20,
            stat_weight: 20,
            min_depth: 2,
            max_depth: 6,
            operations_per_iter: 1000,
        }
    }

    /// Create a write-heavy workload (mostly create_file/mkdir operations).
    pub fn write_heavy() -> Self {
        Self {
            create_file_weight: 60,
            lstat_weight: 10,
            mkdir_weight: 20,
            open_weight: 5,
            readdir_plus_weight: 0,
            stat_weight: 5,
            min_depth: 2,
            max_depth: 6,
            operations_per_iter: 1000,
        }
    }

    /// Get weights as a vector for WeightedIndex.
    fn weights(&self) -> Vec<u32> {
        vec![
            self.create_file_weight,
            self.lstat_weight,
            self.mkdir_weight,
            self.open_weight,
            self.readdir_plus_weight,
            self.stat_weight,
        ]
    }
}

/// Generates realistic paths similar to npm package structure.
struct PathGenerator {
    /// Pre-generated package-like names for realistic paths
    package_names: Vec<&'static str>,
    /// Common file names
    file_names: Vec<&'static str>,
    /// Common directory names
    dir_names: Vec<&'static str>,
    /// Random number generator
    rng: StdRng,
    /// Path depth distribution
    min_depth: usize,
    max_depth: usize,
}

impl PathGenerator {
    fn new(seed: u64, min_depth: usize, max_depth: usize) -> Self {
        Self {
            package_names: vec![
                "react",
                "lodash",
                "webpack",
                "babel",
                "eslint",
                "jest",
                "typescript",
                "express",
                "axios",
                "moment",
                "@types",
                "@babel",
                "@jest",
                "postcss",
                "cssnano",
                "autoprefixer",
                "browserslist",
                "caniuse-lite",
                "source-map",
                "acorn",
            ],
            file_names: vec![
                "index.js",
                "index.ts",
                "package.json",
                "README.md",
                "LICENSE",
                "index.d.ts",
                "main.js",
                "module.js",
                "types.d.ts",
                "utils.js",
            ],
            dir_names: vec![
                "src",
                "lib",
                "dist",
                "types",
                "test",
                "tests",
                "node_modules",
                "cjs",
                "esm",
                "build",
            ],
            rng: StdRng::seed_from_u64(seed),
            min_depth,
            max_depth,
        }
    }

    /// Generate a random path with the configured depth range.
    fn generate_path(&mut self, is_directory: bool) -> String {
        let depth = self.rng.gen_range(self.min_depth..=self.max_depth);
        let mut components = Vec::with_capacity(depth);

        // First component is usually a project name
        components.push("project");

        // For npm-like paths, second component is often "node_modules"
        if depth > 2 && self.rng.gen_bool(0.8) {
            components.push("node_modules");

            // Add package name(s)
            let remaining = depth - 2 - if is_directory { 0 } else { 1 };
            for i in 0..remaining {
                // Sometimes have nested node_modules (like eslint/node_modules/...)
                if i > 0 && self.rng.gen_bool(0.2) {
                    components.push("node_modules");
                }
                let pkg = self.package_names[self.rng.gen_range(0..self.package_names.len())];
                components.push(pkg);

                // Add subdirectory sometimes
                if i < remaining - 1 && self.rng.gen_bool(0.5) {
                    let dir = self.dir_names[self.rng.gen_range(0..self.dir_names.len())];
                    components.push(dir);
                }
            }
        } else {
            // Simple path like /project/src/file.js
            for _ in 1..depth - if is_directory { 0 } else { 1 } {
                let dir = self.dir_names[self.rng.gen_range(0..self.dir_names.len())];
                components.push(dir);
            }
        }

        // Add filename for non-directory paths
        if !is_directory {
            let file = self.file_names[self.rng.gen_range(0..self.file_names.len())];
            components.push(file);
        }

        format!("/{}", components.join("/"))
    }

    /// Generate a path for a file operation.
    fn file_path(&mut self) -> String {
        self.generate_path(false)
    }

    /// Generate a path for a directory operation.
    fn dir_path(&mut self) -> String {
        self.generate_path(true)
    }
}

/// Workload generator that produces operations according to configured distribution.
struct WorkloadGenerator {
    config: WorkloadConfig,
    path_gen: PathGenerator,
    op_dist: WeightedIndex<u32>,
    rng: StdRng,
}

impl WorkloadGenerator {
    fn new(config: WorkloadConfig, seed: u64) -> Self {
        let weights = config.weights();
        let op_dist = WeightedIndex::new(&weights).expect("Invalid weights");
        let path_gen = PathGenerator::new(seed, config.min_depth, config.max_depth);

        Self {
            config,
            path_gen,
            op_dist,
            rng: StdRng::seed_from_u64(seed),
        }
    }

    /// Generate the next operation and its path.
    fn next_operation(&mut self) -> (Operation, String) {
        let op_idx = self.op_dist.sample(&mut self.rng);
        let op = match op_idx {
            0 => Operation::CreateFile,
            1 => Operation::Lstat,
            2 => Operation::Mkdir,
            3 => Operation::Open,
            4 => Operation::ReaddirPlus,
            5 => Operation::Stat,
            _ => unreachable!(),
        };

        let path = match op {
            Operation::CreateFile | Operation::Open => self.path_gen.file_path(),
            Operation::Mkdir | Operation::ReaddirPlus => self.path_gen.dir_path(),
            Operation::Lstat | Operation::Stat => {
                // These can be either files or directories
                if self.rng.gen_bool(0.7) {
                    self.path_gen.file_path()
                } else {
                    self.path_gen.dir_path()
                }
            }
        };

        (op, path)
    }

    /// Generate a batch of operations.
    fn generate_batch(&mut self) -> Vec<(Operation, String)> {
        (0..self.config.operations_per_iter)
            .map(|_| self.next_operation())
            .collect()
    }
}

/// Execute a single operation on the overlay filesystem.
async fn execute_operation(overlay: &OverlayFS, op: Operation, path: &str) {
    match op {
        Operation::CreateFile => {
            // Ignore errors - path may not exist, which is expected
            let _ = overlay.create_file(path, 0o100644, 0, 0).await;
        }
        Operation::Lstat => {
            let _ = overlay.lstat(path).await;
        }
        Operation::Mkdir => {
            let _ = overlay.mkdir(path, 0, 0).await;
        }
        Operation::Open => {
            let _ = overlay.open(path).await;
        }
        Operation::ReaddirPlus => {
            let _ = overlay.readdir_plus(path).await;
        }
        Operation::Stat => {
            let _ = overlay.stat(path).await;
        }
    }
}

fn bench_workload(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("workload");

    // Test different workload configurations
    let configs = vec![
        ("create_react_app", WorkloadConfig::create_react_app()),
        ("read_heavy", WorkloadConfig::read_heavy()),
        ("write_heavy", WorkloadConfig::write_heavy()),
    ];

    for (name, config) in configs {
        let ops_per_iter = config.operations_per_iter;
        group.throughput(Throughput::Elements(ops_per_iter as u64));

        group.bench_with_input(BenchmarkId::new("ops", name), &config, |b, config| {
            b.iter_batched(
                || {
                    // Setup: create overlay and generate workload
                    rt.block_on(async {
                        let base_dir = tempdir().expect("Failed to create base temp dir");
                        let delta_dir = tempdir().expect("Failed to create delta temp dir");

                        let base = Arc::new(
                            HostFS::new(base_dir.path()).expect("Failed to create HostFS"),
                        );
                        let db_path = delta_dir.path().join("delta.db");
                        let delta = AgentFS::new(db_path.to_str().unwrap())
                            .await
                            .expect("Failed to create AgentFS");

                        let overlay = OverlayFS::new(base, delta);
                        overlay
                            .init(base_dir.path().to_str().unwrap())
                            .await
                            .expect("Failed to init overlay");

                        // Pre-generate workload with deterministic seed for reproducibility
                        let mut generator = WorkloadGenerator::new(config.clone(), 42);
                        let operations = generator.generate_batch();

                        (overlay, operations, base_dir, delta_dir)
                    })
                },
                |(overlay, operations, _base_dir, _delta_dir)| {
                    rt.block_on(async {
                        for (op, path) in operations {
                            execute_operation(&overlay, op, &path).await;
                        }
                    });
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

/// Benchmark individual operation types for comparison.
fn bench_individual_ops(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("individual_ops");

    // Benchmark each operation type separately with 100 iterations
    let operations = vec![
        ("stat", Operation::Stat),
        ("lstat", Operation::Lstat),
        ("mkdir", Operation::Mkdir),
        ("create_file", Operation::CreateFile),
        ("readdir_plus", Operation::ReaddirPlus),
    ];

    for (name, op) in operations {
        group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    rt.block_on(async {
                        let base_dir = tempdir().expect("Failed to create base temp dir");
                        let delta_dir = tempdir().expect("Failed to create delta temp dir");

                        let base = Arc::new(
                            HostFS::new(base_dir.path()).expect("Failed to create HostFS"),
                        );
                        let db_path = delta_dir.path().join("delta.db");
                        let delta = AgentFS::new(db_path.to_str().unwrap())
                            .await
                            .expect("Failed to create AgentFS");

                        let overlay = OverlayFS::new(base, delta);
                        overlay
                            .init(base_dir.path().to_str().unwrap())
                            .await
                            .expect("Failed to init overlay");

                        // Generate paths for this operation
                        let config = WorkloadConfig::create_react_app();
                        let mut path_gen =
                            PathGenerator::new(42, config.min_depth, config.max_depth);
                        let paths: Vec<String> = (0..100)
                            .map(|_| match op {
                                Operation::CreateFile | Operation::Open => path_gen.file_path(),
                                Operation::Mkdir | Operation::ReaddirPlus => path_gen.dir_path(),
                                Operation::Lstat | Operation::Stat => path_gen.file_path(),
                            })
                            .collect();

                        (overlay, paths, base_dir, delta_dir)
                    })
                },
                |(overlay, paths, _base_dir, _delta_dir)| {
                    rt.block_on(async {
                        for path in &paths {
                            execute_operation(&overlay, op, path).await;
                        }
                    });
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_workload, bench_individual_ops);
criterion_main!(benches);

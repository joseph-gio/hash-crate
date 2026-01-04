#![doc = include_str!("../README.md")]

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::btree_map::Entry;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use argh::FromArgs;
use cargo_metadata::MetadataCommand;
use cargo_metadata::Node;
use cargo_metadata::Package;
use cargo_metadata::PackageId;
use log::trace;
use rayon::iter::ParallelIterator as _;
use rayon::slice::ParallelSlice as _;

/// compute a deterministic hash for a local rust crate
#[derive(FromArgs)]
struct CliArgs {
    /// path to the relevant Cargo.toml file
    #[argh(option)]
    manifest_path: Option<PathBuf>,
    /// the binary crate for which to create a hash. multiple binaries from the same workspace may be specified.
    #[argh(option)]
    #[argh(long = "bin")]
    bins: Vec<String>,
    /// do not update the Cargo.lock file, erroring if it does not exist or is not up to date
    #[argh(switch)]
    locked: bool,
}

fn main() {
    simple_logger::SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .env()
        .init()
        .unwrap();

    let CliArgs {
        manifest_path,
        bins,
        locked,
    } = argh::from_env();

    if bins.is_empty() {
        panic!("did not receive any `bin` flags");
    }

    let mut metadata_command = MetadataCommand::new();
    metadata_command.features(cargo_metadata::CargoOpt::AllFeatures);
    if let Some(manifest_path) = manifest_path {
        metadata_command.manifest_path(manifest_path);
    }
    if locked {
        metadata_command.other_options(["--locked".to_owned()]);
    }

    let metadata = metadata_command
        .exec()
        .unwrap_or_else(|error| panic!("{error}"));

    trace!("ran cargo-metadta for {} packages", metadata.packages.len());

    dbg!(metadata.packages.len());

    let package_map = metadata
        .packages
        .iter()
        .map(|p| (&p.id, p))
        .collect::<HashMap<_, _>>();

    // package ids corresponding to each of `bins`.
    let bin_packages = {
        let arbitrary_placeholder_package = metadata.packages.first().unwrap();
        let mut package_slots = vec![arbitrary_placeholder_package; bins.len()];

        let mut bin_names_to_slots =
            std::iter::zip(bins.iter().map(String::as_str), &mut package_slots)
                .collect::<HashMap<_, _>>();

        let workspace_packages = metadata.workspace_members.iter().map(|id| package_map[id]);
        'packages: for package in workspace_packages {
            for target in &package.targets {
                if !target.kind.contains(&cargo_metadata::TargetKind::Bin) {
                    continue;
                }
                if let Some(slot) = bin_names_to_slots.remove(&target.name.as_str()) {
                    *slot = package;
                    if bin_names_to_slots.is_empty() {
                        break 'packages;
                    }
                }
            }
        }

        if !bin_names_to_slots.is_empty() {
            let missing_bins = bin_names_to_slots
                .keys()
                .copied()
                .collect::<Vec<_>>()
                .join(", ");
            panic!("could not find workspace packages for `{missing_bins}`");
        }

        package_slots
    };

    let Some(metadata_resolve) = &metadata.resolve else {
        panic!("cargo-metadata did not output a resolved dependency graph");
    };

    let package_nodes = metadata_resolve
        .nodes
        .iter()
        .map(|node| (&node.id, node))
        .collect::<HashMap<_, _>>();

    let hashes_per_package = Mutex::new(HashMap::<&PackageId, HashedInfo>::new());

    rayon::scope(|scope| {
        for &root_package in &bin_packages {
            scope.spawn(|scope| {
                let hashed_info =
                    hash_root_crate(root_package, &package_map, &package_nodes, scope);
                hashes_per_package
                    .lock()
                    .unwrap()
                    .insert(&root_package.id, hashed_info);
            });
        }
    });

    let hashes_per_package = hashes_per_package.into_inner().unwrap();
    let hashes_per_bin = std::iter::zip(&bins, &bin_packages)
        .map(|(bin_name, &bin_package)| {
            let hashed_info = hashes_per_package.get(&bin_package.id).unwrap();
            let mut hasher = blake3::Hasher::new();
            hasher.update(hashed_info.first_hash.as_bytes());

            let path_dependencies = hashed_info.path_dependency_hashes.lock().unwrap();
            trace!(
                "collecting hashes from {} local packages",
                path_dependencies.len()
            );
            for path_dep_hash in path_dependencies.iter() {
                hasher.update(path_dep_hash.0.as_bytes());
            }

            (bin_name.as_str(), hasher.finalize().to_string())
        })
        .collect::<HashMap<_, _>>();

    println!("{}", serde_json::to_string(&hashes_per_bin).unwrap());
}

struct HashedInfo {
    first_hash: blake3::Hash,
    path_dependency_hashes: Arc<Mutex<BTreeSet<Key>>>,
}

fn hash_root_crate<'s>(
    root_package: &'s Package,
    package_map: &HashMap<&PackageId, &'s Package>,
    packages_to_nodes: &HashMap<&PackageId, &Node>,
    scope: &rayon::Scope<'s>,
) -> HashedInfo {
    let root_node = packages_to_nodes[&root_package.id];

    let mut hasher = blake3::Hasher::new();
    hasher.update(root_node.id.repr.as_bytes());
    for feature in &root_node.features {
        hasher.update(feature.as_bytes());
    }

    let mut collected_package_hashes = BTreeMap::new();

    let local_package_hashes = Arc::new(Mutex::new(BTreeSet::new()));

    scope.spawn({
        let package_path = root_package
            .manifest_path
            .parent()
            .expect("couldn't get parent path for root package");
        let path_dependency_hashes = Arc::clone(&local_package_hashes);
        move |_| {
            let hash = hash_directory(package_path.as_std_path());
            path_dependency_hashes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(Key(hash));
        }
    });

    let mut deps_to_walk = root_node.deps.iter().collect::<Vec<_>>();
    while let Some(dep) = deps_to_walk.pop() {
        let node = packages_to_nodes[&dep.pkg];

        match collected_package_hashes.entry(&dep.pkg) {
            Entry::Vacant(entry) => {
                let mut hasher = blake3::Hasher::new();
                hasher.update(dep.pkg.repr.as_bytes());
                for feature in &node.features {
                    hasher.update(feature.as_bytes());
                }
                entry.insert(hasher);
            }
            Entry::Occupied(entry) => {
                let hasher = entry.into_mut();
                for feature in &node.features {
                    hasher.update(feature.as_bytes());
                }
                continue;
            }
        }

        let package_info = package_map[&dep.pkg];

        if package_info.source.is_none() {
            trace!("hashing local project `{}`", dep.pkg);

            scope.spawn({
                let package_path = package_info
                    .manifest_path
                    .parent()
                    .expect("couldn't get parent path for local dependency");
                let path_dependency_hashes = Arc::clone(&local_package_hashes);
                move |_| {
                    let hash = hash_directory(package_path.as_std_path());
                    path_dependency_hashes
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(Key(hash));
                }
            });
        }

        deps_to_walk.extend(&node.deps);
    }

    eprintln!(
        "collecting hash from {} dependencies",
        collected_package_hashes.len()
    );

    let collected_package_hashes = collected_package_hashes.values().collect::<Vec<_>>();
    let hashed_chunks = collected_package_hashes
        .par_chunks(128)
        .map(|hashes| {
            let mut hasher = blake3::Hasher::new();
            for &collected_hasher in hashes {
                hasher.update(collected_hasher.finalize().as_bytes());
            }
            hasher.finalize()
        })
        .collect::<Vec<_>>();
    for chunk_hash in hashed_chunks {
        hasher.update(chunk_hash.as_bytes());
    }

    HashedInfo {
        first_hash: hasher.finalize(),
        path_dependency_hashes: local_package_hashes,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key(blake3::Hash);

impl Ord for Key {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.as_bytes().cmp(other.0.as_bytes())
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn hash_directory(dir_path: &Path) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    // TODO: respect gitignore potentially
    for entry in walkdir::WalkDir::new(dir_path)
        .follow_links(true)
        .sort_by_file_name()
    {
        let entry = entry.unwrap_or_else(|err| {
            panic!("failed to read entry for directory `{dir_path:?}`: {err}")
        });
        if entry.file_type().is_file() {
            let entry_relative_path: &Path = entry
                .path()
                .strip_prefix(dir_path)
                .expect("failed to get relative path");

            hasher.update(entry_relative_path.as_os_str().as_encoded_bytes());

            hasher.update_mmap_rayon(entry.path()).unwrap();
        }
    }

    hasher.finalize()
}

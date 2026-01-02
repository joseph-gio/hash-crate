#![doc = include_str!("../README.md")]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use log::trace;

fn main() {
    simple_logger::SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .env()
        .init()
        .unwrap();

    let mut args = std::env::args_os().skip(1);

    let manifest_path = args.next().expect("expected an argument");
    let manifest_path = Path::new(&manifest_path);

    let cargo_path = std::env::var_os("CARGO_PATH");
    let cargo_path = cargo_path.as_deref().unwrap_or(OsStr::new("cargo"));

    let mut cargo_tree = Command::new(cargo_path)
        .arg("tree")
        .arg("--all-features")
        .arg("--manifest-path")
        .arg(manifest_path)
        .arg("--prefix")
        .arg("none")
        .arg("--format")
        .arg("{p}|{f}|{r}")
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to start cargo tree");

    let mut key_values = BTreeMap::<Key, blake3::Hash>::new();

    let path_dependency_hashes = BTreeMap::<Key, blake3::Hash>::new();
    let path_dependency_hashes = Arc::new(Mutex::new(path_dependency_hashes));

    rayon::scope(|scope| {
        read_lines(BufReader::new(cargo_tree.stdout.take().unwrap()), |line| {
            let line = line.trim_ascii_end();
            if line.ends_with(b"(*)") {
                return;
            }

            let mut fields = line.split(|&b| b == b'|');
            if let Some(package_ident) = fields.next()
                && let Some(package_features) = fields.next()
                && let Some(package_repository) = fields.next()
            {
                let mut package_repository = package_repository.trim_ascii();

                let mut package_ident = package_ident
                    .strip_suffix(b"(proc-macro)")
                    .unwrap_or(package_ident);

                let mut original_source = None;

                // cargo tree uses a different format for workspace members
                // or usages of cargo patch
                if let Some(paren_start) = package_ident.iter().rposition(|&b| b == b'(')
                    && let Some(paren_end) = package_ident.iter().rposition(|&b| b == b')')
                {
                    if !package_repository.is_empty() {
                        original_source = Some(package_repository);
                    }
                    package_repository = package_ident[paren_start + 1..paren_end].trim_ascii();
                    package_ident = package_ident[..paren_start].trim_ascii_end();
                }

                let mut key_hasher = blake3::Hasher::new();
                key_hasher.update(package_ident);
                let key = Key(key_hasher.finalize());

                let mut value_hasher = blake3::Hasher::new();
                value_hasher.update(package_features);

                if let Some(original_source) = original_source {
                    value_hasher.update(original_source);
                }

                let package_repository = package_repository.trim_ascii_start();
                if package_repository.starts_with(b"https://")
                    || package_repository.starts_with(b"http://")
                {
                    value_hasher.update(package_features);
                } else if !package_repository.is_empty() {
                    // FIXME: we should not require utf8
                    let repo_path_str =
                        str::from_utf8(package_repository).expect("invalid utf8 for repo path");
                    let repo_path = Path::new(OsStr::new(repo_path_str)).to_path_buf();
                    let path_dependency_hashes = Arc::clone(&path_dependency_hashes);
                    scope.spawn(move |_| {
                        trace!(
                            "Computing hash for path dependency `{}`",
                            repo_path.display()
                        );
                        let hash = hash_directory(&repo_path);
                        path_dependency_hashes
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(key, hash);
                    });
                }

                let value = value_hasher.finalize();

                key_values.insert(key, value);
            } else {
                panic!("invalid format for line `{line:?}`");
            }
        });

        let status = cargo_tree.wait().expect("failed to wait on cargo-tree");
        if !status.success() {
            panic!("cargo-tree returned failed status: {status}");
        }
    });

    let path_dependency_hashes = path_dependency_hashes
        .lock()
        .unwrap_or_else(PoisonError::into_inner);

    trace!(
        "Computing final hash from {} crates, including {} path dependencies",
        key_values.len(),
        path_dependency_hashes.len()
    );

    let mut final_hasher = blake3::Hasher::new();
    for (Key(key), value) in key_values {
        final_hasher.update(key.as_bytes());
        final_hasher.update(value.as_bytes());
    }
    for value in path_dependency_hashes
        // key has already been hashed above
        .values()
    {
        final_hasher.update(value.as_bytes());
    }
    let final_hash = final_hasher.finalize();
    println!("{}", final_hash);
}

fn read_lines(mut reader: impl BufRead, mut f: impl FnMut(&[u8])) {
    let mut buf = Vec::new();
    while reader
        .read_until(b'\n', &mut buf)
        .expect("failed to read line from cargo tree")
        != 0
    {
        if buf.ends_with(b"\n") {
            buf.pop();
        }
        f(&buf);
        buf.clear();
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

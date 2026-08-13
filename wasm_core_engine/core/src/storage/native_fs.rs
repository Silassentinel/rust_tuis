//! `Store` implementation backed by real files — the native target only.
//! `std::fs` doesn't mean the same thing on `wasm32-unknown-unknown` (see
//! `HANDOVER.md`), so this whole module is compiled out there rather than
//! compiled-and-broken.

use std::fs;
use std::io;
use std::path::PathBuf;

use super::{Store, StoreError};

pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn resolve(&self, key: &str) -> Result<PathBuf, StoreError> {
        if key.is_empty() {
            return Err(StoreError("key must not be empty".to_string()));
        }
        if key.starts_with('/') || key.split('/').any(|part| part == "..") {
            return Err(StoreError(format!("invalid key: {key}")));
        }
        Ok(self.root.join(key))
    }
}

impl Store for FsStore {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.resolve(key)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError(e.to_string())),
        }
    }

    fn put(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| StoreError(e.to_string()))?;
        }
        fs::write(&path, value).map_err(|e| StoreError(e.to_string()))
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let path = self.resolve(key)?;
        fs::remove_file(&path).map_err(|e| StoreError(e.to_string()))
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let dir = if prefix.is_empty() { self.root.clone() } else { self.resolve(prefix)? };
        let entries = fs::read_dir(&dir).map_err(|e| StoreError(e.to_string()))?;
        let mut keys = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| StoreError(e.to_string()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            keys.push(if prefix.is_empty() { name } else { format!("{prefix}/{name}") });
        }
        keys.sort();
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "wasm_core_engine_test_{}_{name}_{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn put_then_get_round_trips_bytes() {
        let dir = TempDir::new("put_get");
        let store = FsStore::new(&dir.0);
        store.put("a.txt", b"hello").unwrap();
        assert_eq!(store.get("a.txt").unwrap(), Some(b"hello".to_vec()));
    }

    #[test]
    fn get_of_a_missing_key_is_none_not_an_error() {
        let dir = TempDir::new("missing");
        let store = FsStore::new(&dir.0);
        assert_eq!(store.get("nope.txt").unwrap(), None);
    }

    #[test]
    fn put_creates_intermediate_directories() {
        let dir = TempDir::new("nested");
        let store = FsStore::new(&dir.0);
        store.put("a/b/c.txt", b"nested").unwrap();
        assert_eq!(store.get("a/b/c.txt").unwrap(), Some(b"nested".to_vec()));
    }

    #[test]
    fn delete_removes_a_key() {
        let dir = TempDir::new("delete");
        let store = FsStore::new(&dir.0);
        store.put("a.txt", b"x").unwrap();
        store.delete("a.txt").unwrap();
        assert_eq!(store.get("a.txt").unwrap(), None);
    }

    #[test]
    fn list_returns_sorted_keys_under_a_prefix() {
        let dir = TempDir::new("list");
        let store = FsStore::new(&dir.0);
        store.put("b.txt", b"1").unwrap();
        store.put("a.txt", b"2").unwrap();
        assert_eq!(store.list("").unwrap(), vec!["a.txt".to_string(), "b.txt".to_string()]);
    }

    #[test]
    fn rejects_keys_that_escape_the_root() {
        let dir = TempDir::new("escape");
        let store = FsStore::new(&dir.0);
        assert!(store.put("../escape.txt", b"x").is_err());
        assert!(store.put("/etc/passwd", b"x").is_err());
        assert!(store.get("a/../../escape.txt").is_err());
    }
}

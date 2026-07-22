//! Minimal filesystem-backed Directory (subset of `store/FSDirectory.java`).
//!
//! Only what the writer needs: create outputs, fsync files, rename
//! (for the two-phase segments_N commit), fsync the directory itself,
//! list / exists / delete.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::io::{BufferedIndexInput, ChecksumIndexOutput, HeapIndexInput, IndexInput, IndexOutput};

#[derive(Clone)]
pub struct FSDirectory {
    root: PathBuf,
}

impl FSDirectory {
    /// Opens (and creates if missing) a directory.
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        Ok(FSDirectory { root })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Directory.createOutput: creates a new file, failing if it exists
    /// (FSDirectory uses CREATE_NEW semantics).
    pub fn create_output(&self, name: &str) -> io::Result<ChecksumIndexOutput> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.resolve(name))?;
        Ok(ChecksumIndexOutput::new(IndexOutput::from_file(file)))
    }

    /// Directory.sync: fsyncs the given files (FSyncDirectory.wrap / FSDirectory.sync).
    pub fn sync(&self, names: &[&str]) -> io::Result<()> {
        for name in names {
            File::open(self.resolve(name))?.sync_all()?;
        }
        Ok(())
    }

    /// Directory.rename (StoreDirectory.rename: atomic same-dir rename).
    pub fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        fs::rename(self.resolve(from), self.resolve(to))
    }

    /// Directory.syncMetaData: on Linux this fsyncs the directory fd so that
    /// renames/creations are durable (FSDirectory.syncMetaData).
    pub fn sync_metadata(&self) -> io::Result<()> {
        File::open(&self.root)?.sync_all()
    }

    /// Directory.listAll: file names in the directory (no subdirectories).
    pub fn list_all(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
        names.sort_unstable();
        Ok(names)
    }

    pub fn file_exists(&self, name: &str) -> bool {
        self.resolve(name).is_file()
    }

    pub fn delete(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.resolve(name))
    }

    /// Opens an existing file for reading. Small files (< 1 MB) are read
    /// entirely into memory (HeapIndexInput); large files use buffered reads.
    pub fn open_input(&self, name: &str) -> io::Result<Box<dyn IndexInput>> {
        let path = self.resolve(name);
        let metadata = std::fs::metadata(&path)?;
        if metadata.len() < 1_048_576 {
            // Small file: read entirely into memory
            let mut file = std::fs::File::open(&path)?;
            let mut buf = Vec::with_capacity(metadata.len() as usize);
            file.read_to_end(&mut buf)?;
            Ok(Box::new(HeapIndexInput::new(buf)))
        } else {
            // Large file: buffered reading
            let file = OpenOptions::new().read(true).open(&path)?;
            Ok(Box::new(BufferedIndexInput::new(file)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codec-lucene9-dirt-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn lifecycle() {
        let root = temp_dir("lifecycle");
        let dir = FSDirectory::open(&root).unwrap();
        assert!(!dir.file_exists("a"));
        let mut out = dir.create_output("a").unwrap();
        out.write_bytes(b"xy").unwrap();
        out.flush().unwrap();
        drop(out);
        assert!(dir.file_exists("a"));
        assert!(dir.create_output("a").is_err(), "create_new must fail");
        dir.sync(&["a"]).unwrap();
        dir.rename("a", "b").unwrap();
        assert!(!dir.file_exists("a") && dir.file_exists("b"));
        assert_eq!(dir.list_all().unwrap(), vec!["b".to_string()]);
        dir.sync_metadata().unwrap();
        dir.delete("b").unwrap();
        assert_eq!(dir.list_all().unwrap().len(), 0);
        fs::remove_dir_all(&root).unwrap();
    }
}

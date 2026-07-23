//! Minimal filesystem-backed Directory (subset of `store/FSDirectory.java`).
//!
//! Only what the index layer needs: create outputs, open inputs, fsync files,
//! rename (for the two-phase segments_N commit), fsync the directory itself,
//! list / exists / delete.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::io::{ChecksumIndexInput, ChecksumIndexOutput, IndexInput, IndexOutput};

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

    /// Directory.openInput: opens an existing file for reading.
    pub fn open_input(&self, name: &str) -> io::Result<IndexInput> {
        let file = File::open(self.resolve(name))?;
        let length = file.metadata()?.len();
        Ok(IndexInput::from_file(file, length))
    }

    /// Directory.openChecksumInput (commit/codec metadata files are read
    /// this way, with CodecUtil.checkFooter at the end).
    pub fn open_checksum_input(&self, name: &str) -> io::Result<ChecksumIndexInput> {
        Ok(ChecksumIndexInput::new(self.open_input(name)?))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::DataInput;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("codec-lucene9-dirt-{}-{}", tag, std::process::id()));
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

    #[test]
    fn open_input_reads_what_output_wrote() {
        let root = temp_dir("open_input");
        let dir = FSDirectory::open(&root).unwrap();
        {
            let mut out = dir.create_output("f").unwrap();
            out.write_int(0x01020304).unwrap();
            out.write_vint(300).unwrap();
            out.flush().unwrap();
        }
        let mut input = dir.open_input("f").unwrap();
        assert_eq!(input.length(), 6);
        assert_eq!(input.read_int().unwrap(), 0x01020304);
        assert_eq!(input.read_vint().unwrap(), 300);
        // 文件读走 slice + 独立定位
        let mut s0 = input.slice(0, 4).unwrap();
        let mut s4 = input.slice(4, 1).unwrap();
        assert_eq!(s4.read_byte().unwrap(), 0xAC);
        assert_eq!(s0.read_int().unwrap(), 0x01020304);
        fs::remove_dir_all(&root).unwrap();
    }
}

use super::{Engine, Status};
use crate::error::Result;

use fs4::FileExt;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// A very simple variant of BitCask, itself a very simple log-structured
/// key-value engine used e.g. by the Riak database. It is not compatible with
/// BitCask databases generated by other implementations. See:
/// https://riak.com/assets/bitcask-intro.pdf
///
/// BitCask writes key-value pairs to an append-only log file, and keeps a
/// mapping of keys to file positions in memory. All live keys must fit in
/// memory. Deletes write a tombstone value to the log file. To remove old
/// garbage, logs can be compacted by writing new logs containing only live
/// data, skipping replaced values and tombstones.
///
/// This implementation makes several significant simplifications over
/// standard BitCask:
///
/// - Instead of writing multiple fixed-size log files, it uses a single
///   append-only log file of arbitrary size. This increases the compaction
///   volume, since the entire log file must be rewritten on every compaction,
///   and can exceed the filesystem's file size limit, but ToyDB databases are
///   expected to be small.
///
/// - Compactions lock the database for reads and writes. This is ok since ToyDB
///   only compacts during node startup and files are expected to be small.
///
/// - Hint files are not used, the log itself is scanned when opened to
///   build the keydir. Hint files only omit values, and ToyDB values are
///   expected to be small, so the hint files would be nearly as large as
///   the compacted log files themselves.
///
/// - Log entries don't contain timestamps or checksums.
///
/// The structure of a log entry is:
///
/// - Key length as big-endian u32.
/// - Value length as big-endian i32, or -1 for tombstones.
/// - Key as raw bytes (max 2 GB).
/// - Value as raw bytes (max 2 GB).
pub struct BitCask {
    /// The active append-only log file.
    log: Log,
    /// Maps keys to a value position and length in the log file.
    keydir: KeyDir,
}

/// Maps keys to a value position and length in the log file.
type KeyDir = std::collections::BTreeMap<Vec<u8>, (u64, u32)>;

impl BitCask {
    /// Opens or creates a BitCask database in the given file.
    pub fn new(path: PathBuf) -> Result<Self> {
        log::info!("Opening database {}", path.display());
        let mut log = Log::new(path.clone())?;
        let keydir = log.build_keydir()?;
        log::info!("Indexed {} live keys in {}", keydir.len(), path.display());
        Ok(Self { log, keydir })
    }

    /// Opens a BitCask database, and automatically compacts it if the amount
    /// of garbage exceeds the given ratio and byte size when opened.
    ///
    /// TODO rename garbage_min_ratio to fraction throughout.
    pub fn new_compact(
        path: PathBuf,
        garbage_min_ratio: f64,
        garbage_min_bytes: u64,
    ) -> Result<Self> {
        let mut s = Self::new(path)?;

        let status = s.status()?;
        if Self::should_compact(
            status.garbage_disk_size,
            status.total_disk_size,
            garbage_min_ratio,
            garbage_min_bytes,
        ) {
            log::info!(
                "Compacting {} to remove {:.0}% garbage ({} MB out of {} MB)",
                s.log.path.display(),
                status.garbage_disk_size as f64 / status.total_disk_size as f64 * 100.0,
                status.garbage_disk_size / 1024 / 1024,
                status.total_disk_size / 1024 / 1024
            );
            s.compact()?;
            log::info!(
                "Compacted {} to size {} MB",
                s.log.path.display(),
                (status.total_disk_size - status.garbage_disk_size) / 1024 / 1024
            );
        }

        Ok(s)
    }

    /// Returns true if the log file should be compacted.
    fn should_compact(garbage_size: u64, total_size: u64, min_ratio: f64, min_bytes: u64) -> bool {
        let garbage_ratio = garbage_size as f64 / total_size as f64;
        garbage_size > 0 && garbage_size >= min_bytes && garbage_ratio >= min_ratio
    }
}

impl Engine for BitCask {
    type ScanIterator<'a> = ScanIterator<'a>;

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.log.write_entry(key, None)?;
        self.keydir.remove(key);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        // Don't fsync in tests, to speed them up.
        #[cfg(not(test))]
        self.log.file.sync_all()?;
        Ok(())
    }

    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some((value_pos, value_len)) = self.keydir.get(key) {
            Ok(Some(self.log.read_value(*value_pos, *value_len)?))
        } else {
            Ok(None)
        }
    }

    fn scan(&mut self, range: impl std::ops::RangeBounds<Vec<u8>>) -> Self::ScanIterator<'_> {
        ScanIterator { inner: self.keydir.range(range), log: &mut self.log }
    }

    fn scan_dyn(
        &mut self,
        range: (std::ops::Bound<Vec<u8>>, std::ops::Bound<Vec<u8>>),
    ) -> Box<dyn super::ScanIterator + '_> {
        Box::new(self.scan(range))
    }

    fn set(&mut self, key: &[u8], value: Vec<u8>) -> Result<()> {
        let (pos, len) = self.log.write_entry(key, Some(&*value))?;
        let value_len = value.len() as u32;
        self.keydir.insert(key.to_vec(), (pos + len as u64 - value_len as u64, value_len));
        Ok(())
    }

    fn status(&mut self) -> Result<Status> {
        let keys = self.keydir.len() as u64;
        let size = self
            .keydir
            .iter()
            .fold(0, |size, (key, (_, value_len))| size + key.len() as u64 + *value_len as u64);
        let total_disk_size = self.log.file.metadata()?.len();
        let live_disk_size = size + 8 * keys; // account for length prefixes
        let garbage_disk_size = total_disk_size - live_disk_size;
        Ok(Status {
            name: "bitcask".to_string(),
            keys,
            size,
            total_disk_size,
            live_disk_size,
            garbage_disk_size,
        })
    }
}

pub struct ScanIterator<'a> {
    inner: std::collections::btree_map::Range<'a, Vec<u8>, (u64, u32)>,
    log: &'a mut Log,
}

impl<'a> ScanIterator<'a> {
    fn map(&mut self, item: (&Vec<u8>, &(u64, u32))) -> <Self as Iterator>::Item {
        let (key, (value_pos, value_len)) = item;
        Ok((key.clone(), self.log.read_value(*value_pos, *value_len)?))
    }
}

impl<'a> Iterator for ScanIterator<'a> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|item| self.map(item))
    }
}

impl<'a> DoubleEndedIterator for ScanIterator<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back().map(|item| self.map(item))
    }
}

impl BitCask {
    /// Compacts the current log file by writing out a new log file containing
    /// only live keys and replacing the current file with it.
    pub fn compact(&mut self) -> Result<()> {
        let mut tmp_path = self.log.path.clone();
        tmp_path.set_extension("new");
        let (mut new_log, new_keydir) = self.write_log(tmp_path)?;

        std::fs::rename(&new_log.path, &self.log.path)?;
        new_log.path = self.log.path.clone();

        self.log = new_log;
        self.keydir = new_keydir;
        Ok(())
    }

    /// Writes out a new log file with the live entries of the current log file
    /// and returns it along with its keydir. Entries are written in key order.
    fn write_log(&mut self, path: PathBuf) -> Result<(Log, KeyDir)> {
        let mut new_keydir = KeyDir::new();
        let mut new_log = Log::new(path)?;
        new_log.file.set_len(0)?; // truncate file if it exists
        for (key, (value_pos, value_len)) in self.keydir.iter() {
            let value = self.log.read_value(*value_pos, *value_len)?;
            let (pos, len) = new_log.write_entry(key, Some(&value))?;
            new_keydir.insert(key.clone(), (pos + len as u64 - *value_len as u64, *value_len));
        }
        Ok((new_log, new_keydir))
    }
}

/// Attempt to flush the file when the database is closed.
impl Drop for BitCask {
    fn drop(&mut self) {
        if let Err(error) = self.flush() {
            log::error!("failed to flush file: {}", error)
        }
    }
}

/// A BitCask append-only log file, containing a sequence of key/value
/// entries encoded as follows;
///
/// - Key length as big-endian u32.
/// - Value length as big-endian i32, or -1 for tombstones.
/// - Key as raw bytes (max 2 GB).
/// - Value as raw bytes (max 2 GB).
struct Log {
    /// Path to the log file.
    path: PathBuf,
    /// The opened file containing the log.
    file: std::fs::File,
}

impl Log {
    /// Opens a log file, or creates one if it does not exist. Takes out an
    /// exclusive lock on the file until it is closed, or errors if the lock is
    /// already held.
    fn new(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        file.try_lock_exclusive()?;
        Ok(Self { path, file })
    }

    /// Builds a keydir by scanning the log file. If an incomplete entry is
    /// encountered, it is assumed to be caused by an incomplete write operation
    /// and the remainder of the file is truncated.
    fn build_keydir(&mut self) -> Result<KeyDir> {
        let mut len_buf = [0u8; 4];
        let mut keydir = KeyDir::new();
        let file_len = self.file.metadata()?.len();
        let mut r = BufReader::new(&mut self.file);
        let mut pos = r.seek(SeekFrom::Start(0))?;

        while pos < file_len {
            // Read the next entry from the file, returning the key, value
            // position, and value length or None for tombstones.
            let result = || -> std::result::Result<(Vec<u8>, u64, Option<u32>), std::io::Error> {
                r.read_exact(&mut len_buf)?;
                let key_len = u32::from_be_bytes(len_buf);
                r.read_exact(&mut len_buf)?;
                let value_len_or_tombstone = match i32::from_be_bytes(len_buf) {
                    l if l >= 0 => Some(l as u32),
                    _ => None, // -1 for tombstones
                };
                let value_pos = pos + 4 + 4 + key_len as u64;

                let mut key = vec![0; key_len as usize];
                r.read_exact(&mut key)?;

                if let Some(value_len) = value_len_or_tombstone {
                    if value_pos + value_len as u64 > file_len {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "value extends beyond end of file",
                        ));
                    }
                    r.seek_relative(value_len as i64)?; // avoids discarding buffer
                }

                Ok((key, value_pos, value_len_or_tombstone))
            }();

            match result {
                // Populate the keydir with the entry, or remove it on tombstones.
                Ok((key, value_pos, Some(value_len))) => {
                    keydir.insert(key, (value_pos, value_len));
                    pos = value_pos + value_len as u64;
                }
                Ok((key, value_pos, None)) => {
                    keydir.remove(&key);
                    pos = value_pos;
                }
                // If an incomplete entry was found at the end of the file, assume an
                // incomplete write and truncate the file.
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    log::error!("Found incomplete entry at offset {}, truncating file", pos);
                    self.file.set_len(pos)?;
                    break;
                }
                Err(err) => return Err(err.into()),
            }
        }

        Ok(keydir)
    }

    /// Reads a value from the log file.
    fn read_value(&mut self, value_pos: u64, value_len: u32) -> Result<Vec<u8>> {
        let mut value = vec![0; value_len as usize];
        self.file.seek(SeekFrom::Start(value_pos))?;
        self.file.read_exact(&mut value)?;
        Ok(value)
    }

    /// Appends a key/value entry to the log file, using a None value for
    /// tombstones. It returns the position and length of the entry.
    fn write_entry(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<(u64, u32)> {
        let key_len = key.len() as u32;
        let value_len = value.map_or(0, |v| v.len() as u32);
        let value_len_or_tombstone = value.map_or(-1, |v| v.len() as i32);
        let len = 4 + 4 + key_len + value_len;

        let pos = self.file.seek(SeekFrom::End(0))?;
        let mut w = BufWriter::with_capacity(len as usize, &mut self.file);
        w.write_all(&key_len.to_be_bytes())?;
        w.write_all(&value_len_or_tombstone.to_be_bytes())?;
        w.write_all(key)?;
        if let Some(value) = value {
            w.write_all(value)?;
        }
        w.flush()?;

        Ok((pos, len))
    }
}

#[cfg(test)]
mod tests {
    use super::super::engine::test::Runner;
    use super::*;
    use std::error::Error as StdError;
    use std::fmt::Write as _;
    use std::result::Result as StdResult;
    use test_case::test_case;
    use test_each_file::test_each_path;

    // Run common goldenscript tests in src/storage/testscripts/engine.
    test_each_path! { in "src/storage/testscripts/engine" as engine => test_goldenscript }

    // Also run BitCask-specific tests in src/storage/testscripts/bitcask.
    test_each_path! { in "src/storage/testscripts/bitcask" as scripts => test_goldenscript }

    fn test_goldenscript(path: &std::path::Path) {
        goldenscript::run(&mut BitCaskRunner::new(), path).expect("goldenscript failed")
    }

    /// Tests that exclusive locks are taken out on log files, erroring if held,
    /// and released when the database is closed.
    #[test]
    fn lock() -> Result<()> {
        let path = tempfile::TempDir::with_prefix("toydb")?.path().join("bitcask");
        let engine = BitCask::new(path.clone()).expect("bitcask failed");

        // Opening another database with the same file should error.
        assert!(BitCask::new(path.clone()).is_err());

        // Opening another database after the current is closed works.
        drop(engine);
        assert!(BitCask::new(path).is_ok());
        Ok(())
    }

    /// Tests that a log with an incomplete write at the end can be recovered by
    /// discarding the last entry.
    #[test]
    fn recovery() -> Result<()> {
        // Create an initial log file with a few entries. Keep track of where
        // each entry ends.
        let dir = tempfile::TempDir::with_prefix("toydb")?;
        let path = dir.path().join("complete");
        let mut log = Log::new(path.clone())?;

        let mut ends = vec![];
        let (pos, len) = log.write_entry("deleted".as_bytes(), Some(&[1, 2, 3]))?;
        ends.push(pos + len as u64);
        let (pos, len) = log.write_entry("deleted".as_bytes(), None)?;
        ends.push(pos + len as u64);
        let (pos, len) = log.write_entry(&[], Some(&[]))?;
        ends.push(pos + len as u64);
        let (pos, len) = log.write_entry("key".as_bytes(), Some(&[1, 2, 3, 4, 5]))?;
        ends.push(pos + len as u64);
        drop(log);

        // Copy the file, and truncate it at each byte, then try to open it
        // and assert that we always retain a prefix of entries.
        let truncpath = dir.path().join("truncated");
        let size = std::fs::metadata(&path)?.len();
        for pos in 0..=size {
            std::fs::copy(&path, &truncpath)?;
            let f = std::fs::OpenOptions::new().write(true).open(&truncpath)?;
            f.set_len(pos)?;
            drop(f);

            let mut expect = vec![];
            if pos >= ends[0] {
                expect.push((b"deleted".to_vec(), vec![1, 2, 3]))
            }
            if pos >= ends[1] {
                expect.pop(); // "deleted" key removed
            }
            if pos >= ends[2] {
                expect.push((b"".to_vec(), vec![]))
            }
            if pos >= ends[3] {
                expect.push((b"key".to_vec(), vec![1, 2, 3, 4, 5]))
            }

            let mut engine = BitCask::new(truncpath.clone())?;
            assert_eq!(expect, engine.scan(..).collect::<Result<Vec<_>>>()?);
        }
        Ok(())
    }

    /// Tests key/value sizes up to 64 MB.
    #[test]
    fn point_ops_sizes() -> Result<()> {
        let path = tempfile::TempDir::with_prefix("toydb")?.path().join("bitcask");
        let mut engine = BitCask::new(path.clone()).expect("bitcask failed");

        // Generate keys/values for increasing powers of two.
        for size in (1..=26).map(|i| 1 << i) {
            let value = vec![b'x'; size];
            let key = value.as_slice();

            assert_eq!(engine.get(key)?, None);
            engine.set(key, value.clone())?;
            assert_eq!(engine.get(key)?.as_ref(), Some(&value));
            engine.delete(key)?;
            assert_eq!(engine.get(key)?, None);
        }
        Ok(())
    }

    /// Tests that should_compact() handles parameters correctly.
    #[test_case(100, 100, -01.0, 0 => true; "ratio negative all garbage")]
    #[test_case(100, 100, 0.0, 0 => true; "ratio 0 all garbage")]
    #[test_case(100, 100, 1.0, 0 => true; "ratio 1 all garbage")]
    #[test_case(100, 100, 2.0, 0 => false; "ratio 2 all garbage")]
    #[test_case(0, 100, 0.0, 0 => false; "ratio 0 no garbage")]
    #[test_case(1, 100, 0.0, 0 => true; "ratio 0 tiny garbage")]
    #[test_case(49, 100, 0.5, 0 => false; "below ratio")]
    #[test_case(50, 100, 0.5, 0 => true; "at ratio")]
    #[test_case(51, 100, 0.5, 0 => true; "above ratio")]
    #[test_case(49, 100, 0.0, 50 => false; "below min bytes")]
    #[test_case(50, 100, 0.0, 50 => true; "at min bytes")]
    #[test_case(51, 100, 0.0, 50 => true; "above min bytes")]
    fn should_compact(garbage_size: u64, total_size: u64, min_ratio: f64, min_bytes: u64) -> bool {
        BitCask::should_compact(garbage_size, total_size, min_ratio, min_bytes)
    }

    /// A BitCask-specific goldenscript runner, which dispatches through to the
    /// standard Engine runner.
    struct BitCaskRunner {
        inner: Runner<BitCask>,
        tempdir: tempfile::TempDir,
    }

    impl goldenscript::Runner for BitCaskRunner {
        fn run(&mut self, command: &goldenscript::Command) -> StdResult<String, Box<dyn StdError>> {
            let mut output = String::new();
            match command.name.as_str() {
                // compact
                // Compacts the BitCask entry log.
                "compact" => {
                    command.consume_args().reject_rest()?;
                    self.inner.engine.compact()?;
                }

                // dump
                // Dumps the full BitCask entry log.
                "dump" => {
                    command.consume_args().reject_rest()?;
                    self.dump(&mut output)?;
                }

                // reopen [compact_fraction=FLOAT]
                // Closes and reopens the BitCask database. If compact_ratio is
                // given, it specifies a garbage ratio beyond which the log
                // should be auto-compacted on open.
                "reopen" => {
                    let mut args = command.consume_args();
                    let compact_fraction = args.lookup_parse("compact_fraction")?;
                    args.reject_rest()?;
                    // We need to close the file before we can reopen it, which
                    // happens when the database is dropped. Replace the engine
                    // with a temporary empty engine then reopen the file.
                    let path = self.inner.engine.log.path.clone();
                    self.inner.engine = BitCask::new(self.tempdir.path().join("empty"))?;
                    if let Some(garbage_ratio) = compact_fraction {
                        self.inner.engine = BitCask::new_compact(path, garbage_ratio, 0)?;
                    } else {
                        self.inner.engine = BitCask::new(path)?;
                    }
                }

                // Pass other commands to the standard engine runner.
                _ => return self.inner.run(command),
            }
            Ok(output)
        }
    }

    impl BitCaskRunner {
        fn new() -> Self {
            let tempdir = tempfile::TempDir::with_prefix("toydb").expect("tempdir failed");
            let engine = BitCask::new(tempdir.path().join("bitcask")).expect("bitcask failed");
            let inner = Runner::new(engine);
            Self { inner, tempdir }
        }

        /// Dumps the full BitCask entry log.
        fn dump(&mut self, output: &mut String) -> StdResult<(), Box<dyn StdError>> {
            let file = &mut self.inner.engine.log.file;
            let file_len = file.metadata()?.len();
            let mut r = BufReader::new(file);
            let mut pos = r.seek(SeekFrom::Start(0))?;
            let mut len_buf = [0; 4];
            let mut idx = 0;

            while pos < file_len {
                if idx > 0 {
                    writeln!(output, "--------")?;
                }
                write!(output, "{:<7}", format!("{idx}@{pos}"))?;

                r.read_exact(&mut len_buf)?;
                let key_len = u32::from_be_bytes(len_buf);
                write!(output, " keylen={key_len} [{}]", hex::encode(len_buf))?;

                r.read_exact(&mut len_buf)?;
                let value_len_or_tombstone = i32::from_be_bytes(len_buf); // NB: -1 for tombstones
                let value_len = value_len_or_tombstone.max(0) as u32;
                writeln!(output, " valuelen={value_len_or_tombstone} [{}]", hex::encode(len_buf))?;

                let mut key = vec![0; key_len as usize];
                r.read_exact(&mut key)?;
                let mut value = vec![0; value_len as usize];
                r.read_exact(&mut value)?;
                let size = 4 + 4 + key_len as u64 + value_len as u64;
                writeln!(
                    output,
                    "{:<7} key=\"{}\" [{}] {}",
                    format!("{size}b"),
                    Runner::<BitCask>::format_bytes(&key),
                    hex::encode(key),
                    match value_len_or_tombstone {
                        -1 => "tombstone".to_string(),
                        _ => format!(
                            "value=\"{}\" [{}]",
                            Runner::<BitCask>::format_bytes(&value),
                            hex::encode(&value)
                        ),
                    },
                )?;

                pos += size;
                idx += 1;
            }
            Ok(())
        }
    }
}

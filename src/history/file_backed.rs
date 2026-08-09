use super::{
    base::CommandLineSearch, History, HistoryItem, HistoryItemId, SearchDirection, SearchQuery,
};
use crate::{
    result::{ReedlineError, ReedlineErrorVariants},
    HistorySessionId, Result,
};

use std::{
    borrow::Cow,
    collections::VecDeque,
    fs::OpenOptions,
    io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write},
    ops::{Deref, DerefMut},
    path::PathBuf,
};

/// Default size of the [`FileBackedHistory`] used when calling [`FileBackedHistory::default()`]
pub const HISTORY_SIZE: usize = 1000;
pub const NEWLINE_ESCAPE: &str = "<\\n>";

/// Stateful history that allows up/down-arrow browsing with an internal cursor.
///
/// Can optionally be associated with a newline separated history file using the [`FileBackedHistory::with_file()`] constructor.
/// Similar to bash's behavior without HISTTIMEFORMAT.
/// (See <https://www.gnu.org/software/bash/manual/html_node/Bash-History-Facilities.html>)
/// If the history is associated to a file all new changes within a given history capacity will be written to disk when History is dropped.
#[derive(Debug)]
pub struct FileBackedHistory {
    capacity: usize,
    entries: VecDeque<String>,
    file: Option<PathBuf>,
    len_on_disk: usize, // Keep track what was previously written to disk
    session: Option<HistorySessionId>,
    /// How many lines of the history file the most recent [`FileBackedHistory::sync()`]
    /// had to recover lossily because they were not valid UTF-8.
    ///
    /// See [`FileBackedHistory::lossy_recoveries()`].
    lossy_recoveries: usize,
}

impl Default for FileBackedHistory {
    /// Creates an in-memory [`History`] with a maximal capacity of [`HISTORY_SIZE`].
    ///
    /// To create a [`History`] that is synchronized with a file use [`FileBackedHistory::with_file()`]
    ///
    /// # Panics
    ///
    /// If `HISTORY_SIZE == usize::MAX`
    fn default() -> Self {
        match Self::new(HISTORY_SIZE) {
            Ok(history) => history,
            Err(e) => panic!("{}", e),
        }
    }
}

fn encode_entry(s: &str) -> String {
    s.replace('\n', NEWLINE_ESCAPE)
}

fn decode_entry(s: &str) -> String {
    s.replace(NEWLINE_ESCAPE, "\n")
}

impl History for FileBackedHistory {
    /// only saves a value if it's different than the last value
    fn save(&mut self, h: HistoryItem) -> Result<HistoryItem> {
        let entry = h.command_line;
        // Don't append if the preceding value is identical or the string empty
        let entry_id =
            if (self.entries.back() != Some(&entry)) && !entry.is_empty() && self.capacity > 0 {
                if self.entries.len() == self.capacity {
                    // History is "full", so we delete the oldest entry first,
                    // before adding a new one.
                    self.entries.pop_front();
                    self.len_on_disk = self.len_on_disk.saturating_sub(1);
                }
                self.entries.push_back(entry.to_string());
                Some(HistoryItemId::new((self.entries.len() - 1) as i64))
            } else {
                None
            };
        Ok(FileBackedHistory::construct_entry(entry_id, entry))
    }

    fn load(&self, id: HistoryItemId) -> Result<super::HistoryItem> {
        Ok(FileBackedHistory::construct_entry(
            Some(id),
            self.entries
                .get(id.0 as usize)
                .ok_or(ReedlineError(ReedlineErrorVariants::OtherHistoryError(
                    "Item does not exist",
                )))?
                .clone(),
        ))
    }

    fn count(&self, query: SearchQuery) -> Result<i64> {
        // todo: this could be done cheaper
        Ok(self.search(query)?.len() as i64)
    }

    fn search(&self, query: SearchQuery) -> Result<Vec<HistoryItem>> {
        if query.start_time.is_some() || query.end_time.is_some() {
            return Err(ReedlineError(
                ReedlineErrorVariants::HistoryFeatureUnsupported {
                    history: "FileBackedHistory",
                    feature: "filtering by time",
                },
            ));
        }

        if query.filter.hostname.is_some()
            || query.filter.cwd_exact.is_some()
            || query.filter.cwd_prefix.is_some()
            || query.filter.exit_successful.is_some()
        {
            return Err(ReedlineError(
                ReedlineErrorVariants::HistoryFeatureUnsupported {
                    history: "FileBackedHistory",
                    feature: "filtering by extra info",
                },
            ));
        }
        let (min_id, max_id) = {
            let start = query.start_id.map(|e| e.0);
            let end = query.end_id.map(|e| e.0);
            if let SearchDirection::Backward = query.direction {
                (end, start)
            } else {
                (start, end)
            }
        };
        // add one to make it inclusive
        let min_id = min_id.map(|e| e + 1).unwrap_or(0);
        // subtract one to make it inclusive
        let max_id = max_id
            .map(|e| e - 1)
            .unwrap_or(self.entries.len() as i64 - 1);
        if max_id < 0 || min_id > self.entries.len() as i64 - 1 {
            return Ok(vec![]);
        }
        let intrinsic_limit = max_id - min_id + 1;
        let limit = if let Some(given_limit) = query.limit {
            std::cmp::min(intrinsic_limit, given_limit) as usize
        } else {
            intrinsic_limit as usize
        };
        let filter = |(idx, cmd): (usize, &String)| {
            if !match &query.filter.command_line {
                Some(CommandLineSearch::Prefix(p)) => cmd.starts_with(p),
                Some(CommandLineSearch::Substring(p)) => cmd.contains(p),
                Some(CommandLineSearch::Exact(p)) => cmd == p,
                None => true,
            } {
                return None;
            }
            if let Some(str) = &query.filter.not_command_line {
                if cmd == str {
                    return None;
                }
            }
            Some(FileBackedHistory::construct_entry(
                Some(HistoryItemId::new(idx as i64)),
                cmd.to_string(), // todo: this copy might be a perf bottleneck
            ))
        };

        let iter = self
            .entries
            .iter()
            .enumerate()
            .skip(min_id as usize)
            .take(intrinsic_limit as usize);
        if let SearchDirection::Backward = query.direction {
            Ok(iter.rev().filter_map(filter).take(limit).collect())
        } else {
            Ok(iter.filter_map(filter).take(limit).collect())
        }
    }

    fn update(
        &mut self,
        _id: super::HistoryItemId,
        _updater: &dyn Fn(super::HistoryItem) -> super::HistoryItem,
    ) -> Result<()> {
        Err(ReedlineError(
            ReedlineErrorVariants::HistoryFeatureUnsupported {
                history: "FileBackedHistory",
                feature: "updating entries",
            },
        ))
    }

    fn clear(&mut self) -> Result<()> {
        self.entries.clear();
        self.len_on_disk = 0;

        if let Some(file) = &self.file {
            if let Err(err) = std::fs::remove_file(file) {
                return Err(ReedlineError(ReedlineErrorVariants::IOError(err)));
            }
        }

        Ok(())
    }

    fn delete(&mut self, _h: super::HistoryItemId) -> Result<()> {
        Err(ReedlineError(
            ReedlineErrorVariants::HistoryFeatureUnsupported {
                history: "FileBackedHistory",
                feature: "removing entries",
            },
        ))
    }

    /// Writes unwritten history contents to disk.
    ///
    /// If file would exceed `capacity` truncates the oldest entries.
    fn sync(&mut self) -> std::io::Result<()> {
        if let Some(fname) = &self.file {
            // The unwritten entries
            let own_entries = self.entries.range(self.len_on_disk..);

            if let Some(base_dir) = fname.parent() {
                std::fs::create_dir_all(base_dir)?;
            }

            let mut f_lock = fd_lock::RwLock::new(
                OpenOptions::new()
                    .create(true)
                    .write(true)
                    .read(true)
                    .truncate(false)
                    .open(fname)?,
            );
            let mut writer_guard = f_lock.write()?;
            let (mut foreign_entries, truncate, lossy_recoveries) = {
                let mut reader = BufReader::new(writer_guard.deref());
                // Read byte-oriented rather than with `BufRead::lines()`.
                //
                // `lines()` yields `Err(InvalidData)` for any line that is not valid
                // UTF-8, and collecting into `io::Result<_>` short-circuits on the
                // first such error — so a single stray byte anywhere in the file used
                // to abort the entire sync. Because `Drop` discards sync's error, that
                // failure was silent and *permanent*: every command of every later
                // session was dropped on the floor. One bad byte must cost one record,
                // never the corpus, so undecodable bytes are recovered lossily
                // (U+FFFD) and merely counted.
                let mut from_file: VecDeque<String> = VecDeque::new();
                let mut lossy_recoveries: usize = 0;
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    buf.clear();
                    if reader.read_until(b'\n', &mut buf)? == 0 {
                        break;
                    }
                    // Strip the line terminator, matching `lines()`, which drops a
                    // trailing `\n` and the `\r` of a `\r\n` pair.
                    if buf.last() == Some(&b'\n') {
                        buf.pop();
                        if buf.last() == Some(&b'\r') {
                            buf.pop();
                        }
                    }
                    let line = String::from_utf8_lossy(&buf);
                    // `from_utf8_lossy` borrows iff the input was already valid UTF-8,
                    // so an owned `Cow` is exactly "this line needed recovery".
                    if matches!(line, Cow::Owned(_)) {
                        lossy_recoveries += 1;
                    }
                    from_file.push_back(decode_entry(&line));
                }
                if from_file.len() + own_entries.len() > self.capacity {
                    (
                        from_file.split_off(
                            from_file.len() - (self.capacity.saturating_sub(own_entries.len())),
                        ),
                        true,
                        lossy_recoveries,
                    )
                } else {
                    (from_file, false, lossy_recoveries)
                }
            };
            self.lossy_recoveries = lossy_recoveries;

            {
                let mut writer = BufWriter::new(writer_guard.deref_mut());
                if truncate {
                    writer.rewind()?;

                    for line in &foreign_entries {
                        writer.write_all(encode_entry(line).as_bytes())?;
                        writer.write_all("\n".as_bytes())?;
                    }
                } else {
                    writer.seek(SeekFrom::End(0))?;
                }
                for line in own_entries {
                    writer.write_all(encode_entry(line).as_bytes())?;
                    writer.write_all("\n".as_bytes())?;
                }
                writer.flush()?;
            }
            if truncate {
                let file = writer_guard.deref_mut();
                let file_len = file.stream_position()?;
                file.set_len(file_len)?;
            }

            let own_entries = self.entries.drain(self.len_on_disk..);
            foreign_entries.extend(own_entries);
            self.entries = foreign_entries;

            self.len_on_disk = self.entries.len();
        }
        Ok(())
    }

    fn session(&self) -> Option<HistorySessionId> {
        self.session
    }
}

impl FileBackedHistory {
    /// Creates a new in-memory history that remembers `n <= capacity` elements
    ///
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == usize::MAX {
            return Err(ReedlineError(ReedlineErrorVariants::OtherHistoryError(
                "History capacity too large to be addressed safely",
            )));
        }

        Ok(FileBackedHistory {
            capacity,
            entries: VecDeque::new(),
            file: None,
            len_on_disk: 0,
            session: None,
            lossy_recoveries: 0,
        })
    }

    /// Number of history-file lines the most recent [`History::sync()`] had to
    /// recover lossily because they were not valid UTF-8.
    ///
    /// Such lines are kept, with the undecodable bytes replaced by U+FFFD, rather
    /// than aborting the read: one corrupt byte costs one record, never the whole
    /// file. A non-zero value means the history file on disk holds bytes that are
    /// not valid UTF-8 — the entries in memory have already been repaired, and the
    /// file itself is rewritten the next time the capacity limit truncates it.
    ///
    /// Callers that want to surface this (a warning, a metric) can read it after
    /// any sync; reedline itself has no logging facility to report it through.
    pub fn lossy_recoveries(&self) -> usize {
        self.lossy_recoveries
    }

    /// Creates a new history with an associated history file.
    ///
    /// History file format: commands separated by new lines.
    /// If file exists file will be read otherwise empty file will be created.
    ///
    ///
    /// **Side effects:** creates all nested directories to the file
    ///
    pub fn with_file(capacity: usize, file: PathBuf) -> Result<Self> {
        let mut hist = Self::new(capacity)?;
        if let Some(base_dir) = file.parent() {
            std::fs::create_dir_all(base_dir)?;
        }
        hist.file = Some(file);
        hist.sync()?;
        Ok(hist)
    }

    // this history doesn't store any info except command line
    fn construct_entry(id: Option<HistoryItemId>, command_line: String) -> HistoryItem {
        HistoryItem {
            id,
            start_timestamp: None,
            command_line,
            session_id: None,
            hostname: None,
            cwd: None,
            duration: None,
            exit_status: None,
            more_info: None,
        }
    }
}

impl Drop for FileBackedHistory {
    /// On drop the content of the [`History`] will be written to the file if specified via [`FileBackedHistory::with_file()`].
    fn drop(&mut self) {
        let _res = self.sync();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `contents` verbatim, bypassing any UTF-8 validation.
    fn write_raw(path: &std::path::Path, contents: &[u8]) {
        std::fs::write(path, contents).unwrap();
    }

    fn read_lines_lossy(path: &std::path::Path) -> Vec<String> {
        let bytes = std::fs::read(path).unwrap();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn save(hist: &mut FileBackedHistory, cmd: &str) {
        hist.save(FileBackedHistory::construct_entry(None, cmd.to_owned()))
            .unwrap();
    }

    /// A history file whose *first* line holds an invalid UTF-8 byte must not cost
    /// the corpus: the valid entries around it survive, and newly saved entries
    /// still reach disk.
    ///
    /// Against the pre-fix code `with_file` returns `Err(InvalidData)` here (and,
    /// in a real session, `Drop` swallows that error and silently discards every
    /// command of every subsequent session).
    #[test]
    fn sync_survives_invalid_utf8_on_the_first_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.txt");

        let mut raw = Vec::new();
        raw.extend_from_slice(b"echo caf\xba"); // lone 0xba: not valid UTF-8
        raw.push(b'\n');
        raw.extend_from_slice(b"echo good-one\n");
        raw.extend_from_slice(b"echo good-two\n");
        write_raw(&path, &raw);

        let mut hist = FileBackedHistory::with_file(10, path.clone()).unwrap();
        assert_eq!(
            hist.lossy_recoveries(),
            1,
            "exactly the one undecodable line should have needed recovery"
        );
        assert_eq!(
            hist.entries.len(),
            3,
            "the bad byte must cost one record's fidelity, not the other records"
        );
        assert_eq!(hist.entries[0], "echo caf\u{fffd}");
        assert_eq!(hist.entries[1], "echo good-one");
        assert_eq!(hist.entries[2], "echo good-two");

        save(&mut hist, "echo brand-new");
        hist.sync().unwrap();
        drop(hist);

        let lines = read_lines_lossy(&path);
        assert_eq!(
            lines,
            vec![
                "echo caf\u{fffd}",
                "echo good-one",
                "echo good-two",
                "echo brand-new",
            ],
            "the pre-existing entries must survive and the new entry must land"
        );
    }

    /// The capacity/truncate path — rewind, rewrite, `set_len` — must keep working
    /// when the entries being truncated away were themselves lossily recovered.
    #[test]
    fn sync_truncates_correctly_after_lossy_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.txt");

        let mut raw = Vec::new();
        raw.extend_from_slice(b"echo bad\xba");
        raw.push(b'\n');
        raw.extend_from_slice(b"echo one\n");
        raw.extend_from_slice(b"echo two\n");
        raw.extend_from_slice(b"echo three\n");
        write_raw(&path, &raw);

        // capacity 3 over 4 on-disk entries: opening truncates the oldest (the
        // corrupt one) and rewrites the file.
        let mut hist = FileBackedHistory::with_file(3, path.clone()).unwrap();
        assert_eq!(hist.lossy_recoveries(), 1);
        assert_eq!(
            read_lines_lossy(&path),
            vec!["echo one", "echo two", "echo three"],
            "truncation must rewrite and shorten the file, leaving no tail"
        );

        save(&mut hist, "echo four");
        hist.sync().unwrap();
        drop(hist);

        assert_eq!(
            read_lines_lossy(&path),
            vec!["echo two", "echo three", "echo four"]
        );
    }

    /// Multi-line entries are stored escaped; lossy recovery must not disturb the
    /// decode of neighbouring lines.
    #[test]
    fn lossy_recovery_preserves_newline_escaping() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.txt");

        let mut raw = Vec::new();
        raw.extend_from_slice(b"\xff\xfe\n");
        raw.extend_from_slice(b"echo one<\\n>echo two\n");
        write_raw(&path, &raw);

        let hist = FileBackedHistory::with_file(10, path).unwrap();
        assert_eq!(hist.lossy_recoveries(), 1);
        assert_eq!(hist.entries[1], "echo one\necho two");
    }

    /// A clean file must report zero recoveries.
    #[test]
    fn valid_history_file_reports_no_lossy_recoveries() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.txt");
        write_raw(&path, "echo one\necho two\n".as_bytes());

        let hist = FileBackedHistory::with_file(10, path).unwrap();
        assert_eq!(hist.lossy_recoveries(), 0);
        assert_eq!(hist.entries.len(), 2);
    }
}

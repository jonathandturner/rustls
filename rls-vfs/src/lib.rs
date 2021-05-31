#![warn(rust_2018_idioms)]

extern crate rls_span as span;
#[macro_use]
extern crate log;

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::Read;
use std::marker::PhantomData;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread::{self, Thread};

#[cfg(test)]
mod test;

macro_rules! try_opt_loc {
    ($e:expr) => {
        match $e {
            Some(e) => e,
            None => return Err(Error::BadLocation),
        }
    };
}

pub struct Vfs<U = ()>(VfsInternal<RealFileLoader, U>);

/// Span of the text to be replaced defined in col/row terms.
#[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct SpanData {
    /// Span of the text defined in col/row terms.
    pub span: span::Span<span::ZeroIndexed>,
    /// Length in chars of the text. If present,
    /// used to calculate replacement range instead of
    /// span's row_end/col_end fields. Needed for editors that
    /// can't properly calculate the latter fields.
    /// Span's row_start/col_start are still assumed valid.
    pub len: Option<u64>,
}

/// Span of text that VFS can operate with.
#[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum VfsSpan {
    /// Span with offsets based on unicode scalar values.
    UnicodeScalarValue(SpanData),
    /// Span with offsets based on UTF-16 code units.
    Utf16CodeUnit(SpanData),
}

#[allow(clippy::len_without_is_empty)]
impl VfsSpan {
    pub fn from_usv(span: span::Span<span::ZeroIndexed>, len: Option<u64>) -> VfsSpan {
        VfsSpan::UnicodeScalarValue(SpanData { span, len })
    }

    pub fn from_utf16(span: span::Span<span::ZeroIndexed>, len: Option<u64>) -> VfsSpan {
        VfsSpan::Utf16CodeUnit(SpanData { span, len })
    }

    /// Return a UTF-8 byte offset in `s` for a given text unit offset.
    pub fn byte_in_str(&self, s: &str, c: span::Column<span::ZeroIndexed>) -> Result<usize, Error> {
        match self {
            VfsSpan::UnicodeScalarValue(..) => byte_in_str(s, c),
            VfsSpan::Utf16CodeUnit(..) => byte_in_str_utf16(s, c),
        }
    }

    fn as_inner(&self) -> &SpanData {
        match self {
            VfsSpan::UnicodeScalarValue(span) => span,
            VfsSpan::Utf16CodeUnit(span) => span,
        }
    }

    pub fn span(&self) -> &span::Span<span::ZeroIndexed> {
        &self.as_inner().span
    }

    pub fn len(&self) -> Option<u64> {
        self.as_inner().len
    }
}

#[derive(Debug)]
pub enum Change {
    /// Create an in-memory image of the file.
    AddFile { file: PathBuf, text: String },
    /// Changes in-memory contents of the previously added file.
    ReplaceText {
        /// Span of the text to be replaced.
        span: VfsSpan,
        /// Text to replace specified text range with.
        text: String,
    },
}

impl Change {
    fn file(&self) -> &Path {
        match *self {
            Change::AddFile { ref file, .. } => file.as_ref(),
            Change::ReplaceText { ref span, .. } => span.span().file.as_ref(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Error {
    /// The given file has become out of sync with the filesystem.
    OutOfSync(PathBuf),
    /// IO error reading or writing the given path, 2nd arg is a message.
    Io(Option<PathBuf>, Option<String>),
    /// There are changes to the given file which have not been written to disk.
    UncommittedChanges(PathBuf),
    /// Client specified a location that is not within a file. I.e., a row or
    /// column not in the file.
    BadLocation,
    /// The requested file was not cached in the VFS.
    FileNotCached,
    /// Not really an error, file is cached but there is no user data for it.
    NoUserDataForFile,
    /// Wrong kind of file.
    BadFileKind,
    /// An internal error - a bug in the VFS.
    InternalError(&'static str),
}

impl Error {
    fn description(&self) -> &str {
        match *self {
            Error::OutOfSync(ref _path_buf) => "file out of sync with filesystem",
            Error::Io(ref _path_buf, ref _message) => "io::Error reading or writing path",
            Error::UncommittedChanges(ref _path_buf) => {
                "changes exist which have not been written to disk"
            }
            Error::BadLocation => "client specified location not existing within a file",
            Error::FileNotCached => "requested file was not cached in the VFS",
            Error::NoUserDataForFile => "file is cached but there is no user data for it",
            Error::BadFileKind => {
                "file is not the correct kind for the operation (e.g., text op on binary file)"
            }
            Error::InternalError(_) => "internal error",
        }
    }
}

impl Into<String> for Error {
    fn into(self) -> String {
        self.description().to_owned()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::OutOfSync(ref path_buf) => {
                write!(f, "file {} out of sync with filesystem", path_buf.display())
            }
            Error::UncommittedChanges(ref path_buf) => {
                write!(f, "{} has uncommitted changes", path_buf.display())
            }
            Error::InternalError(e) => write!(f, "internal error: {}", e),
            Error::BadLocation
            | Error::FileNotCached
            | Error::NoUserDataForFile
            | Error::Io(..)
            | Error::BadFileKind => f.write_str(self.description()),
        }
    }
}

impl<U> Default for Vfs<U> {
    fn default() -> Self {
        Self::new()
    }
}

impl<U> Vfs<U> {
    /// Creates a new, empty VFS.
    pub fn new() -> Vfs<U> {
        Vfs(VfsInternal::<RealFileLoader, U>::new())
    }

    /// Indicate that the current file as known to the VFS has been written to
    /// disk.
    pub fn file_saved(&self, path: &Path) -> Result<(), Error> {
        self.0.file_saved(path)
    }

    /// Removes a file from the VFS. Does not check if the file is synced with
    /// the disk. Does not check if the file exists.
    pub fn flush_file(&self, path: &Path) -> Result<(), Error> {
        self.0.flush_file(path)
    }

    pub fn file_is_synced(&self, path: &Path) -> Result<bool, Error> {
        self.0.file_is_synced(path)
    }

    /// Record a set of changes to the VFS.
    pub fn on_changes(&self, changes: &[Change]) -> Result<(), Error> {
        self.0.on_changes(changes)
    }

    /// Return all files in the VFS.
    pub fn get_cached_files(&self) -> HashMap<PathBuf, String> {
        self.0.get_cached_files()
    }

    pub fn get_changes(&self) -> HashMap<PathBuf, String> {
        self.0.get_changes()
    }

    /// Returns true if the VFS contains any changed files.
    pub fn has_changes(&self) -> bool {
        self.0.has_changes()
    }

    pub fn set_file(&self, path: &Path, text: &str) {
        self.0.set_file(path, text)
    }

    pub fn load_file(&self, path: &Path) -> Result<FileContents, Error> {
        self.0.load_file(path)
    }

    pub fn load_line(
        &self,
        path: &Path,
        line: span::Row<span::ZeroIndexed>,
    ) -> Result<String, Error> {
        self.0.load_line(path, line)
    }

    pub fn load_lines(
        &self,
        path: &Path,
        line_start: span::Row<span::ZeroIndexed>,
        line_end: span::Row<span::ZeroIndexed>,
    ) -> Result<String, Error> {
        self.0.load_lines(path, line_start, line_end)
    }

    pub fn load_span(&self, span: span::Span<span::ZeroIndexed>) -> Result<String, Error> {
        self.0.load_span(span)
    }

    pub fn for_each_line<F>(&self, path: &Path, f: F) -> Result<(), Error>
    where
        F: FnMut(&str, usize) -> Result<(), Error>,
    {
        self.0.for_each_line(path, f)
    }

    pub fn write_file(&self, path: &Path) -> Result<(), Error> {
        self.0.write_file(path)
    }

    pub fn set_user_data(&self, path: &Path, data: Option<U>) -> Result<(), Error> {
        self.0.set_user_data(path, data)
    }

    // If f returns NoUserDataForFile, then the user data for the given file is erased.
    pub fn with_user_data<F, R>(&self, path: &Path, f: F) -> Result<R, Error>
    where
        F: FnOnce(Result<(Option<&str>, &mut U), Error>) -> Result<R, Error>,
    {
        self.0.with_user_data(path, f)
    }

    // If f returns NoUserDataForFile, then the user data for the given file is erased.
    pub fn ensure_user_data<F>(&self, path: &Path, f: F) -> Result<(), Error>
    where
        F: FnOnce(Option<&str>) -> Result<U, Error>,
    {
        self.0.ensure_user_data(path, f)
    }

    pub fn clear(&self) {
        self.0.clear()
    }
}

// Important invariants! If you are going to lock both files and pending_files,
// you must lock pending_files first.
// You must have both locks to insert or remove files.
struct VfsInternal<T, U> {
    files: Mutex<HashMap<PathBuf, File<U>>>,
    pending_files: Mutex<HashMap<PathBuf, Vec<Thread>>>,
    loader: PhantomData<T>,
}

impl<T: FileLoader, U> VfsInternal<T, U> {
    fn new() -> VfsInternal<T, U> {
        VfsInternal {
            files: Mutex::new(HashMap::new()),
            pending_files: Mutex::new(HashMap::new()),
            loader: PhantomData,
        }
    }

    fn clear(&self) {
        let mut pending_files = self.pending_files.lock().unwrap();
        let mut files = self.files.lock().unwrap();
        *files = HashMap::new();
        let mut new_pending_files = HashMap::new();
        mem::swap(&mut *pending_files, &mut new_pending_files);
        for ts in new_pending_files.values() {
            for t in ts {
                t.unpark();
            }
        }
    }

    fn file_saved(&self, path: &Path) -> Result<(), Error> {
        let mut files = self.files.lock().unwrap();
        if let Some(ref mut f) = files.get_mut(path) {
            match f.kind {
                FileKind::Text(ref mut f) => f.changed = false,
                FileKind::Binary(_) => return Err(Error::BadFileKind),
            }
        }
        Ok(())
    }

    fn flush_file(&self, path: &Path) -> Result<(), Error> {
        loop {
            let mut pending_files = self.pending_files.lock().unwrap();
            let mut files = self.files.lock().unwrap();
            if !pending_files.contains_key(path) {
                files.remove(path);
                return Ok(());
            }

            pending_files.get_mut(path).unwrap().push(thread::current());
            thread::park();
        }
    }

    fn file_is_synced(&self, path: &Path) -> Result<bool, Error> {
        let files = self.files.lock().unwrap();
        match files.get(path) {
            Some(f) => Ok(!f.changed()),
            None => Err(Error::FileNotCached),
        }
    }

    fn on_changes(&self, changes: &[Change]) -> Result<(), Error> {
        trace!("on_changes: {:?}", changes);
        for (file_name, changes) in coalesce_changes(changes) {
            let path = Path::new(file_name);
            {
                let mut files = self.files.lock().unwrap();
                if let Some(file) = files.get_mut(Path::new(path)) {
                    file.make_change(&changes)?;
                    continue;
                }
            }

            // FIXME(#11): if the first change is `Add`, we should avoid
            // loading the file. If the first change is not `Add`, then
            // this is subtly broken, because we can't guarantee that the
            // edits are intended to be applied to the version of the file
            // we read from disk. That is, the on disk contents might have
            // changed after the edit request.
            let mut file = T::read(Path::new(path))?;
            file.make_change(&changes)?;

            let mut files = self.files.lock().unwrap();
            files.insert(path.to_path_buf(), file);
        }

        Ok(())
    }

    fn set_file(&self, path: &Path, text: &str) {
        let file = File {
            kind: FileKind::Text(TextFile {
                text: text.to_owned(),
                line_indices: make_line_indices(text),
                changed: true,
            }),
            user_data: None,
        };

        loop {
            let mut pending_files = self.pending_files.lock().unwrap();
            let mut files = self.files.lock().unwrap();
            if !pending_files.contains_key(path) {
                files.insert(path.to_owned(), file);
                return;
            }

            pending_files.get_mut(path).unwrap().push(thread::current());
            thread::park();
        }
    }

    fn get_cached_files(&self) -> HashMap<PathBuf, String> {
        let files = self.files.lock().unwrap();
        files
            .iter()
            .filter_map(|(p, f)| match f.kind {
                FileKind::Text(ref f) => Some((p.clone(), f.text.clone())),
                FileKind::Binary(_) => None,
            })
            .collect()
    }

    fn get_changes(&self) -> HashMap<PathBuf, String> {
        let files = self.files.lock().unwrap();
        files
            .iter()
            .filter_map(|(p, f)| match f.kind {
                FileKind::Text(ref f) if f.changed => Some((p.clone(), f.text.clone())),
                _ => None,
            })
            .collect()
    }

    fn has_changes(&self) -> bool {
        let files = self.files.lock().unwrap();
        files.values().any(|f| f.changed())
    }

    fn load_line(&self, path: &Path, line: span::Row<span::ZeroIndexed>) -> Result<String, Error> {
        self.ensure_file(path, |f| f.load_line(line).map(|s| s.to_owned()))
    }

    fn load_lines(
        &self,
        path: &Path,
        line_start: span::Row<span::ZeroIndexed>,
        line_end: span::Row<span::ZeroIndexed>,
    ) -> Result<String, Error> {
        self.ensure_file(path, |f| f.load_lines(line_start, line_end).map(|s| s.to_owned()))
    }

    fn load_span(&self, span: span::Span<span::ZeroIndexed>) -> Result<String, Error> {
        self.ensure_file(&span.file, |f| f.load_range(span.range).map(|s| s.to_owned()))
    }

    fn for_each_line<F>(&self, path: &Path, f: F) -> Result<(), Error>
    where
        F: FnMut(&str, usize) -> Result<(), Error>,
    {
        self.ensure_file(path, |file| file.for_each_line(f))
    }

    fn load_file(&self, path: &Path) -> Result<FileContents, Error> {
        self.ensure_file(path, |f| Ok(f.contents()))
    }

    fn ensure_file<F, R>(&self, path: &Path, f: F) -> Result<R, Error>
    where
        F: FnOnce(&File<U>) -> Result<R, Error>,
    {
        loop {
            {
                let mut pending_files = self.pending_files.lock().unwrap();
                let files = self.files.lock().unwrap();
                if files.contains_key(path) {
                    return f(&files[path]);
                }
                if !pending_files.contains_key(path) {
                    pending_files.insert(path.to_owned(), vec![]);
                    break;
                }
                pending_files.get_mut(path).unwrap().push(thread::current());
            }
            thread::park();
        }

        // We should not hold the locks while we read from disk.
        let file = T::read(path);

        // Need to re-get the locks here.
        let mut pending_files = self.pending_files.lock().unwrap();
        let mut files = self.files.lock().unwrap();
        match file {
            Ok(file) => {
                files.insert(path.to_owned(), file);
                let ts = pending_files.remove(path).unwrap();
                for t in ts {
                    t.unpark();
                }
            }
            Err(e) => {
                let ts = pending_files.remove(path).unwrap();
                for t in ts {
                    t.unpark();
                }
                return Err(e);
            }
        }

        f(&files[path])
    }

    fn write_file(&self, path: &Path) -> Result<(), Error> {
        let file = {
            let mut files = self.files.lock().unwrap();
            match files.get_mut(path) {
                Some(f) => {
                    if let FileKind::Text(ref mut f) = f.kind {
                        f.changed = false;
                    }
                    f.kind.clone()
                }
                None => return Err(Error::FileNotCached),
            }
        };

        T::write(path, &file)?;
        Ok(())
    }

    pub fn set_user_data(&self, path: &Path, data: Option<U>) -> Result<(), Error> {
        let mut files = self.files.lock().unwrap();
        match files.get_mut(path) {
            Some(ref mut f) => {
                f.user_data = data;
                Ok(())
            }
            None => Err(Error::FileNotCached),
        }
    }

    // Note that f should not be a long-running operation since we hold the lock
    // to the VFS while it runs.
    pub fn with_user_data<F, R>(&self, path: &Path, f: F) -> Result<R, Error>
    where
        F: FnOnce(Result<(Option<&str>, &mut U), Error>) -> Result<R, Error>,
    {
        let mut files = self.files.lock().unwrap();
        let file = match files.get_mut(path) {
            Some(f) => f,
            None => return f(Err(Error::FileNotCached)),
        };

        let result = f(match file.user_data {
            Some(ref mut u) => {
                let text = match file.kind {
                    FileKind::Text(ref f) => Some(&f.text as &str),
                    FileKind::Binary(_) => None,
                };
                Ok((text, u))
            }
            None => Err(Error::NoUserDataForFile),
        });

        if let Err(Error::NoUserDataForFile) = result {
            file.user_data = None;
        }

        result
    }

    pub fn ensure_user_data<F>(&self, path: &Path, f: F) -> Result<(), Error>
    where
        F: FnOnce(Option<&str>) -> Result<U, Error>,
    {
        let mut files = self.files.lock().unwrap();
        match files.get_mut(path) {
            Some(ref mut file) => {
                if file.user_data.is_none() {
                    let text = match file.kind {
                        FileKind::Text(ref f) => Some(&f.text as &str),
                        FileKind::Binary(_) => None,
                    };
                    match f(text) {
                        Ok(u) => {
                            file.user_data = Some(u);
                            Ok(())
                        }
                        Err(Error::NoUserDataForFile) => {
                            file.user_data = None;
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Ok(())
                }
            }
            None => Err(Error::FileNotCached),
        }
    }
}

fn coalesce_changes<'a>(changes: &'a [Change]) -> HashMap<&'a Path, Vec<&'a Change>> {
    // Note that for any given file, we preserve the order of the changes.
    let mut result = HashMap::new();
    for c in changes {
        result.entry(&*c.file()).or_insert_with(Vec::new).push(c);
    }
    result
}

fn make_line_indices(text: &str) -> Vec<u32> {
    let mut result = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == 0xA {
            result.push((i + 1) as u32);
        }
    }
    result.push(text.len() as u32);
    result
}

#[derive(Clone)]
enum FileKind {
    Text(TextFile),
    Binary(Vec<u8>),
}

impl FileKind {
    fn as_bytes(&self) -> &[u8] {
        match *self {
            FileKind::Text(ref t) => t.text.as_bytes(),
            FileKind::Binary(ref b) => b,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum FileContents {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Clone)]
struct TextFile {
    // FIXME(https://github.com/jonathandturner/rustls/issues/21) should use a rope.
    text: String,
    line_indices: Vec<u32>,
    changed: bool,
}

struct File<U> {
    kind: FileKind,
    user_data: Option<U>,
}

impl<U> File<U> {
    fn contents(&self) -> FileContents {
        match self.kind {
            FileKind::Text(ref t) => FileContents::Text(t.text.clone()),
            FileKind::Binary(ref b) => FileContents::Binary(b.clone()),
        }
    }

    fn make_change(&mut self, changes: &[&Change]) -> Result<(), Error> {
        match self.kind {
            FileKind::Text(ref mut t) => {
                self.user_data = None;
                t.make_change(changes)
            }
            FileKind::Binary(_) => Err(Error::BadFileKind),
        }
    }

    fn load_line(&self, line: span::Row<span::ZeroIndexed>) -> Result<&str, Error> {
        match self.kind {
            FileKind::Text(ref t) => t.load_line(line),
            FileKind::Binary(_) => Err(Error::BadFileKind),
        }
    }

    fn load_lines(
        &self,
        line_start: span::Row<span::ZeroIndexed>,
        line_end: span::Row<span::ZeroIndexed>,
    ) -> Result<&str, Error> {
        match self.kind {
            FileKind::Text(ref t) => t.load_lines(line_start, line_end),
            FileKind::Binary(_) => Err(Error::BadFileKind),
        }
    }

    fn load_range(&self, range: span::Range<span::ZeroIndexed>) -> Result<&str, Error> {
        match self.kind {
            FileKind::Text(ref t) => t.load_range(range),
            FileKind::Binary(_) => Err(Error::BadFileKind),
        }
    }

    fn for_each_line<F>(&self, f: F) -> Result<(), Error>
    where
        F: FnMut(&str, usize) -> Result<(), Error>,
    {
        match self.kind {
            FileKind::Text(ref t) => t.for_each_line(f),
            FileKind::Binary(_) => Err(Error::BadFileKind),
        }
    }

    fn changed(&self) -> bool {
        match self.kind {
            FileKind::Text(ref t) => t.changed,
            FileKind::Binary(_) => false,
        }
    }
}

impl TextFile {
    fn make_change(&mut self, changes: &[&Change]) -> Result<(), Error> {
        trace!("TextFile::make_change");
        for c in changes {
            trace!("TextFile::make_change: {:?}", c);
            let new_text = match **c {
                Change::ReplaceText { span: ref vfs_span, ref text } => {
                    let (span, len) = (vfs_span.span(), vfs_span.len());

                    let range = {
                        let first_line = self.load_line(span.range.row_start)?;
                        let byte_start = self.line_indices[span.range.row_start.0 as usize]
                            + vfs_span.byte_in_str(first_line, span.range.col_start)? as u32;

                        let byte_end = if let Some(len) = len {
                            // if `len` exists, the replaced portion of text
                            // is `len` chars starting from row_start/col_start.
                            byte_start
                                + vfs_span.byte_in_str(
                                    &self.text[byte_start as usize..],
                                    span::Column::new_zero_indexed(len as u32),
                                )? as u32
                        } else {
                            // if no `len`, fall back to using row_end/col_end
                            // for determining the tail end of replaced text.
                            let last_line = self.load_line(span.range.row_end)?;
                            self.line_indices[span.range.row_end.0 as usize]
                                + vfs_span.byte_in_str(last_line, span.range.col_end)? as u32
                        };

                        (byte_start, byte_end)
                    };
                    let mut new_text = self.text[..range.0 as usize].to_owned();
                    new_text.push_str(text);
                    new_text.push_str(&self.text[range.1 as usize..]);
                    new_text
                }
                Change::AddFile { ref text, .. } => text.to_owned(),
            };

            self.text = new_text;
            self.line_indices = make_line_indices(&self.text);
        }

        self.changed = true;
        Ok(())
    }

    fn load_line(&self, line: span::Row<span::ZeroIndexed>) -> Result<&str, Error> {
        self.load_lines(line, line)
    }

    fn load_lines(
        &self,
        line_start: span::Row<span::ZeroIndexed>,
        line_end: span::Row<span::ZeroIndexed>,
    ) -> Result<&str, Error> {
        let line_start = line_start.0 as usize;
        let mut line_end = line_end.0 as usize + 1;
        if line_end >= self.line_indices.len() {
            line_end = self.line_indices.len() - 1;
        }

        let start = (*try_opt_loc!(self.line_indices.get(line_start))) as usize;
        let end = (*try_opt_loc!(self.line_indices.get(line_end))) as usize;

        if (end) <= self.text.len() && start <= end {
            Ok(&self.text[start..end])
        } else {
            Err(Error::BadLocation)
        }
    }

    fn load_range(&self, range: span::Range<span::ZeroIndexed>) -> Result<&str, Error> {
        let line_start = range.row_start.0 as usize;
        let mut line_end = range.row_end.0 as usize;
        if line_end >= self.line_indices.len() {
            line_end = self.line_indices.len() - 1;
        }

        let start = (*try_opt_loc!(self.line_indices.get(line_start))) as usize;
        let start = start + range.col_start.0 as usize;
        let end = (*try_opt_loc!(self.line_indices.get(line_end))) as usize;
        let end = end + range.col_end.0 as usize + 1;

        if (end) <= self.text.len() && start <= end {
            Ok(&self.text[start..end])
        } else {
            Err(Error::BadLocation)
        }
    }

    fn for_each_line<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(&str, usize) -> Result<(), Error>,
    {
        let mut line_iter = self.line_indices.iter();
        let mut start = *line_iter.next().unwrap() as usize;
        for (i, idx) in line_iter.enumerate() {
            let idx = *idx as usize;
            f(&self.text[start..idx], i)?;
            start = idx;
        }

        Ok(())
    }
}

/// Return a UTF-8 byte offset in `s` for a given UTF-8 unicode scalar value offset.
fn byte_in_str(s: &str, c: span::Column<span::ZeroIndexed>) -> Result<usize, Error> {
    // We simulate a null-terminated string here because spans are exclusive at
    // the top, and so that index might be outside the length of the string.
    for (i, (b, _)) in s.char_indices().chain(Some((s.len(), '\0')).into_iter()).enumerate() {
        if c.0 as usize == i {
            return Ok(b);
        }
    }

    Err(Error::InternalError("Out of bounds access in `byte_in_str`"))
}

/// Return a UTF-8 byte offset in `s` for a given UTF-16 code unit offset.
fn byte_in_str_utf16(s: &str, c: span::Column<span::ZeroIndexed>) -> Result<usize, Error> {
    let (mut utf8_offset, mut utf16_offset) = (0, 0);
    let target_utf16_offset = c.0 as usize;

    for chr in s.chars().chain(std::iter::once('\0')) {
        if utf16_offset > target_utf16_offset {
            break;
        } else if utf16_offset == target_utf16_offset {
            return Ok(utf8_offset);
        }

        utf8_offset += chr.len_utf8();
        utf16_offset += chr.len_utf16();
    }

    Err(Error::InternalError("UTF-16 code unit offset is not at `str` char boundary"))
}

trait FileLoader {
    fn read<U>(file_name: &Path) -> Result<File<U>, Error>;
    fn write(file_name: &Path, file: &FileKind) -> Result<(), Error>;
}

struct RealFileLoader;

impl FileLoader for RealFileLoader {
    fn read<U>(file_name: &Path) -> Result<File<U>, Error> {
        let mut file = match fs::File::open(file_name) {
            Ok(f) => f,
            Err(_) => {
                return Err(Error::Io(
                    Some(file_name.to_owned()),
                    Some(format!("Could not open file: {}", file_name.display())),
                ));
            }
        };
        let mut buf = vec![];
        if file.read_to_end(&mut buf).is_err() {
            return Err(Error::Io(
                Some(file_name.to_owned()),
                Some(format!("Could not read file: {}", file_name.display())),
            ));
        }

        match String::from_utf8(buf) {
            Ok(s) => Ok(File {
                kind: FileKind::Text(TextFile {
                    line_indices: make_line_indices(&s),
                    text: s,
                    changed: false,
                }),
                user_data: None,
            }),
            Err(e) => Ok(File { kind: FileKind::Binary(e.into_bytes()), user_data: None }),
        }
    }

    fn write(file_name: &Path, file: &FileKind) -> Result<(), Error> {
        use std::io::Write;

        macro_rules! try_io {
            ($e:expr) => {
                match $e {
                    Ok(e) => e,
                    Err(e) => {
                        return Err(Error::Io(Some(file_name.to_owned()), Some(e.to_string())));
                    }
                }
            };
        }

        let mut out = try_io!(::std::fs::File::create(file_name));
        try_io!(out.write_all(file.as_bytes()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use span::Column;

    #[test]
    fn byte_in_str_utf16() {
        use super::byte_in_str_utf16;

        assert_eq!(
            '😢'.len_utf8(),
            byte_in_str_utf16("😢a", Column::new_zero_indexed('😢'.len_utf16() as u32)).unwrap()
        );

        // 😢 is represented by 2 u16s - we can't index in the middle of a character
        assert!(byte_in_str_utf16("😢", Column::new_zero_indexed(1)).is_err());
    }
}

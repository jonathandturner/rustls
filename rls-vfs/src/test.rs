use std::path::{Path, PathBuf};

use span::{self, Column, Position, Row};

use super::{
    make_line_indices, Change, Error, File, FileContents, FileKind, FileLoader, TextFile,
    VfsInternal, VfsSpan,
};

type Span = span::Span<span::ZeroIndexed>;

struct MockFileLoader;

impl FileLoader for MockFileLoader {
    fn read<U>(file_name: &Path) -> Result<File<U>, Error> {
        let text = format!("{}\nHello\nWorld\nHello, World!\n", file_name.display());
        let text_file = TextFile { line_indices: make_line_indices(&text), text, changed: false };
        Ok(File { kind: FileKind::Text(text_file), user_data: None })
    }

    fn write(file_name: &Path, file: &FileKind) -> Result<(), Error> {
        if let FileKind::Text(ref text_file) = *file {
            if file_name.display().to_string() == "foo" {
                // TODO: is this test useful still?
                assert_eq!(text_file.changed, false);
                assert_eq!(text_file.text, "foo\nHfooo\nWorld\nHello, World!\n");
            }
        }
        Ok(())
    }
}

fn make_change(with_len: bool) -> Change {
    let (row_end, col_end, len) = if with_len {
        // If len is present, we shouldn't depend on row_end/col_end
        // at all, because they may be invalid.
        (0, 0, Some(3))
    } else {
        (1, 4, None)
    };
    Change::ReplaceText {
        span: VfsSpan::from_usv(
            Span::new(
                Row::new_zero_indexed(1),
                Row::new_zero_indexed(row_end),
                Column::new_zero_indexed(1),
                Column::new_zero_indexed(col_end),
                "foo",
            ),
            len,
        ),
        text: "foo".to_owned(),
    }
}

fn make_change_2(with_len: bool) -> Change {
    let (row_end, col_end, len) = if with_len {
        // If len is present, we shouldn't depend on row_end/col_end
        // at all, because they may be invalid.
        (0, 0, Some(4))
    } else {
        (3, 2, None)
    };
    Change::ReplaceText {
        span: VfsSpan::from_usv(
            Span::new(
                Row::new_zero_indexed(2),
                Row::new_zero_indexed(row_end),
                Column::new_zero_indexed(4),
                Column::new_zero_indexed(col_end),
                "foo",
            ),
            len,
        ),
        text: "aye carumba".to_owned(),
    }
}

fn test_has_changes(with_len: bool) {
    let vfs = VfsInternal::<MockFileLoader, ()>::new();

    assert!(!vfs.has_changes());
    vfs.load_file(&Path::new("foo")).unwrap();
    assert!(!vfs.has_changes());
    vfs.on_changes(&[make_change(with_len)]).unwrap();
    assert!(vfs.has_changes());
    vfs.file_saved(&Path::new("bar")).unwrap();
    assert!(vfs.has_changes());
    vfs.file_saved(&Path::new("foo")).unwrap();
    assert!(!vfs.has_changes());
}

#[test]
fn test_has_changes_without_len() {
    test_has_changes(false)
}

#[test]
fn test_has_changes_with_len() {
    test_has_changes(true)
}

#[test]
fn test_cached_files() {
    let vfs = VfsInternal::<MockFileLoader, ()>::new();
    assert!(vfs.get_cached_files().is_empty());
    vfs.load_file(&Path::new("foo")).unwrap();
    vfs.load_file(&Path::new("bar")).unwrap();
    let files = vfs.get_cached_files();
    assert!(files.len() == 2);
    assert!(files[Path::new("foo")] == "foo\nHello\nWorld\nHello, World!\n");
    assert!(files[Path::new("bar")] == "bar\nHello\nWorld\nHello, World!\n");
}

#[test]
fn test_flush_file() {
    let vfs = VfsInternal::<MockFileLoader, ()>::new();
    // Flushing an uncached-file should succeed.
    vfs.flush_file(&Path::new("foo")).unwrap();
    vfs.load_file(&Path::new("foo")).unwrap();
    vfs.flush_file(&Path::new("foo")).unwrap();
    assert!(vfs.get_cached_files().is_empty());
}

fn test_changes(with_len: bool) {
    let vfs = VfsInternal::<MockFileLoader, ()>::new();

    vfs.on_changes(&[make_change(with_len)]).unwrap();
    let files = vfs.get_cached_files();
    assert!(files.len() == 1);
    assert_eq!(files[&PathBuf::from("foo")], "foo\nHfooo\nWorld\nHello, World!\n");
    assert_eq!(
        vfs.load_file(&Path::new("foo")).unwrap(),
        FileContents::Text("foo\nHfooo\nWorld\nHello, World!\n".to_owned()),
    );
    assert_eq!(
        vfs.load_file(&Path::new("bar")).unwrap(),
        FileContents::Text("bar\nHello\nWorld\nHello, World!\n".to_owned()),
    );

    vfs.on_changes(&[make_change_2(with_len)]).unwrap();
    let files = vfs.get_cached_files();
    assert!(files.len() == 2);
    assert_eq!(files[&PathBuf::from("foo")], "foo\nHfooo\nWorlaye carumballo, World!\n");
    assert_eq!(
        vfs.load_file(&Path::new("foo")).unwrap(),
        FileContents::Text("foo\nHfooo\nWorlaye carumballo, World!\n".to_owned()),
    );
}

#[test]
fn test_changes_without_len() {
    test_changes(false)
}

#[test]
fn test_changes_with_len() {
    test_changes(true)
}

#[test]
fn test_change_add_file() {
    let vfs = VfsInternal::<MockFileLoader, ()>::new();
    let new_file = Change::AddFile { file: PathBuf::from("foo"), text: "Hello, World!".to_owned() };
    vfs.on_changes(&[new_file]).unwrap();

    let files = vfs.get_cached_files();
    assert_eq!(files.len(), 1);
    assert_eq!(files[&PathBuf::from("foo")], "Hello, World!");
}

fn test_user_data(with_len: bool) {
    let vfs = VfsInternal::<MockFileLoader, i32>::new();

    // New files have no user data.
    vfs.load_file(&Path::new("foo")).unwrap();
    vfs.with_user_data(&Path::new("foo"), |u| {
        assert_eq!(u, Err(Error::NoUserDataForFile));
        Ok(())
    })
    .unwrap();

    // Set and read data.
    vfs.set_user_data(&Path::new("foo"), Some(42)).unwrap();
    vfs.with_user_data(&Path::new("foo"), |u| {
        assert_eq!(*u.unwrap().1, 42);
        Ok(())
    })
    .unwrap();
    assert_eq!(vfs.set_user_data(&Path::new("bar"), Some(42)), Err(Error::FileNotCached));

    // ensure_user_data should not be called if the userdata already exists.
    vfs.ensure_user_data(&Path::new("foo"), |_| panic!()).unwrap();

    // Test ensure_user_data is called.
    vfs.load_file(&Path::new("bar")).unwrap();
    vfs.ensure_user_data(&Path::new("bar"), |_| Ok(1)).unwrap();
    vfs.with_user_data(&Path::new("bar"), |u| {
        assert_eq!(*u.unwrap().1, 1);
        Ok(())
    })
    .unwrap();

    // compute and read data.
    vfs.with_user_data(&Path::new("foo"), |u| {
        assert_eq!(u.as_ref().unwrap().0, Some("foo\nHello\nWorld\nHello, World!\n"));
        *u.unwrap().1 = 43;
        Ok(())
    })
    .unwrap();
    vfs.with_user_data(&Path::new("foo"), |u| {
        assert_eq!(*u.unwrap().1, 43);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        vfs.with_user_data(&Path::new("foo"), |u| {
            assert_eq!(*u.unwrap().1, 43);
            Result::Err::<(), Error>(Error::BadLocation)
        }),
        Err(Error::BadLocation)
    );
    vfs.with_user_data(&Path::new("foo"), |u| {
        assert_eq!(*u.unwrap().1, 43);
        Ok(())
    })
    .unwrap();

    // Clear and read data.
    vfs.set_user_data(&Path::new("foo"), None).unwrap();
    vfs.with_user_data(&Path::new("foo"), |u| {
        assert_eq!(u, Err(Error::NoUserDataForFile));
        Ok(())
    })
    .unwrap();

    // Compute (clear) and read data.
    vfs.set_user_data(&Path::new("foo"), Some(42)).unwrap();
    assert_eq!(
        vfs.with_user_data(&Path::new("foo"), |_| Result::Err::<(), Error>(
            Error::NoUserDataForFile
        )),
        Err(Error::NoUserDataForFile)
    );
    vfs.with_user_data(&Path::new("foo"), |u| {
        assert_eq!(u, Err(Error::NoUserDataForFile));
        Ok(())
    })
    .unwrap();

    // Flushing a file should clear user data.
    vfs.set_user_data(&Path::new("foo"), Some(42)).unwrap();
    vfs.flush_file(&Path::new("foo")).unwrap();
    vfs.load_file(&Path::new("foo")).unwrap();
    vfs.with_user_data(&Path::new("foo"), |u| {
        assert_eq!(u, Err(Error::NoUserDataForFile));
        Ok(())
    })
    .unwrap();

    // Recording a change should clear user data.
    vfs.set_user_data(&Path::new("foo"), Some(42)).unwrap();
    vfs.on_changes(&[make_change(with_len)]).unwrap();
    vfs.with_user_data(&Path::new("foo"), |u| {
        assert_eq!(u, Err(Error::NoUserDataForFile));
        Ok(())
    })
    .unwrap();
}

#[test]
fn test_user_data_without_len() {
    test_user_data(false)
}

#[test]
fn test_user_data_with_len() {
    test_user_data(true)
}

fn test_write(with_len: bool) {
    let vfs = VfsInternal::<MockFileLoader, ()>::new();

    vfs.on_changes(&[make_change(with_len)]).unwrap();
    vfs.write_file(&Path::new("foo")).unwrap();
    let files = vfs.get_cached_files();
    assert!(files.len() == 1);
    let files = vfs.get_changes();
    assert!(files.is_empty());
}

#[test]
fn test_write_without_len() {
    test_write(false)
}

#[test]
fn test_write_with_len() {
    test_write(true)
}

#[test]
fn test_clear() {
    let vfs = VfsInternal::<MockFileLoader, ()>::new();
    vfs.load_file(&Path::new("foo")).unwrap();
    vfs.load_file(&Path::new("bar")).unwrap();
    assert!(vfs.get_cached_files().len() == 2);
    vfs.clear();
    assert!(vfs.get_cached_files().is_empty());
}

#[test]
fn test_wide_utf8() {
    let vfs = VfsInternal::<MockFileLoader, ()>::new();
    let changes = [
        Change::AddFile { file: PathBuf::from("foo"), text: String::from("😢") },
        Change::ReplaceText {
            span: VfsSpan::from_usv(
                Span::from_positions(
                    Position::new(Row::new_zero_indexed(0), Column::new_zero_indexed(0)),
                    Position::new(Row::new_zero_indexed(0), Column::new_zero_indexed(1)),
                    "foo",
                ),
                Some(1),
            ),
            text: "".into(),
        },
    ];

    vfs.on_changes(&changes).unwrap();

    assert_eq!(vfs.load_file(&Path::new("foo")).unwrap(), FileContents::Text("".to_owned()),);
}

#[test]
fn test_wide_utf16() {
    let vfs = VfsInternal::<MockFileLoader, ()>::new();
    let changes = [
        Change::AddFile { file: PathBuf::from("foo"), text: String::from("😢") },
        Change::ReplaceText {
            span: VfsSpan::from_utf16(
                Span::from_positions(
                    Position::new(Row::new_zero_indexed(0), Column::new_zero_indexed(0)),
                    Position::new(Row::new_zero_indexed(0), Column::new_zero_indexed(2)),
                    "foo",
                ),
                Some(2),
            ),
            text: "".into(),
        },
    ];

    vfs.on_changes(&changes).unwrap();

    assert_eq!(vfs.load_file(&Path::new("foo")).unwrap(), FileContents::Text("".to_owned()),);
}

#[test]
fn test_load_span() {
    let vfs = VfsInternal::<MockFileLoader, ()>::new();
    let changes = [Change::AddFile {
        file: PathBuf::from("foo"),
        text: String::from("hello\nfrom\nthe\nother\nside"),
    }];

    vfs.on_changes(&changes).unwrap();

    assert_eq!(
        vfs.load_span(Span::from_positions(
            Position::new(Row::new_zero_indexed(0), Column::new_zero_indexed(0)),
            Position::new(Row::new_zero_indexed(4), Column::new_zero_indexed(3)),
            "foo",
        ),)
            .unwrap(),
        "hello\nfrom\nthe\nother\nside"
    );

    assert_eq!(
        vfs.load_span(Span::from_positions(
            Position::new(Row::new_zero_indexed(0), Column::new_zero_indexed(2)),
            Position::new(Row::new_zero_indexed(4), Column::new_zero_indexed(2)),
            "foo",
        ),)
            .unwrap(),
        "llo\nfrom\nthe\nother\nsid"
    );

    assert_eq!(
        vfs.load_span(Span::from_positions(
            Position::new(Row::new_zero_indexed(2), Column::new_zero_indexed(1)),
            Position::new(Row::new_zero_indexed(2), Column::new_zero_indexed(2)),
            "foo",
        ),)
            .unwrap(),
        "he"
    );
}

#[test]
fn test_load_lines() {
    let vfs = VfsInternal::<MockFileLoader, ()>::new();
    let changes = [Change::AddFile {
        file: PathBuf::from("foo"),
        text: String::from("hello\nfrom\nthe\nother\nside"),
    }];

    vfs.on_changes(&changes).unwrap();

    assert_eq!(
        vfs.load_lines(&PathBuf::from("foo"), Row::new_zero_indexed(0), Row::new_zero_indexed(0))
            .unwrap(),
        "hello\n"
    );
    assert_eq!(
        vfs.load_lines(&PathBuf::from("foo"), Row::new_zero_indexed(0), Row::new_zero_indexed(4))
            .unwrap(),
        "hello\nfrom\nthe\nother\nside"
    );
    assert_eq!(
        vfs.load_lines(&PathBuf::from("foo"), Row::new_zero_indexed(2), Row::new_zero_indexed(4))
            .unwrap(),
        "the\nother\nside"
    );
}

#[test]
fn test_load_line() {
    let vfs = VfsInternal::<MockFileLoader, ()>::new();
    let changes = [Change::AddFile {
        file: PathBuf::from("foo"),
        text: String::from("hello\nfrom\nthe\nother\nside"),
    }];

    vfs.on_changes(&changes).unwrap();

    assert_eq!(vfs.load_line(&PathBuf::from("foo"), Row::new_zero_indexed(0)).unwrap(), "hello\n");
    assert_eq!(vfs.load_line(&PathBuf::from("foo"), Row::new_zero_indexed(2)).unwrap(), "the\n");
    assert_eq!(vfs.load_line(&PathBuf::from("foo"), Row::new_zero_indexed(4)).unwrap(), "side");
}

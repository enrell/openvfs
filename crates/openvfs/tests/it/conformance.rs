//! The behavioral contract of `FileSystem`, checked against every backend
//! configuration in `common::fixtures`.
//!
//! Each check is an executable statement of the semantics documented on the
//! `FileSystem` trait. If a backend passes this suite (and the differential
//! suite), code written against `Vfs` behaves identically on it.

use crate::common;

use common::fixtures::for_each_fixture;

macro_rules! conformance {
    ($($fixture:ident),* $(,)?) => {
        $( mod $fixture { checks!($fixture:
            // --- file I/O
            write_then_read, empty_file, binary_roundtrip_all_byte_values, large_file_roundtrip,
            write_truncates, append_mode, append_creates_missing_file, seek_and_overwrite,
            seek_past_end_then_write_zero_fills, seek_before_start_fails, set_len_truncates_and_extends,
            read_only_handle_rejects_writes, write_only_handle_rejects_reads, create_new_semantics,
            open_missing_without_create_fails, handles_share_contents, metadata_reports_kind_and_len,
            modification_time_is_tracked, invalid_open_options_are_rejected,
            // --- directories
            nested_dirs_and_sorted_listing, create_dir_all_is_idempotent, create_dir_twice_fails,
            create_dir_over_file_fails, create_dir_needs_parent, create_dir_all_through_file_fails,
            paths_through_files_are_not_directories, read_dir_errors, remove_file_and_dir,
            remove_non_empty_dir_fails, remove_dir_all_is_recursive, remove_dir_all_on_file_fails,
            wrong_kind_errors, root_is_protected,
            // --- rename
            rename_file, rename_replaces_existing_file, rename_moves_subtree, rename_onto_itself_is_noop,
            rename_refuses_dir_into_itself, rename_refuses_dir_target, rename_dir_over_file_fails,
            rename_missing_source_fails, rename_missing_destination_parent_fails, rename_then_recreate_source,
            // --- copy
            copy_file_and_tree, copy_overwrites_existing_file, copy_onto_itself_is_rejected_without_data_loss,
            copy_directory_errors, copy_tree_into_itself_is_rejected,
            // --- atomic writes
            write_atomic_replaces_and_leaves_no_temp_files, write_atomic_errors,
            // --- walking
            walk_is_sorted_preorder, walk_max_depth_and_filter,
            // --- paths and errors
            dotdot_cannot_escape, unicode_and_unusual_names, errors_carry_the_requested_path,
            exists_and_try_exists,
        ); } )*
    };
}

macro_rules! checks {
    ($fixture:ident: $($check:ident),* $(,)?) => {
        $( #[test] fn $check() {
            // The watchdog turns a deadlock into a failure instead of a hung run.
            crate::common::within(std::time::Duration::from_secs(120), || {
                let fixture = crate::common::fixtures::$fixture(&Default::default());
                crate::conformance::checks::$check(&openvfs::Vfs::from_arc(fixture.fs.clone()));
            });
        } )*
    };
}

for_each_fixture!(conformance);

#[allow(clippy::wildcard_imports)]
mod checks {
    use std::io::{Read, Seek, SeekFrom, Write};

    use openvfs::prelude::*;

    use crate::common::names;

    /// Asserts an error variant and that it reports `path`.
    macro_rules! assert_err {
        ($result:expr, $variant:ident, $path:expr) => {{
            match $result {
                Err(VfsError::$variant(p)) => assert_eq!(p, $path, "wrong path in {}", stringify!($variant)),
                other => panic!("expected {}({}), got {other:?}", stringify!($variant), $path),
            }
        }};
        ($result:expr, $variant:ident) => {{
            match $result {
                Err(VfsError::$variant(..)) => {}
                other => panic!("expected {}, got {other:?}", stringify!($variant)),
            }
        }};
    }

    fn fs(v: &Vfs) -> &dyn FileSystem {
        &**v.backend()
    }

    // --- file I/O --------------------------------------------------------------

    pub fn write_then_read(v: &Vfs) {
        v.write("/hello.txt", "olá, mundo").unwrap();
        assert_eq!(v.read_to_string("/hello.txt").unwrap(), "olá, mundo");
        assert!(v.is_file("/hello.txt"));
        assert!(!v.is_dir("/hello.txt"));
    }

    pub fn empty_file(v: &Vfs) {
        v.write("/empty", "").unwrap();
        assert_eq!(v.read("/empty").unwrap(), b"");
        assert_eq!(v.metadata("/empty").unwrap().len, 0);
    }

    pub fn binary_roundtrip_all_byte_values(v: &Vfs) {
        let data: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        v.write("/bin", &data).unwrap();
        assert_eq!(v.read("/bin").unwrap(), data);
    }

    pub fn large_file_roundtrip(v: &Vfs) {
        // Larger than any internal buffer (copy uses 8 KiB, compare uses 64 KiB).
        let data: Vec<u8> = (0..3_000_017u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13).to_le_bytes()[0]).collect();
        v.write("/large", &data).unwrap();
        assert_eq!(v.metadata("/large").unwrap().len, data.len() as u64);
        assert_eq!(v.copy("/large", "/large-copy").unwrap(), data.len() as u64);
        assert!(v.read("/large-copy").unwrap() == data, "copied contents differ");
    }

    pub fn write_truncates(v: &Vfs) {
        v.write("/f", "a much longer text").unwrap();
        v.write("/f", "short").unwrap();
        assert_eq!(v.read_to_string("/f").unwrap(), "short");
    }

    pub fn append_mode(v: &Vfs) {
        v.write("/log", "a").unwrap();
        v.append("/log", "b").unwrap();
        v.append("/log", "c").unwrap();
        assert_eq!(v.read_to_string("/log").unwrap(), "abc");
        // Appends go to the end even after seeking elsewhere.
        let mut f = v.open("/log", OpenOptions::new().append(true)).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(b"d").unwrap();
        drop(f);
        assert_eq!(v.read_to_string("/log").unwrap(), "abcd");
    }

    pub fn append_creates_missing_file(v: &Vfs) {
        v.append("/fresh", "created").unwrap();
        assert_eq!(v.read_to_string("/fresh").unwrap(), "created");
    }

    pub fn seek_and_overwrite(v: &Vfs) {
        v.write("/f", "hello world").unwrap();
        let mut f = v.open("/f", OpenOptions::new().read(true).write(true)).unwrap();
        f.seek(SeekFrom::Start(6)).unwrap();
        f.write_all(b"there").unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello there");
        assert_eq!(f.seek(SeekFrom::End(-5)).unwrap(), 6);
        assert_eq!(f.seek(SeekFrom::Current(2)).unwrap(), 8);
    }

    pub fn seek_past_end_then_write_zero_fills(v: &Vfs) {
        v.write("/sparse", "ab").unwrap();
        let mut f = v.open("/sparse", OpenOptions::new().write(true)).unwrap();
        f.seek(SeekFrom::Start(5)).unwrap();
        f.write_all(b"z").unwrap();
        drop(f);
        assert_eq!(v.read("/sparse").unwrap(), b"ab\0\0\0z");
    }

    pub fn seek_before_start_fails(v: &Vfs) {
        v.write("/f", "abc").unwrap();
        let mut f = v.open_read("/f").unwrap();
        assert!(f.seek(SeekFrom::Current(-1)).is_err());
        assert!(f.seek(SeekFrom::End(-4)).is_err());
    }

    pub fn set_len_truncates_and_extends(v: &Vfs) {
        v.write("/f", "abcdef").unwrap();
        v.set_len("/f", 3).unwrap();
        assert_eq!(v.read("/f").unwrap(), b"abc");
        v.set_len("/f", 5).unwrap();
        assert_eq!(v.read("/f").unwrap(), b"abc\0\0");
        let mut f = v.open("/f", OpenOptions::new().write(true)).unwrap();
        f.set_len(0).unwrap();
        f.sync_all().unwrap();
        drop(f);
        assert_eq!(v.metadata("/f").unwrap().len, 0);
        assert!(v.set_len("/missing", 1).is_err());
    }

    pub fn read_only_handle_rejects_writes(v: &Vfs) {
        v.write("/f", "keep").unwrap();
        let mut f = v.open_read("/f").unwrap();
        assert!(f.write_all(b"x").is_err());
        assert!(f.set_len(0).is_err());
        drop(f);
        assert_eq!(v.read_to_string("/f").unwrap(), "keep");
    }

    pub fn write_only_handle_rejects_reads(v: &Vfs) {
        v.write("/f", "data").unwrap();
        let mut f = v.open("/f", OpenOptions::new().write(true)).unwrap();
        let mut buf = [0u8; 4];
        assert!(f.read(&mut buf).is_err());
    }

    pub fn create_new_semantics(v: &Vfs) {
        let opts = OpenOptions::new().write(true).create_new(true);
        v.open("/g", opts).unwrap().write_all(b"new").unwrap();
        assert_eq!(v.read_to_string("/g").unwrap(), "new");
        assert_err!(v.open("/g", opts), AlreadyExists, "/g");
        assert_eq!(v.read_to_string("/g").unwrap(), "new", "a failed create_new must not truncate");
    }

    pub fn open_missing_without_create_fails(v: &Vfs) {
        assert_err!(v.open_read("/nope"), NotFound, "/nope");
        assert_err!(v.open("/nope", OpenOptions::new().write(true)), NotFound, "/nope");
        assert!(!v.exists("/nope"), "a failed open must not create the file");
    }

    pub fn handles_share_contents(v: &Vfs) {
        let mut writer = v.create("/shared").unwrap();
        writer.write_all(b"visible").unwrap();
        writer.flush().unwrap();
        assert_eq!(v.read_to_string("/shared").unwrap(), "visible");
        writer.write_all(b" more").unwrap();
        writer.flush().unwrap();
        assert_eq!(v.read_to_string("/shared").unwrap(), "visible more");
    }

    pub fn metadata_reports_kind_and_len(v: &Vfs) {
        v.write("/f", [7u8; 1234]).unwrap();
        v.create_dir("/d").unwrap();
        let file = v.metadata("/f").unwrap();
        assert!(file.is_file() && !file.is_dir());
        assert_eq!(file.len, 1234);
        assert!(!file.read_only);
        let dir = v.metadata("/d").unwrap();
        assert!(dir.is_dir() && !dir.is_file());
        assert_eq!(dir.len, 0);
        assert!(v.metadata("/").unwrap().is_dir());
    }

    pub fn modification_time_is_tracked(v: &Vfs) {
        v.write("/f", "1").unwrap();
        let first = v.metadata("/f").unwrap().modified.expect("bundled backends track mtime");
        std::thread::sleep(std::time::Duration::from_millis(15));
        v.write("/f", "22").unwrap();
        let second = v.metadata("/f").unwrap().modified.unwrap();
        assert!(second >= first, "mtime went backwards");
    }

    pub fn invalid_open_options_are_rejected(v: &Vfs) {
        v.write("/f", "x").unwrap();
        for bad in [
            OpenOptions::new(),
            OpenOptions::new().read(true).create(true),
            OpenOptions::new().read(true).truncate(true),
            OpenOptions::new().append(true).truncate(true),
        ] {
            assert_err!(v.open("/f", bad), InvalidInput);
        }
        assert_eq!(v.read_to_string("/f").unwrap(), "x");
    }

    // --- directories -------------------------------------------------------------

    pub fn nested_dirs_and_sorted_listing(v: &Vfs) {
        v.create_dir_all("/a/b/c").unwrap();
        v.write("/a/zeta.txt", "").unwrap();
        v.write("/a/alpha.txt", "").unwrap();
        let listing: Vec<_> = v.read_dir("/a").unwrap().into_iter().map(|e| (e.name, e.file_type, e.path)).collect();
        assert_eq!(
            listing,
            [
                ("alpha.txt".into(), FileType::File, VPath::new("/a/alpha.txt").unwrap()),
                ("b".into(), FileType::Dir, VPath::new("/a/b").unwrap()),
                ("zeta.txt".into(), FileType::File, VPath::new("/a/zeta.txt").unwrap()),
            ]
        );
        assert_eq!(names(fs(v), "/a/b"), ["c"]);
        assert!(v.read_dir("/a/b/c").unwrap().is_empty());
    }

    pub fn create_dir_all_is_idempotent(v: &Vfs) {
        v.create_dir_all("/x/y/z").unwrap();
        v.create_dir_all("/x/y/z").unwrap();
        v.create_dir_all("/x").unwrap();
        v.create_dir_all("/").unwrap();
        assert!(v.is_dir("/x/y/z"));
    }

    pub fn create_dir_twice_fails(v: &Vfs) {
        v.create_dir("/d").unwrap();
        assert_err!(v.create_dir("/d"), AlreadyExists, "/d");
    }

    pub fn create_dir_over_file_fails(v: &Vfs) {
        v.write("/f", "x").unwrap();
        assert_err!(v.create_dir("/f"), AlreadyExists, "/f");
        assert_err!(v.create_dir_all("/f"), AlreadyExists, "/f");
        assert_eq!(v.read_to_string("/f").unwrap(), "x");
    }

    pub fn create_dir_needs_parent(v: &Vfs) {
        assert_err!(v.create_dir("/missing/child"), NotFound, "/missing/child");
        assert_err!(v.write("/missing/file.txt", "x"), NotFound, "/missing/file.txt");
        assert!(!v.exists("/missing"));
    }

    pub fn create_dir_all_through_file_fails(v: &Vfs) {
        v.write("/f", "x").unwrap();
        assert_err!(v.create_dir_all("/f/a/b"), NotADirectory, "/f/a/b");
    }

    pub fn paths_through_files_are_not_directories(v: &Vfs) {
        v.create_dir("/d").unwrap();
        v.write("/d/f", "x").unwrap();
        for path in ["/d/f/x", "/d/f/x/y"] {
            assert_err!(v.metadata(path), NotADirectory, path);
            assert_err!(v.read(path), NotADirectory, path);
            assert_err!(v.write(path, "y"), NotADirectory, path);
            assert_err!(v.create_dir(path), NotADirectory, path);
            assert_err!(v.read_dir(path), NotADirectory, path);
            assert_err!(v.remove_file(path), NotADirectory, path);
        }
    }

    pub fn read_dir_errors(v: &Vfs) {
        v.write("/f", "x").unwrap();
        assert_err!(v.read_dir("/f"), NotADirectory, "/f");
        assert_err!(v.read_dir("/missing"), NotFound, "/missing");
    }

    pub fn remove_file_and_dir(v: &Vfs) {
        v.create_dir("/d").unwrap();
        v.write("/d/f", "x").unwrap();
        v.remove_file("/d/f").unwrap();
        assert!(!v.exists("/d/f"));
        v.remove_dir("/d").unwrap();
        assert!(!v.exists("/d"));
        assert!(v.read_dir("/").unwrap().is_empty());
        assert_err!(v.remove_file("/d/f"), NotFound, "/d/f");
        assert_err!(v.remove_dir("/d"), NotFound, "/d");
    }

    pub fn remove_non_empty_dir_fails(v: &Vfs) {
        v.create_dir_all("/d/sub").unwrap();
        assert_err!(v.remove_dir("/d"), DirectoryNotEmpty, "/d");
        assert!(v.is_dir("/d/sub"), "a failed remove_dir must not delete anything");
    }

    pub fn remove_dir_all_is_recursive(v: &Vfs) {
        v.create_dir_all("/t/x/y").unwrap();
        v.write("/t/x/y/deep.txt", "1").unwrap();
        v.write("/t/top.txt", "2").unwrap();
        v.write("/keep.txt", "3").unwrap();
        v.remove_dir_all("/t").unwrap();
        assert!(!v.exists("/t"));
        assert!(!v.exists("/t/x/y/deep.txt"));
        assert_eq!(names(fs(v), "/"), ["keep.txt"]);
        // Through the trait, too (not only via Vfs, which dispatches on type).
        v.create_dir_all("/u/v").unwrap();
        fs(v).remove_dir_all(&VPath::new("/u").unwrap()).unwrap();
        assert!(!v.exists("/u"));
    }

    pub fn remove_dir_all_on_file_fails(v: &Vfs) {
        v.write("/f", "x").unwrap();
        assert_err!(fs(v).remove_dir_all(&VPath::new("/f").unwrap()), NotADirectory, "/f");
        assert_err!(fs(v).remove_dir_all(&VPath::new("/missing").unwrap()), NotFound, "/missing");
        v.remove_dir_all("/f").unwrap(); // Vfs::remove_dir_all accepts files
        assert!(!v.exists("/f"));
    }

    pub fn wrong_kind_errors(v: &Vfs) {
        v.create_dir("/d").unwrap();
        v.write("/f", "x").unwrap();
        assert_err!(v.remove_file("/d"), IsADirectory, "/d");
        assert_err!(v.remove_dir("/f"), NotADirectory, "/f");
        assert_err!(v.open_read("/d"), IsADirectory, "/d");
        assert_err!(v.write("/d", "x"), IsADirectory, "/d");
        assert_err!(v.append("/d", "x"), IsADirectory, "/d");
    }

    pub fn root_is_protected(v: &Vfs) {
        v.write("/survivor", "x").unwrap();
        assert_err!(v.create_dir("/"), AlreadyExists, "/");
        assert_err!(v.remove_dir("/"), InvalidInput);
        assert_err!(v.remove_dir_all("/"), InvalidInput);
        assert_err!(v.remove_file("/"), IsADirectory, "/");
        assert_err!(v.rename("/", "/x"), InvalidInput);
        assert_err!(v.rename("/survivor", "/"), InvalidInput);
        assert_err!(v.open_read("/"), IsADirectory, "/");
        assert_err!(v.write("/", "x"), IsADirectory, "/");
        assert_err!(v.write_atomic("/", "x"), IsADirectory, "/");
        assert!(v.is_dir("/"));
        assert_eq!(v.read_to_string("/survivor").unwrap(), "x");
    }

    // --- rename --------------------------------------------------------------------

    pub fn rename_file(v: &Vfs) {
        v.write("/a.txt", "data").unwrap();
        v.create_dir("/sub").unwrap();
        v.rename("/a.txt", "/sub/b.txt").unwrap();
        assert!(!v.exists("/a.txt"));
        assert_eq!(v.read_to_string("/sub/b.txt").unwrap(), "data");
    }

    pub fn rename_replaces_existing_file(v: &Vfs) {
        v.write("/new", "new").unwrap();
        v.write("/old", "old").unwrap();
        v.rename("/new", "/old").unwrap();
        assert_eq!(v.read_to_string("/old").unwrap(), "new");
        assert!(!v.exists("/new"));
    }

    pub fn rename_moves_subtree(v: &Vfs) {
        v.create_dir_all("/src/inner").unwrap();
        v.write("/src/inner/f.txt", "deep").unwrap();
        v.write("/src/g.txt", "top").unwrap();
        v.rename("/src", "/dst").unwrap();
        assert!(!v.exists("/src"));
        assert!(!v.exists("/src/inner/f.txt"));
        assert_eq!(v.read_to_string("/dst/inner/f.txt").unwrap(), "deep");
        assert_eq!(v.read_to_string("/dst/g.txt").unwrap(), "top");
        assert_eq!(names(fs(v), "/"), ["dst"]);
    }

    pub fn rename_onto_itself_is_noop(v: &Vfs) {
        v.write("/f", "x").unwrap();
        v.create_dir("/d").unwrap();
        v.rename("/f", "/f").unwrap();
        v.rename("/d", "/d").unwrap();
        assert_eq!(v.read_to_string("/f").unwrap(), "x");
        assert!(v.is_dir("/d"));
        assert_err!(v.rename("/missing", "/missing"), NotFound, "/missing");
    }

    pub fn rename_refuses_dir_into_itself(v: &Vfs) {
        v.create_dir_all("/a/b").unwrap();
        assert_err!(v.rename("/a", "/a/b/c"), InvalidInput);
        assert_err!(v.rename("/a", "/a/c"), InvalidInput);
        assert!(v.is_dir("/a/b"));
    }

    pub fn rename_refuses_dir_target(v: &Vfs) {
        v.write("/f", "x").unwrap();
        v.create_dir("/d").unwrap();
        v.create_dir("/e").unwrap();
        assert_err!(v.rename("/f", "/d"), AlreadyExists, "/d");
        assert_err!(v.rename("/e", "/d"), AlreadyExists, "/d");
        assert!(v.is_file("/f") && v.is_dir("/e"));
    }

    pub fn rename_dir_over_file_fails(v: &Vfs) {
        v.create_dir("/d").unwrap();
        v.write("/f", "x").unwrap();
        assert_err!(v.rename("/d", "/f"), NotADirectory, "/f");
        assert_eq!(v.read_to_string("/f").unwrap(), "x");
    }

    pub fn rename_missing_source_fails(v: &Vfs) {
        assert_err!(v.rename("/nope", "/x"), NotFound, "/nope");
    }

    pub fn rename_missing_destination_parent_fails(v: &Vfs) {
        v.write("/f", "x").unwrap();
        assert_err!(v.rename("/f", "/missing/f"), NotFound, "/missing/f");
        assert_err!(v.rename("/f", "/f/inside"), NotADirectory, "/f/inside");
        assert!(v.is_file("/f"));
    }

    pub fn rename_then_recreate_source(v: &Vfs) {
        v.create_dir("/d").unwrap();
        v.write("/d/f", "1").unwrap();
        v.rename("/d", "/e").unwrap();
        v.create_dir("/d").unwrap();
        assert!(v.read_dir("/d").unwrap().is_empty(), "old children must not reappear");
        assert_eq!(v.read_to_string("/e/f").unwrap(), "1");
    }

    // --- copy ------------------------------------------------------------------------

    pub fn copy_file_and_tree(v: &Vfs) {
        v.create_dir_all("/src/a").unwrap();
        v.write("/src/a/x", "1").unwrap();
        v.write("/src/y", "22").unwrap();
        assert_eq!(v.copy("/src/y", "/y-copy").unwrap(), 2);
        v.copy_dir_all("/src", "/backup").unwrap();
        assert_eq!(v.read_to_string("/backup/a/x").unwrap(), "1");
        assert_eq!(v.read_to_string("/backup/y").unwrap(), "22");
        assert_eq!(v.read_to_string("/src/a/x").unwrap(), "1", "copy must not move");
        assert_eq!(v.read_to_string("/y-copy").unwrap(), "22");
        // Copies are independent of their source.
        v.write("/src/y", "changed").unwrap();
        assert_eq!(v.read_to_string("/y-copy").unwrap(), "22");
    }

    pub fn copy_overwrites_existing_file(v: &Vfs) {
        v.write("/a", "short").unwrap();
        v.write("/b", "a much longer text").unwrap();
        v.copy("/a", "/b").unwrap();
        assert_eq!(v.read_to_string("/b").unwrap(), "short");
    }

    pub fn copy_onto_itself_is_rejected_without_data_loss(v: &Vfs) {
        v.write("/f", "precious").unwrap();
        assert_err!(v.copy("/f", "/f"), InvalidInput);
        assert_err!(v.copy("/f", "/./f"), InvalidInput);
        assert_eq!(v.read_to_string("/f").unwrap(), "precious");
    }

    pub fn copy_directory_errors(v: &Vfs) {
        v.create_dir("/d").unwrap();
        v.write("/f", "x").unwrap();
        assert_err!(v.copy("/d", "/e"), IsADirectory, "/d");
        assert_err!(v.copy("/f", "/d"), IsADirectory, "/d");
        assert_err!(v.copy("/missing", "/x"), NotFound, "/missing");
        assert_err!(v.copy("/f", "/missing/x"), NotFound, "/missing/x");
    }

    pub fn copy_tree_into_itself_is_rejected(v: &Vfs) {
        v.create_dir_all("/src/a").unwrap();
        assert_err!(v.copy_dir_all("/src", "/src/a/inner"), InvalidInput);
        assert_err!(v.copy_dir_all("/src", "/src"), InvalidInput);
        assert_eq!(names(fs(v), "/src"), ["a"]);
    }

    // --- atomic writes ------------------------------------------------------------------

    pub fn write_atomic_replaces_and_leaves_no_temp_files(v: &Vfs) {
        v.create_dir("/cfg").unwrap();
        v.write_atomic("/cfg/app.toml", "v = 1").unwrap();
        v.write_atomic("/cfg/app.toml", "v = 2").unwrap();
        assert_eq!(v.read_to_string("/cfg/app.toml").unwrap(), "v = 2");
        assert_eq!(names(fs(v), "/cfg"), ["app.toml"]);
    }

    pub fn write_atomic_errors(v: &Vfs) {
        v.create_dir("/d").unwrap();
        v.write("/f", "x").unwrap();
        assert_err!(v.write_atomic("/d", "x"), IsADirectory, "/d");
        assert_err!(v.write_atomic("/missing/x", "x"), NotFound, "/missing/x");
        assert_err!(v.write_atomic("/f/x", "x"), NotADirectory, "/f/x");
        assert!(v.read_dir("/d").unwrap().is_empty());
        assert_eq!(names(fs(v), "/"), ["d", "f"], "no temp files may be left behind");
    }

    // --- walking ----------------------------------------------------------------------------

    pub fn walk_is_sorted_preorder(v: &Vfs) {
        v.create_dir_all("/w/b").unwrap();
        v.write("/w/b/c.txt", "").unwrap();
        v.write("/w/b/a.txt", "").unwrap();
        v.write("/w/a.txt", "").unwrap();
        let seen: Vec<String> = v.walk("/w").unwrap().map(|e| e.unwrap().path.to_string()).collect();
        assert_eq!(seen, ["/w/a.txt", "/w/b", "/w/b/a.txt", "/w/b/c.txt"]);
        assert_eq!(v.walk("/w/a.txt").unwrap().count(), 0, "walking a file yields nothing");
        assert!(v.walk("/missing").is_err());
    }

    pub fn walk_max_depth_and_filter(v: &Vfs) {
        v.create_dir_all("/r/one/two/three").unwrap();
        v.create_dir_all("/r/skip/inner").unwrap();
        let depth2: Vec<String> = v.walk("/r").unwrap().max_depth(2).map(|e| e.unwrap().path.to_string()).collect();
        assert_eq!(depth2, ["/r/one", "/r/one/two", "/r/skip", "/r/skip/inner"]);
        let pruned: Vec<String> =
            v.walk("/r").unwrap().filter_entry(|e| e.name != "skip").map(|e| e.unwrap().path.to_string()).collect();
        assert_eq!(pruned, ["/r/one", "/r/one/two", "/r/one/two/three"]);
        assert_eq!(v.walk("/r").unwrap().max_depth(0).count(), 0);
    }

    // --- paths and errors ---------------------------------------------------------------------

    pub fn dotdot_cannot_escape(v: &Vfs) {
        v.write("/../../../escaped.txt", "safe").unwrap();
        assert_eq!(v.read_to_string("/escaped.txt").unwrap(), "safe");
        assert_eq!(v.read_dir("/../..").unwrap().len(), 1);
        v.create_dir_all("/a/b").unwrap();
        assert_eq!(v.read_to_string("/a/b/../../../escaped.txt").unwrap(), "safe");
    }

    pub fn unicode_and_unusual_names(v: &Vfs) {
        let names_in = ["ação e espaço.txt", "日本語", "emoji 🦀.md", ".hidden", "a..b", "-dash", "UPPER"];
        for name in names_in {
            v.write(format!("/{name}"), name).unwrap();
        }
        for name in names_in {
            assert_eq!(v.read_to_string(format!("/{name}")).unwrap(), name, "{name}");
        }
        let mut expected: Vec<String> = names_in.iter().map(|s| (*s).to_owned()).collect();
        expected.sort();
        assert_eq!(names(fs(v), "/"), expected);
        v.create_dir("/pasta ü").unwrap();
        v.rename("/日本語", "/pasta ü/日本語").unwrap();
        assert_eq!(v.read_to_string("/pasta ü/日本語").unwrap(), "日本語");
    }

    pub fn errors_carry_the_requested_path(v: &Vfs) {
        v.write("/f", "x").unwrap();
        assert_err!(v.read("/missing/deep/file"), NotFound, "/missing/deep/file");
        assert_err!(v.metadata("/missing/deep"), NotFound, "/missing/deep");
        assert_err!(v.create_dir_all("/f/a/b"), NotADirectory, "/f/a/b");
        assert_err!(fs(v).remove_dir_all(&VPath::new("/nope").unwrap()), NotFound, "/nope");
        assert_err!(v.copy("/f", "/missing/x"), NotFound, "/missing/x");
        assert_err!(v.rename("/f", "/missing/x"), NotFound, "/missing/x");
        let err = v.read("/missing").unwrap_err();
        assert!(err.to_string().contains("/missing"), "display must name the path: {err}");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    pub fn exists_and_try_exists(v: &Vfs) {
        v.write("/f", "x").unwrap();
        assert!(v.exists("/f"));
        assert!(!v.exists("/g"));
        assert!(v.try_exists("/f").unwrap());
        assert!(!v.try_exists("/g").unwrap());
        assert!(!v.try_exists("/f/inside").unwrap(), "a path through a file simply does not exist");
        assert!(fs(v).exists(&VPath::new("/f").unwrap()));
    }
}

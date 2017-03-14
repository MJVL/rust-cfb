//! A library for reading/writing [Compound File Binary](
//! https://en.wikipedia.org/wiki/Compound_File_Binary_Format) (structured
//! storage) files.  See [MS-CFB](
//! https://msdn.microsoft.com/en-us/library/dd942138.aspx) for the format
//! specification.

#![warn(missing_docs)]

extern crate byteorder;

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::cmp::{self, Ordering};
use std::path::{Component, Path, PathBuf};
use std::io::{self, Read, Seek, SeekFrom, Write};

// ========================================================================= //

macro_rules! invalid_data {
    ($e:expr) => {
        return Err(io::Error::new(io::ErrorKind::InvalidData, $e));
    };
    ($fmt:expr, $($arg:tt)+) => {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
                                  format!($fmt, $($arg)+)));
    };
}

macro_rules! invalid_input {
    ($e:expr) => {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, $e));
    };
    ($fmt:expr, $($arg:tt)+) => {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                  format!($fmt, $($arg)+)));
    };
}

// ========================================================================= //

const HEADER_LEN: usize = 512; // length of CFB file header, in bytes
const DIR_ENTRY_LEN: usize = 128; // length of directory entry, in bytes

// Constants for CFB file header values:
const MAGIC_NUMBER: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
const MINOR_VERSION: u16 = 0x3e;
const BYTE_ORDER_MARK: u16 = 0xfffe;
const MINI_SECTOR_SHIFT: u16 = 6; // 64-byte mini sectors
const MINI_STREAM_MAX_LEN: u32 = 4096;

// Constants for FAT entries:
const MAX_REGULAR_SECTOR: u32 = 0xfffffffa;
const FAT_SECTOR: u32 = 0xfffffffd;
const END_OF_CHAIN: u32 = 0xfffffffe;
const FREE_SECTOR: u32 = 0xffffffff;

// Constants for directory entries:
const ROOT_DIR_NAME: &'static str = "Root Entry";
const DIR_NAME_MAX_LEN: usize = 31;
const OBJ_TYPE_UNALLOCATED: u8 = 0;
const OBJ_TYPE_STORAGE: u8 = 1;
const OBJ_TYPE_STREAM: u8 = 2;
const OBJ_TYPE_ROOT: u8 = 5;
const ROOT_STREAM_ID: u32 = 0;
const MAX_REGULAR_STREAM_ID: u32 = 0xfffffffa;
const NO_STREAM: u32 = 0xffffffff;

// ========================================================================= //

/// A compound file, backed by an underlying reader/writer (such as a
/// [`File`](https://doc.rust-lang.org/std/fs/struct.File.html) or
/// [`Cursor`](https://doc.rust-lang.org/std/io/struct.Cursor.html)).
pub struct CompoundFile<F> {
    inner: F,
    version: Version,
    difat: Vec<u32>,
    fat: Vec<u32>,
    directory: Vec<DirEntry>,
}

impl<F> CompoundFile<F> {
    /// Returns the CFB format version used for this compound file.
    pub fn version(&self) -> Version { self.version }

    fn stream_id_for_path(&self, path: &Path) -> io::Result<u32> {
        let mut names: Vec<String> = Vec::new();
        for component in path.components() {
            match component {
                Component::Prefix(_) => invalid_input!("Invalid path"),
                Component::RootDir => names.clear(),
                Component::CurDir => {}
                Component::ParentDir => {
                    if names.pop().is_none() {
                        invalid_input!("Invalid path");
                    }
                }
                Component::Normal(osstr) => {
                    match osstr.to_str() {
                        Some(name) => names.push(name.to_string()),
                        None => invalid_input!("Non UTF-8 path"),
                    }
                }
            }
        }
        let mut stream_id = ROOT_STREAM_ID;
        for name in names.into_iter() {
            stream_id = self.directory[stream_id as usize].child;
            loop {
                if stream_id == NO_STREAM {
                    // TODO: make this a NotFound error
                    invalid_input!("not found");
                }
                let dir_entry = &self.directory[stream_id as usize];
                match compare_names(&name, &dir_entry.name) {
                    Ordering::Equal => break,
                    Ordering::Less => stream_id = dir_entry.left_sibling,
                    Ordering::Greater => stream_id = dir_entry.right_sibling,
                }
            }
        }
        Ok(stream_id)
    }

    /// Given a path within the compound file, get information about that
    /// stream or storage object.
    pub fn entry<P: AsRef<Path>>(&self, path: P) -> io::Result<StorageEntry> {
        self.entry_for_path(path.as_ref())
    }

    fn entry_for_path(&self, path: &Path) -> io::Result<StorageEntry> {
        let stream_id = self.stream_id_for_path(path)?;
        let dir_entry = &self.directory[stream_id as usize];
        Ok(StorageEntry {
            name: dir_entry.name.clone(),
            path: path.to_path_buf(), // TODO: canonicalize path
            obj_type: dir_entry.obj_type,
            stream_len: dir_entry.stream_len,
        })
    }

    /// Returns an iterator over the entries within a storage object.
    pub fn read_storage<P: AsRef<Path>>(&self, path: P)
                                        -> io::Result<ReadStorage> {
        self.read_storage_for_path(path.as_ref())
    }

    fn read_storage_for_path(&self, path: &Path) -> io::Result<ReadStorage> {
        let stream_id = self.stream_id_for_path(path)?;
        Ok(ReadStorage {
            directory: &self.directory,
            path: path.to_path_buf(), // TODO: canonicalize path
            stack: Vec::new(),
            current: stream_id,
        })
    }

    // TODO: pub fn walk_storage

    // TODO: pub fn create_storage

    // TODO: pub fn remove_storage

    /// Opens an existing stream in the compound file for reading and/or
    /// writing (depending on what the underlying file supports).
    pub fn open_stream<P: AsRef<Path>>(&mut self, path: P)
                                       -> io::Result<Stream<F>> {
        self.open_stream_for_path(path.as_ref())
    }

    fn open_stream_for_path(&mut self, path: &Path) -> io::Result<Stream<F>> {
        let stream_id = self.stream_id_for_path(path)?;
        let (stream_len, start_sector) = {
            let dir_entry = &self.directory[stream_id as usize];
            if dir_entry.obj_type != OBJ_TYPE_STREAM {
                invalid_input!("not a stream: {:?}", path);
            }
            (dir_entry.stream_len, dir_entry.start_sector)
        };
        Ok(Stream {
            comp: self,
            total_len: stream_len,
            offset_from_start: 0,
            offset_within_sector: 0,
            start_sector: start_sector,
            current_sector: start_sector,
        })
    }

    // TODO: pub fn create_stream

    // TODO: pub fn remove_stream

    // TODO: pub fn copy_stream

    // TODO: pub fn rename

    /// Returns the root storage (i.e. directory) within this compound file.
    pub fn root_storage(&mut self) -> Storage<F> {
        Storage {
            comp: self,
            path: PathBuf::from("/"),
            stream_id: 0,
        }
    }

    /// Consumes the `CompoundFile`, returning the underlying reader/writer.
    pub fn into_inner(self) -> F { self.inner }
}

impl<F: Seek> CompoundFile<F> {
    fn seek_to_sector(&mut self, sector_index: u32) -> io::Result<()> {
        self.seek_within_sector(sector_index, 0)
    }

    fn seek_within_sector(&mut self, sector_index: u32,
                          offset_within_sector: usize)
                          -> io::Result<()> {
        self.inner
            .seek(SeekFrom::Start((offset_within_sector +
                                   self.version.sector_len() *
                                   (1 + sector_index as usize)) as
                                  u64))?;
        Ok(())
    }
}

impl<F: Read + Seek> CompoundFile<F> {
    /// Opens a existing compound file, using the underlying reader.
    pub fn open(mut inner: F) -> io::Result<CompoundFile<F>> {
        // Read basic header information.
        inner.seek(SeekFrom::Start(0))?;
        let mut magic = [0u8; 8];
        inner.read_exact(&mut magic)?;
        if magic != MAGIC_NUMBER {
            invalid_data!("Invalid CFB file (wrong magic number)");
        }
        inner.seek(SeekFrom::Start(26))?;
        let version_number = inner.read_u16::<LittleEndian>()?;
        let version = match Version::from_number(version_number) {
            Some(version) => version,
            None => {
                invalid_data!("CFB version {} is not supported",
                              version_number);
            }
        };
        if inner.read_u16::<LittleEndian>()? != BYTE_ORDER_MARK {
            invalid_data!("Invalid CFB byte order mark");
        }
        let sector_shift = inner.read_u16::<LittleEndian>()?;
        if sector_shift != version.sector_shift() {
            invalid_data!("Incorrect sector shift ({}) for CFB version {}",
                          sector_shift,
                          version.number());
        }
        let sector_len = version.sector_len();
        inner.seek(SeekFrom::Start(48))?;
        let first_dir_sector = inner.read_u32::<LittleEndian>()?;
        let mut comp = CompoundFile {
            inner: inner,
            version: version,
            difat: Vec::new(),
            fat: Vec::new(),
            directory: Vec::new(),
        };

        // Read in DIFAT.
        comp.inner.seek(SeekFrom::Start(68))?;
        let first_difat_sector = comp.inner.read_u32::<LittleEndian>()?;
        let num_difat_sectors = comp.inner.read_u32::<LittleEndian>()?;
        for _ in 0..109 {
            let next = comp.inner.read_u32::<LittleEndian>()?;
            if next == FREE_SECTOR {
                break;
            } else if next > MAX_REGULAR_SECTOR {
                invalid_data!("Invalid sector index ({}) in DIFAT", next);
            }
            comp.difat.push(next);
        }
        let mut difat_sectors = Vec::new();
        let mut current_difat_sector = first_difat_sector;
        while current_difat_sector != END_OF_CHAIN {
            difat_sectors.push(current_difat_sector);
            comp.seek_to_sector(current_difat_sector)?;
            for _ in 0..(sector_len / 4 - 1) {
                comp.difat.push(comp.inner.read_u32::<LittleEndian>()?);
            }
            current_difat_sector = comp.inner.read_u32::<LittleEndian>()?;
        }
        if num_difat_sectors as usize != difat_sectors.len() {
            invalid_data!("Incorrect DIFAT chain length \
                           (file says {}, actual is {})",
                          num_difat_sectors,
                          difat_sectors.len());
        }

        // Read in FAT.
        for index in 0..comp.difat.len() {
            let current_fat_sector = comp.difat[index];
            comp.seek_to_sector(current_fat_sector)?;
            for _ in 0..(sector_len / 4) {
                comp.fat.push(comp.inner.read_u32::<LittleEndian>()?);
            }
        }
        while comp.fat.last() == Some(&FREE_SECTOR) {
            comp.fat.pop();
        }

        // Read in directory.
        let mut current_dir_sector = first_dir_sector;
        while current_dir_sector != END_OF_CHAIN {
            comp.seek_to_sector(current_dir_sector)?;
            for _ in 0..(sector_len / DIR_ENTRY_LEN) {
                comp.directory.push(DirEntry::read(&mut comp.inner,
                                                   current_dir_sector,
                                                   version)?);
            }
            current_dir_sector = comp.fat[current_dir_sector as usize];
        }

        // TODO: Read in MiniFAT.

        Ok(comp)
    }
}

impl<F: Write + Seek> CompoundFile<F> {
    /// Creates a new compound file with no contents, using the underlying
    /// writer.  The writer should be initially empty.
    pub fn create(inner: F) -> io::Result<CompoundFile<F>> {
        CompoundFile::create_with_version(inner, Version::V4)
    }

    /// Creates a new compound file of the given version with no contents,
    /// using the underlying writer.  The writer should be initially empty.
    pub fn create_with_version(mut inner: F, version: Version)
                               -> io::Result<CompoundFile<F>> {
        // Write file header:
        inner.write_all(&MAGIC_NUMBER)?;
        inner.write_all(&[0; 16])?; // reserved field
        inner.write_u16::<LittleEndian>(MINOR_VERSION)?;
        inner.write_u16::<LittleEndian>(version.number())?;
        inner.write_u16::<LittleEndian>(BYTE_ORDER_MARK)?;
        inner.write_u16::<LittleEndian>(version.sector_shift())?;
        inner.write_u16::<LittleEndian>(MINI_SECTOR_SHIFT)?;
        inner.write_all(&[0; 6])?; // reserved field
        inner.write_u32::<LittleEndian>(1)?; // num dir sectors
        inner.write_u32::<LittleEndian>(1)?; // num FAT sectors
        inner.write_u32::<LittleEndian>(1)?; // first dir sector
        inner.write_u32::<LittleEndian>(0)?; // transaction signature (unused)
        inner.write_u32::<LittleEndian>(MINI_STREAM_MAX_LEN)?;
        inner.write_u32::<LittleEndian>(END_OF_CHAIN)?; // first MiniFAT sector
        inner.write_u32::<LittleEndian>(0)?; // num MiniFAT sectors
        inner.write_u32::<LittleEndian>(END_OF_CHAIN)?; // first DIFAT sector
        inner.write_u32::<LittleEndian>(0)?; // num DIFAT sectors
        // First 109 DIFAT entries:
        inner.write_u32::<LittleEndian>(0)?;
        for _ in 1..109 {
            inner.write_u32::<LittleEndian>(FREE_SECTOR)?;
        }
        // Pad the header with zeroes so it's the length of a sector.
        let sector_len = version.sector_len();
        debug_assert!(sector_len >= HEADER_LEN);
        if sector_len > HEADER_LEN {
            inner.write_all(&vec![0; HEADER_LEN - sector_len])?;
        }

        // Write FAT sector:
        let fat = vec![FAT_SECTOR, END_OF_CHAIN];
        for &entry in fat.iter() {
            inner.write_u32::<LittleEndian>(entry)?;
        }
        for _ in fat.len()..(sector_len / 4) {
            inner.write_u32::<LittleEndian>(FREE_SECTOR)?;
        }

        // Write directory sector:
        let root_dir_entry = DirEntry {
            sector: 1,
            name: ROOT_DIR_NAME.to_string(),
            obj_type: OBJ_TYPE_ROOT,
            left_sibling: NO_STREAM,
            right_sibling: NO_STREAM,
            child: NO_STREAM,
            start_sector: END_OF_CHAIN, // TODO: mini stream
            stream_len: 0,
        };
        root_dir_entry.write(&mut inner)?;
        for _ in 1..(sector_len / DIR_ENTRY_LEN) {
            DirEntry::write_unallacated(&mut inner)?;
        }

        Ok(CompoundFile {
            inner: inner,
            version: version,
            difat: Vec::new(),
            fat: fat,
            directory: vec![root_dir_entry],
        })
    }
}

// ========================================================================= //

struct DirEntry {
    sector: u32,
    name: String,
    obj_type: u8,
    left_sibling: u32,
    right_sibling: u32,
    child: u32,
    start_sector: u32,
    stream_len: u64,
}

impl DirEntry {
    fn read<R: Read>(reader: &mut R, sector: u32, version: Version)
                     -> io::Result<DirEntry> {
        let name: String = {
            let mut name_chars: Vec<u16> = Vec::with_capacity(32);
            for _ in 0..32 {
                name_chars.push(reader.read_u16::<LittleEndian>()?);
            }
            let name_len_bytes = reader.read_u16::<LittleEndian>()?;
            if name_len_bytes > 64 || name_len_bytes % 2 != 0 {
                invalid_data!("Invalid name length ({}) in directory entry",
                              name_len_bytes);
            }
            let name_len_chars = if name_len_bytes > 0 {
                (name_len_bytes / 2 - 1) as usize
            } else {
                0
            };
            debug_assert!(name_len_chars < name_chars.len());
            if name_chars[name_len_chars] != 0 {
                invalid_data!("Directory entry name must be null-terminated");
            }
            String::from_utf16_lossy(&name_chars[0..name_len_chars])
        };
        let obj_type = reader.read_u8()?;
        let _color = reader.read_u8()?;
        let left_sibling = reader.read_u32::<LittleEndian>()?;
        if left_sibling != NO_STREAM && left_sibling > MAX_REGULAR_STREAM_ID {
            invalid_data!("Invalid left sibling in directory entry ({})",
                          left_sibling);
        }
        let right_sibling = reader.read_u32::<LittleEndian>()?;
        if right_sibling != NO_STREAM &&
           right_sibling > MAX_REGULAR_STREAM_ID {
            invalid_data!("Invalid right sibling in directory entry ({})",
                          right_sibling);
        }
        let child = reader.read_u32::<LittleEndian>()?;
        if child != NO_STREAM && child > MAX_REGULAR_STREAM_ID {
            invalid_data!("Invalid child in directory entry ({})", child);
        }
        let mut clsid = [0u8; 16];
        reader.read_exact(&mut clsid)?;
        let _state_bits = reader.read_u32::<LittleEndian>()?;
        let _creation_time = reader.read_u64::<LittleEndian>()?;
        let _modified_time = reader.read_u64::<LittleEndian>()?;
        let start_sector = reader.read_u32::<LittleEndian>()?;
        let stream_len = reader.read_u64::<LittleEndian>()? &
                         version.stream_len_mask();
        Ok(DirEntry {
            sector: sector,
            name: name,
            obj_type: obj_type,
            left_sibling: left_sibling,
            right_sibling: right_sibling,
            child: child,
            start_sector: start_sector,
            stream_len: stream_len,
        })
    }

    fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        let name_utf16: Vec<u16> = self.name.encode_utf16().collect();
        debug_assert!(name_utf16.len() <= DIR_NAME_MAX_LEN);
        for &chr in name_utf16.iter() {
            writer.write_u16::<LittleEndian>(chr)?;
        }
        for _ in name_utf16.len()..32 {
            writer.write_u16::<LittleEndian>(0)?;
        }
        writer.write_u16::<LittleEndian>((name_utf16.len() as u16 + 1) * 2)?;
        writer.write_u8(self.obj_type)?;
        writer.write_all(&[0; 61])?; // TODO: other fields
        Ok(())
    }

    fn write_unallacated<W: Write>(writer: &mut W) -> io::Result<()> {
        writer.write_all(&[0; 64])?; // name
        writer.write_u16::<LittleEndian>(0)?; // name length
        writer.write_u8(OBJ_TYPE_UNALLOCATED)?;
        writer.write_all(&[0; 61])?; // other fields don't matter
        Ok(())
    }
}

// ========================================================================= //

/// Metadata about a single object (storage or stream) in a compound file.
#[derive(Clone)]
pub struct StorageEntry {
    name: String,
    path: PathBuf,
    obj_type: u8,
    stream_len: u64,
}

impl StorageEntry {
    /// Returns the name of the object that this entry represents.
    pub fn name(&self) -> &str { &self.name }

    /// Returns the full path to the object that this entry represents.
    pub fn path(&self) -> &Path { &self.path }

    /// Returns whether this entry is for a stream object (i.e. a "file" within
    /// the compound file).
    pub fn is_stream(&self) -> bool { self.obj_type == OBJ_TYPE_STREAM }

    /// Returns whether this entry is for a storage object (i.e. a "directory"
    /// within the compound file), either the root or a nested storage.
    pub fn is_storage(&self) -> bool {
        self.obj_type == OBJ_TYPE_STORAGE || self.obj_type == OBJ_TYPE_ROOT
    }

    /// Returns whether this entry is specifically for the root storage object
    /// of the compound file).
    pub fn is_root(&self) -> bool { self.obj_type == OBJ_TYPE_ROOT }

    /// Returns the size, in bytes, of the stream that this metadata is for.
    pub fn len(&self) -> u64 { self.stream_len }

    // TODO: creation/modified time
    // TODO: CLSID
}

// ========================================================================= //

/// Iterator over the entries in a storage object.
pub struct ReadStorage<'a> {
    directory: &'a Vec<DirEntry>,
    path: PathBuf,
    stack: Vec<u32>,
    current: u32,
}

impl<'a> Iterator for ReadStorage<'a> {
    type Item = StorageEntry;

    fn next(&mut self) -> Option<StorageEntry> {
        while self.current != NO_STREAM {
            self.stack.push(self.current);
            self.current = self.directory[self.current as usize].left_sibling;
        }
        if let Some(parent) = self.stack.pop() {
            let dir_entry = &self.directory[parent as usize];
            self.current = dir_entry.right_sibling;
            Some(StorageEntry {
                name: dir_entry.name.clone(),
                path: self.path.join(&dir_entry.name),
                obj_type: dir_entry.obj_type,
                stream_len: dir_entry.stream_len,
            })
        } else {
            None
        }
    }
}

// ========================================================================= //

/// A storage entry in a compound file, much like a filesystem directory.
pub struct Storage<'a, F: 'a> {
    comp: &'a mut CompoundFile<F>,
    path: PathBuf,
    stream_id: u32,
}

impl<'a, F> Storage<'a, F> {
    fn dir_entry(&self) -> &DirEntry {
        &self.comp.directory[self.stream_id as usize]
    }

    fn dir_entry_mut(&mut self) -> &mut DirEntry {
        &mut self.comp.directory[self.stream_id as usize]
    }

    /// Returns the name of this storage entry.
    pub fn name(&self) -> &str { &self.dir_entry().name }

    /// Returns true if this is the root storage entry, false otherwise.
    pub fn is_root(&self) -> bool {
        self.dir_entry().obj_type == OBJ_TYPE_ROOT
    }

    /// Returns this storage entry's path within the compound file.  The root
    /// storage entry has a path of `/`.
    pub fn path(&self) -> &Path { &self.path }

    /// Consumes this `Storage` object and returns its parent storage entry, or
    /// `None` if this was the root storage entry.
    pub fn parent(self) -> Option<Storage<'a, F>> {
        Some(self.comp.root_storage()) // TODO: implement this
    }
}

impl<'a, F: Write + Seek> Storage<'a, F> {
    /// Sets the name of this storage entry.  The name must encode to no more
    /// than 31 code units in UTF-16.  Fails if the new name is invalid, or if
    /// the new name is the same as one of this entry's siblings, or if this is
    /// the root entry (which cannot be renamed).
    pub fn set_name(&mut self, name: &str) -> io::Result<()> {
        if self.is_root() {
            invalid_input!("Cannot rename the root entry");
        }

        // Validate new name:
        // TODO: Check that name does not contain '/', '\', ':', or '!'.
        let name_utf16: Vec<u16> =
            name.encode_utf16().take(DIR_NAME_MAX_LEN + 1).collect();
        if name_utf16.len() > DIR_NAME_MAX_LEN {
            invalid_input!("New name cannot be more than {} UTF-16 code \
                            units (was {})",
                           DIR_NAME_MAX_LEN,
                           name.encode_utf16().count());
        }

        // TODO: check siblings for name conflicts

        // Write new name to underlying file:
        let sector = self.dir_entry().sector;
        let offset = ((self.stream_id as usize) %
                      (self.comp.version.sector_len() / DIR_ENTRY_LEN)) *
                     DIR_ENTRY_LEN;
        self.comp.seek_within_sector(sector, offset)?;
        for &chr in name_utf16.iter() {
            self.comp.inner.write_u16::<LittleEndian>(chr)?;
        }
        for _ in name_utf16.len()..32 {
            self.comp.inner.write_u16::<LittleEndian>(0)?;
        }

        self.dir_entry_mut().name = name.to_string();
        Ok(())
    }
}

// ========================================================================= //

/// A stream entry in a compound file, much like a filesystem file.
pub struct Stream<'a, F: 'a> {
    comp: &'a mut CompoundFile<F>,
    total_len: u64,
    offset_from_start: u64,
    offset_within_sector: usize,
    start_sector: u32,
    current_sector: u32,
}

// TODO: Handle case where this stream is stored in the Mini Stream.

impl<'a, F> Stream<'a, F> {
    /// Returns the current length of the stream, in bytes.
    pub fn len(&self) -> u64 { self.total_len }
}

impl<'a, F: Seek> Seek for Stream<'a, F> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(delta) => delta as i64,
            SeekFrom::End(delta) => delta + self.total_len as i64,
            SeekFrom::Current(delta) => delta + self.offset_from_start as i64,
        };
        if new_pos < 0 || (new_pos as u64) > self.total_len {
            invalid_input!("Cannot seek to {}, stream length is {}",
                           new_pos,
                           self.total_len);
        } else {
            let old_pos = self.offset_from_start as u64;
            let new_pos = new_pos as u64;
            if new_pos != self.offset_from_start {
                let sector_len = self.comp.version.sector_len() as u64;
                let mut offset = new_pos;
                let mut sector = self.start_sector;
                while offset >= sector_len {
                    sector = self.comp.fat[sector as usize];
                    offset -= sector_len;
                }
                self.comp.seek_within_sector(sector, offset as usize)?;
                self.current_sector = sector;
                self.offset_within_sector = offset as usize;
                self.offset_from_start = new_pos;
            }
            Ok(old_pos)
        }
    }
}

impl<'a, F: Read + Seek> Read for Stream<'a, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        debug_assert!(self.offset_from_start <= self.total_len);
        let remaining_in_file = self.total_len - self.offset_from_start;
        let sector_len = self.comp.version.sector_len();
        debug_assert!(self.offset_within_sector < sector_len);
        let remaining_in_sector = sector_len - self.offset_within_sector;
        let max_len = cmp::min(buf.len() as u64,
                               cmp::min(remaining_in_file,
                                        remaining_in_sector as u64)) as
                      usize;
        if max_len == 0 {
            return Ok(0);
        }
        let bytes_read = self.comp.inner.read(&mut buf[0..max_len])?;
        self.offset_from_start += bytes_read as u64;
        debug_assert!(self.offset_from_start <= self.total_len);
        self.offset_within_sector += bytes_read;
        debug_assert!(self.offset_within_sector <= sector_len);
        if self.offset_within_sector == sector_len {
            self.offset_within_sector = 0;
            self.current_sector = self.comp.fat[self.current_sector as usize];
            if self.current_sector == END_OF_CHAIN {
                debug_assert!(self.offset_from_start == self.total_len);
            } else {
                self.comp.seek_to_sector(self.current_sector)?;
            }
        }
        Ok(bytes_read)
    }
}

// TODO: impl<'a, F: Write + Seek> Write for Stream<'a, F>

// ========================================================================= //

/// The CFB format version to use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Version {
    /// Version 3, which uses 512-byte sectors.
    V3,
    /// Version 4, which uses 4096-byte sectors.
    V4,
}

impl Version {
    fn from_number(number: u16) -> Option<Version> {
        match number {
            3 => Some(Version::V3),
            4 => Some(Version::V4),
            _ => None,
        }
    }

    fn number(self) -> u16 {
        match self {
            Version::V3 => 3,
            Version::V4 => 4,
        }
    }

    fn sector_shift(self) -> u16 {
        match self {
            Version::V3 => 9, // 512-byte sectors
            Version::V4 => 12, // 4096-byte sectors
        }
    }

    fn sector_len(self) -> usize { 1 << (self.sector_shift() as usize) }

    fn stream_len_mask(self) -> u64 {
        match self {
            Version::V3 => 0xffffffff,
            Version::V4 => 0xffffffffffffffff,
        }
    }
}

// ========================================================================= //

/// Compares two directory entry names according to CFB ordering, which is
/// case-insensitive, and which always puts shorter names before longer names,
/// as encoded in UTF-16 (i.e. [shortlex
/// order](https://en.wikipedia.org/wiki/Shortlex_order), rather than
/// dictionary order).
fn compare_names(name1: &str, name2: &str) -> Ordering {
    match name1.encode_utf16().count().cmp(&name2.encode_utf16().count()) {
        // This is actually not 100% correct -- the MS-CFB spec specifies a
        // particular way of doing the uppercasing on individual UTF-16 code
        // units, along with a list of weird exceptions and corner cases.  But
        // hopefully this is good enough for 99+% of the time.
        Ordering::Equal => name1.to_uppercase().cmp(&name2.to_uppercase()),
        other => other,
    }
}

// ========================================================================= //

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use super::{CompoundFile, ROOT_DIR_NAME, Version};

    #[test]
    #[should_panic(expected = "Invalid CFB file (wrong magic number)")]
    fn wrong_magic_number() {
        let cursor = Cursor::new([1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        CompoundFile::open(cursor).unwrap();
    }

    #[test]
    fn write_and_read_empty_compound_file() {
        let version = Version::V3;

        let cursor = Cursor::new(Vec::new());
        let mut comp = CompoundFile::create_with_version(cursor, Version::V3)
            .expect("create");
        assert_eq!(comp.version(), version);
        {
            let root_storage = comp.root_storage();
            assert_eq!(root_storage.name(), ROOT_DIR_NAME);
        }

        let cursor = comp.into_inner();
        assert_eq!(cursor.get_ref().len(), 3 * version.sector_len());
        let mut comp = CompoundFile::open(cursor).expect("open");
        assert_eq!(comp.version(), version);
        {
            let root_storage = comp.root_storage();
            assert_eq!(root_storage.name(), ROOT_DIR_NAME);
        }
    }
}

// ========================================================================= //

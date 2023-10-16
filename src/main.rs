mod path;
mod security;
mod subsonic;

use crate::subsonic::{SubsonicInfo, IdentifiedSong};

use std::{
	borrow::Borrow,
	collections::HashMap,
	hash::{Hash, Hasher},
	os::windows::io::AsRawHandle,
	sync::{
		atomic::{AtomicBool, AtomicU64, Ordering},
		Arc, Mutex, RwLock, Weak,
	},
	time::SystemTime,
};

use clap::{App, Arg};
use dokan::{
	init, shutdown, unmount, CreateFileInfo, DiskSpaceInfo, FileInfo, FileSystemHandler,
	FileSystemMounter, FileTimeOperation, FillDataError, FillDataResult, FindData, FindStreamData,
	MountFlags, MountOptions, OperationInfo, OperationResult, VolumeInfo, IO_SECURITY_CONTEXT,
};
use dokan_sys::win32::{
	FILE_CREATE, FILE_DELETE_ON_CLOSE, FILE_DIRECTORY_FILE, FILE_MAXIMUM_DISPOSITION,
	FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OVERWRITE, FILE_OVERWRITE_IF,
	FILE_SUPERSEDE,
};
use widestring::{U16CStr, U16CString, U16Str, U16String};
use winapi::{
	shared::{ntdef, ntstatus::*},
	um::winnt,
};

use crate::{path::FullName, security::SecurityDescriptor};



#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct Attributes {
	value: u32,
}

impl Attributes {
	fn new(attrs: u32) -> Self {
		const SUPPORTED_ATTRS: u32 = winnt::FILE_ATTRIBUTE_ARCHIVE
			| winnt::FILE_ATTRIBUTE_HIDDEN
			| winnt::FILE_ATTRIBUTE_NOT_CONTENT_INDEXED
			| winnt::FILE_ATTRIBUTE_OFFLINE
			| winnt::FILE_ATTRIBUTE_READONLY
			| winnt::FILE_ATTRIBUTE_SYSTEM
			| winnt::FILE_ATTRIBUTE_TEMPORARY;
		Self {
			value: attrs & SUPPORTED_ATTRS,
		}
	}

	fn get_output_attrs(&self, is_dir: bool) -> u32 {
		let mut attrs = self.value;
		if is_dir {
			attrs |= winnt::FILE_ATTRIBUTE_DIRECTORY;
		}
		if attrs == 0 {
			attrs = winnt::FILE_ATTRIBUTE_NORMAL
		}
		attrs
	}
}

#[derive(Debug)]
struct Stat {
	id: u64,
	attrs: Attributes,
	ctime: SystemTime,
	mtime: SystemTime,
	atime: SystemTime,
	sec_desc: SecurityDescriptor,
	handle_count: u32,
	delete_pending: bool,
	parent: Weak<DirEntry>,
}

impl Stat {
	fn new(id: u64, attrs: u32, sec_desc: SecurityDescriptor, parent: Weak<DirEntry>) -> Self {
		let now = SystemTime::now();
		Self {
			id,
			attrs: Attributes::new(attrs),
			ctime: now,
			mtime: now,
			atime: now,
			sec_desc,
			handle_count: 0,
			delete_pending: false,
			parent,
		}
	}

	fn update_atime(&mut self, atime: SystemTime) {
		self.atime = atime;
	}

	fn update_mtime(&mut self, mtime: SystemTime) {
		self.update_atime(mtime);
		self.mtime = mtime;
	}
}

#[derive(Debug, Eq)]
struct EntryNameRef(U16Str);

fn u16_tolower(c: u16) -> u16 {
	if c >= 'A' as u16 && c <= 'Z' as u16 {
		c + 'a' as u16 - 'A' as u16
	} else {
		c
	}
}

impl Hash for EntryNameRef {
	fn hash<H: Hasher>(&self, state: &mut H) {
		for c in self.0.as_slice() {
			state.write_u16(u16_tolower(*c));
		}
	}
}

impl PartialEq for EntryNameRef {
	fn eq(&self, other: &Self) -> bool {
		if self.0.len() != other.0.len() {
			false
		} else {
			self.0
				.as_slice()
				.iter()
				.zip(other.0.as_slice())
				.all(|(c1, c2)| u16_tolower(*c1) == u16_tolower(*c2))
		}
	}
}

impl EntryNameRef {
	fn new(s: &U16Str) -> &Self {
		unsafe { &*(s as *const _ as *const Self) }
	}
}

#[derive(Debug, Clone)]
struct EntryName(U16String);

impl Borrow<EntryNameRef> for EntryName {
	fn borrow(&self) -> &EntryNameRef {
		EntryNameRef::new(&self.0)
	}
}

impl Hash for EntryName {
	fn hash<H: Hasher>(&self, state: &mut H) {
		Borrow::<EntryNameRef>::borrow(self).hash(state)
	}
}

impl PartialEq for EntryName {
	fn eq(&self, other: &Self) -> bool {
		Borrow::<EntryNameRef>::borrow(self).eq(other.borrow())
	}
}

impl Eq for EntryName {}

#[derive(Debug)]
struct FileEntry {
	stat: RwLock<Stat>,
	//data: RwLock<Vec<u8>>,
	len: u64,
}

impl FileEntry {
	fn new(stat: Stat) -> Self {
		Self {
			stat: RwLock::new(stat),
			//data: RwLock::new(Vec::new()),
			len: 0,
		}
	}
}

// The compiler incorrectly believes that its usage in a public function of the private path module is public.
#[derive(Debug)]
pub struct DirEntry {
	stat: RwLock<Stat>,
	children: RwLock<HashMap<EntryName, Entry>>,
}

impl DirEntry {
	fn new(stat: Stat) -> Self {
		Self {
			stat: RwLock::new(stat),
			children: RwLock::new(HashMap::new()),
		}
	}
}

#[derive(Debug)]
enum Entry {
	File(Arc<FileEntry>),
	Directory(Arc<DirEntry>),
}

impl Entry {
	fn stat(&self) -> &RwLock<Stat> {
		match self {
			Entry::File(file) => &file.stat,
			Entry::Directory(dir) => &dir.stat,
		}
	}

	fn is_dir(&self) -> bool {
		match self {
			Entry::File(_) => false,
			Entry::Directory(_) => true,
		}
	}
}

impl PartialEq for Entry {
	fn eq(&self, other: &Self) -> bool {
		match self {
			Entry::File(file) => {
				if let Entry::File(other_file) = other {
					Arc::ptr_eq(file, other_file)
				} else {
					false
				}
			}
			Entry::Directory(dir) => {
				if let Entry::Directory(other_dir) = other {
					Arc::ptr_eq(dir, other_dir)
				} else {
					false
				}
			}
		}
	}
}

impl Eq for Entry {}

impl Clone for Entry {
	fn clone(&self) -> Self {
		match self {
			Entry::File(file) => Entry::File(Arc::clone(file)),
			Entry::Directory(dir) => Entry::Directory(Arc::clone(dir)),
		}
	}
}

#[derive(Debug)]
struct EntryHandle {
	entry: Entry,
	delete_on_close: bool,
	mtime_delayed: Mutex<Option<SystemTime>>,
	atime_delayed: Mutex<Option<SystemTime>>,
	ctime_enabled: AtomicBool,
	mtime_enabled: AtomicBool,
	atime_enabled: AtomicBool,
}

impl EntryHandle {
	fn new(
		entry: Entry,
		delete_on_close: bool,
	) -> Self {
		entry.stat().write().unwrap().handle_count += 1;
		Self {
			entry,
			delete_on_close,
			mtime_delayed: Mutex::new(None),
			atime_delayed: Mutex::new(None),
			ctime_enabled: AtomicBool::new(true),
			mtime_enabled: AtomicBool::new(true),
			atime_enabled: AtomicBool::new(true),
		}
	}

	fn is_dir(&self) -> bool {
		self.entry.is_dir()
	}

	fn update_atime(&self, stat: &mut Stat, atime: SystemTime) {
		if self.atime_enabled.load(Ordering::Relaxed) {
			stat.atime = atime;
		}
	}

	fn update_mtime(&self, stat: &mut Stat, mtime: SystemTime) {
		self.update_atime(stat, mtime);
		if self.mtime_enabled.load(Ordering::Relaxed) {
			stat.mtime = mtime;
		}
	}
}

impl Drop for EntryHandle {
	fn drop(&mut self) {
		// The read lock on stat will be released before locking parent. This avoids possible deadlocks with
		// create_file.
		let parent = self.entry.stat().read().unwrap().parent.upgrade();
		// Lock parent before checking. This avoids racing with create_file.
		let parent_children = parent.as_ref().map(|p| p.children.write().unwrap());
		let mut stat = self.entry.stat().write().unwrap();
		if self.delete_on_close {
			stat.delete_pending = true;
		}
		stat.handle_count -= 1;
		if stat.delete_pending && stat.handle_count == 0 {
			// The result of upgrade() can be safely unwrapped here because the root directory is the only case when the
			// reference can be null, which has been handled in delete_directory.
			parent
				.as_ref()
				.unwrap()
				.stat
				.write()
				.unwrap()
				.update_mtime(SystemTime::now());
			let mut parent_children = parent_children.unwrap();
			let key = parent_children
				.iter()
				.find_map(|(k, v)| if &self.entry == v { Some(k) } else { None })
				.unwrap()
				.clone();
			parent_children
				.remove(Borrow::<EntryNameRef>::borrow(&key))
				.unwrap();
		} else {
			// Ignore root directory.
			stat.delete_pending = false
		}
	}
}

#[derive(Debug)]
struct SubFSHandler {
	id_counter: AtomicU64,
	root: Arc<DirEntry>,
	info: SubsonicInfo,
}

impl SubFSHandler {
	fn new(info: SubsonicInfo) -> Self {
		Self {
			id_counter: AtomicU64::new(1),
			root: Arc::new(DirEntry::new(Stat::new(
				0,
				0,
				SecurityDescriptor::new_default().unwrap(),
				Weak::new(),
			))),
			info,
		}
	}

	fn next_id(&self) -> u64 {
		self.id_counter.fetch_add(1, Ordering::Relaxed)
	}

	fn create_new(
		&self,
		name: &FullName,
		attrs: u32,
		delete_on_close: bool,
		creator_desc: winnt::PSECURITY_DESCRIPTOR,
		token: ntdef::HANDLE,
		parent: &Arc<DirEntry>,
		children: &mut HashMap<EntryName, Entry>,
		is_dir: bool,
	) -> OperationResult<CreateFileInfo<EntryHandle>> {
		/*
		if attrs & winnt::FILE_ATTRIBUTE_READONLY > 0 && delete_on_close {
			return Err(STATUS_CANNOT_DELETE);
		}
		let mut stat = Stat::new(
			self.next_id(),
			attrs,
			SecurityDescriptor::new_inherited(
				&parent.stat.read().unwrap().sec_desc,
				creator_desc,
				token,
				is_dir,
			)?,
			Arc::downgrade(&parent),
		);
		let stream = if let Some(stream_info) = &name.stream_info {
			if stream_info.check_default(is_dir)? {
				None
			} else {
				None
			}
		} else {
			None
		};
		let entry = if is_dir {
			Entry::Directory(Arc::new(DirEntry::new(stat)))
		} else {
			Entry::File(Arc::new(FileEntry::new(stat)))
		};
		assert!(children
			.insert(EntryName(name.file_name.to_owned()), entry.clone())
			.is_none());
		parent.stat.write().unwrap().update_mtime(SystemTime::now());
		let is_dir = is_dir && stream.is_some();
		Ok(CreateFileInfo {
			context: EntryHandle::new(entry, delete_on_close),
			is_dir,
			new_file_created: true,
		})*/
		Err(STATUS_ACCESS_DENIED)
	}
}

fn ignore_name_too_long(err: FillDataError) -> OperationResult<()> {
	match err {
		// Normal behavior.
		FillDataError::BufferFull => Err(STATUS_BUFFER_OVERFLOW),
		// Silently ignore this error because 1) file names passed to create_file should have been checked
		// by Windows. 2) We don't want an error on a single file to make the whole directory unreadable.
		FillDataError::NameTooLong => Ok(()),
	}
}

impl<'c, 'h: 'c> FileSystemHandler<'c, 'h> for SubFSHandler {
	type Context = EntryHandle;

	fn create_file(
		&'h self,
		file_name: &U16CStr,
		security_context: &IO_SECURITY_CONTEXT,
		desired_access: winnt::ACCESS_MASK,
		file_attributes: u32,
		_share_access: u32,
		create_disposition: u32,
		create_options: u32,
		info: &mut OperationInfo<'c, 'h, Self>,
	) -> OperationResult<CreateFileInfo<Self::Context>> {

		for (artist, albums) in &self.info.desired_folders {
			if file_name.to_string().unwrap().starts_with(&format!("\\{}", artist.artist).to_string()) {
				if file_name.to_string().unwrap() == format!("\\{}", artist.artist).to_string() {
					let mut artist_children: HashMap<EntryName, Entry> = HashMap::new();
					for album in albums {
						let stat = Stat::new(1, 0, SecurityDescriptor::new_default().unwrap(), Arc::<DirEntry>::downgrade(&self.root)).into();
						let tracks: HashMap<EntryName, Entry> = HashMap::new();
						let album_dir_entry: DirEntry = DirEntry {
							stat,
							children: tracks.into(),
						};
						let gen_entry: Entry = Entry::Directory(album_dir_entry.into());
						let entry_name: EntryName = EntryName(album.clone().album.into());
						artist_children.insert(entry_name, gen_entry);
					}

					let artist_dir_entry: DirEntry = DirEntry {
						stat: Stat::new(1, 0, SecurityDescriptor::new_default().unwrap(), Arc::<DirEntry>::downgrade(&self.root)).into(), 
						children: artist_children.into(),
					};

					return Ok(CreateFileInfo {
						context: EntryHandle::new(
							Entry::Directory(artist_dir_entry.into()),
							false,
						),
						is_dir: true,
						new_file_created: false,
					});
				} else {
					for album in albums {
						if file_name.to_string().unwrap().starts_with(&format!("\\{}\\{}", artist.artist, album.album).to_string()) {
							let songs: Vec<IdentifiedSong> = album.songs.clone();
							if file_name.to_string().unwrap() == format!("\\{}\\{}", artist.artist, album.album).to_string() {
								// looking at album folder
								let album_dir_entry: DirEntry = DirEntry {
									stat: Stat::new(1, 0, SecurityDescriptor::new_default().unwrap(), Arc::<DirEntry>::downgrade(&self.root)).into(), 
									children: HashMap::new().into(),
								};
								return Ok(CreateFileInfo {
									context: EntryHandle::new(
										Entry::Directory(album_dir_entry.into()),
										false,
									),
									is_dir: true,
									new_file_created: false,
								});
							} else {
								// getting song
								for song in songs {
									if file_name.to_string().unwrap() == format!("\\{}\\{}\\{}", artist.artist, album.album, song.get_filename()).to_string() {
										let song_file_entry: FileEntry = FileEntry {
											stat: Stat::new(1, 0, SecurityDescriptor::new_default().unwrap(), Arc::<DirEntry>::downgrade(&self.root)).into(), 
											//data: vec![].into(),
											len: song.get_size().unwrap(),
										};
										return Ok(CreateFileInfo {
											context: EntryHandle::new(
												Entry::File(song_file_entry.into()),
												false,
											),
											is_dir: false,
											new_file_created: false,
										});
									}
								}
							}
							
						}
					}
				}
			}
		}


		if create_disposition > FILE_MAXIMUM_DISPOSITION {
			return Err(STATUS_INVALID_PARAMETER);
		}
		let delete_on_close = create_options & FILE_DELETE_ON_CLOSE > 0;
		let path_info = path::split_path(&self.root, file_name)?;
		if let Some((name, parent)) = path_info {
			let mut children = parent.children.write().unwrap();
			if let Some(entry) = children.get(EntryNameRef::new(name.file_name)) {
				let stat = entry.stat().read().unwrap();
				let is_readonly = stat.attrs.value & winnt::FILE_ATTRIBUTE_READONLY > 0;
				let is_hidden_system = stat.attrs.value & winnt::FILE_ATTRIBUTE_HIDDEN > 0
					&& stat.attrs.value & winnt::FILE_ATTRIBUTE_SYSTEM > 0
					&& !(file_attributes & winnt::FILE_ATTRIBUTE_HIDDEN > 0
						&& file_attributes & winnt::FILE_ATTRIBUTE_SYSTEM > 0);
				if is_readonly
					&& (desired_access & winnt::FILE_WRITE_DATA > 0
						|| desired_access & winnt::FILE_APPEND_DATA > 0)
				{
					return Err(STATUS_ACCESS_DENIED);
				}
				if stat.delete_pending {
					return Err(STATUS_DELETE_PENDING);
				}
				if is_readonly && delete_on_close {
					return Err(STATUS_CANNOT_DELETE);
				}
				std::mem::drop(stat);
				match entry {
					Entry::File(file) => {
						if create_options & FILE_DIRECTORY_FILE > 0 {
							return Err(STATUS_NOT_A_DIRECTORY);
						}
						match create_disposition {
							FILE_SUPERSEDE | FILE_OVERWRITE | FILE_OVERWRITE_IF => {
								if create_disposition != FILE_SUPERSEDE && is_readonly
									|| is_hidden_system
								{
									return Err(STATUS_ACCESS_DENIED);
								}
								//file.data.write().unwrap().clear();
								let mut stat = file.stat.write().unwrap();
								stat.attrs = Attributes::new(
									file_attributes | winnt::FILE_ATTRIBUTE_ARCHIVE,
								);
								stat.update_mtime(SystemTime::now());
							}
							FILE_CREATE => return Err(STATUS_OBJECT_NAME_COLLISION),
							_ => (),
						}
						Ok(CreateFileInfo {
							context: EntryHandle::new(
								Entry::File(Arc::clone(&file)),
								delete_on_close,
							),
							is_dir: false,
							new_file_created: false,
						})
					}
					Entry::Directory(dir) => {
						if create_options & FILE_NON_DIRECTORY_FILE > 0 {
							return Err(STATUS_FILE_IS_A_DIRECTORY);
						}
						match create_disposition {
							FILE_OPEN | FILE_OPEN_IF => Ok(CreateFileInfo {
								context: EntryHandle::new(
									Entry::Directory(Arc::clone(&dir)),
									delete_on_close,
								),
								is_dir: true,
								new_file_created: false,
							}),
							FILE_CREATE => Err(STATUS_OBJECT_NAME_COLLISION),
							_ => Err(STATUS_INVALID_PARAMETER),
						}
					}
				}
			} else {
				if parent.stat.read().unwrap().delete_pending {
					return Err(STATUS_DELETE_PENDING);
				}
				let token = info.requester_token().unwrap();
				if create_options & FILE_DIRECTORY_FILE > 0 {
					match create_disposition {
						FILE_CREATE | FILE_OPEN_IF => self.create_new(
							&name,
							file_attributes,
							delete_on_close,
							security_context.AccessState.SecurityDescriptor,
							token.as_raw_handle(),
							&parent,
							&mut children,
							true,
						),
						FILE_OPEN => Err(STATUS_OBJECT_NAME_NOT_FOUND),
						_ => Err(STATUS_INVALID_PARAMETER),
					}
				} else {
					if create_disposition == FILE_OPEN || create_disposition == FILE_OVERWRITE {
						Err(STATUS_OBJECT_NAME_NOT_FOUND)
					} else {
						self.create_new(
							&name,
							file_attributes | winnt::FILE_ATTRIBUTE_ARCHIVE,
							delete_on_close,
							security_context.AccessState.SecurityDescriptor,
							token.as_raw_handle(),
							&parent,
							&mut children,
							false,
						)
					}
				}
			}
		} else {
			if create_disposition == FILE_OPEN || create_disposition == FILE_OPEN_IF {
				if create_options & FILE_NON_DIRECTORY_FILE > 0 {
					Err(STATUS_FILE_IS_A_DIRECTORY)
				} else {
					Ok(CreateFileInfo {
						context: EntryHandle::new(
							Entry::Directory(Arc::clone(&self.root)),
							info.delete_on_close(),
						),
						is_dir: true,
						new_file_created: false,
					})
				}
			} else {
				Err(STATUS_INVALID_PARAMETER)
			}
		}
	}

	fn close_file(
		&'h self,
		_file_name: &U16CStr,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) {
		let mut stat = context.entry.stat().write().unwrap();
		if let Some(mtime) = context.mtime_delayed.lock().unwrap().clone() {
			if mtime > stat.mtime {
				stat.mtime = mtime;
			}
		}
		if let Some(atime) = context.atime_delayed.lock().unwrap().clone() {
			if atime > stat.atime {
				stat.atime = atime;
			}
		}
	}

	fn read_file(
		&'h self,
		file_name: &U16CStr,
		offset: i64,
		buffer: &mut [u8],
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<u32> {

		let mut url: Option<String> = None;

		for (artist, albums) in &self.info.desired_folders {
			if file_name.to_string().unwrap().starts_with(&format!("\\{}", artist.artist).to_string()) && !(file_name.to_string().unwrap() == format!("\\{}", artist.artist).to_string()) {
					for album in albums {
						if file_name.to_string().unwrap().starts_with(&format!("\\{}\\{}", artist.artist, album.album).to_string()) {
							let songs: Vec<IdentifiedSong> = album.songs.clone();
								// getting song
								for song in songs {
									if file_name.to_string().unwrap() == format!("\\{}\\{}\\{}", artist.artist, album.album, song.get_filename()).to_string() {
										url = Some(song.url.unwrap());
									}
								}
						}
					}
			}
		}

		match url {
			Some(url) => {
				let client = reqwest::blocking::Client::new();
				let data = client.get(url)
				.header(reqwest::header::RANGE, format!("bytes={offset}-{}", offset as usize + buffer.len()-1) )
				.send()
				.unwrap()
				.bytes()
				.unwrap();
				let len = data.len();
				buffer[0..len].copy_from_slice(&data);
				Ok(len.try_into().unwrap())
			},
			None => {
				return Err(STATUS_INVALID_DEVICE_REQUEST);
			}
		}


	}

	fn write_file(
		&'h self,
		_file_name: &U16CStr,
		offset: i64,
		buffer: &[u8],
		info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<u32> {
		Err(STATUS_ACCESS_DENIED)
	}

	fn flush_file_buffers(
		&'h self,
		_file_name: &U16CStr,
		_info: &OperationInfo<'c, 'h, Self>,
		_context: &'c Self::Context,
	) -> OperationResult<()> {
		Ok(())
	}

	fn get_file_information(
		&'h self,
		_file_name: &U16CStr,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<FileInfo> {
		let stat = context.entry.stat().read().unwrap();
		Ok(FileInfo {
			attributes: stat.attrs.get_output_attrs(context.is_dir()),
			creation_time: stat.ctime,
			last_access_time: stat.atime,
			last_write_time: stat.mtime,
			file_size: match &context.entry {
					Entry::File(file) => file.len as u64,
					Entry::Directory(_) => 0,
				},
			number_of_links: 1,
			file_index: stat.id,
		})
	}

	fn find_files(
		&'h self,
		file_name: &U16CStr,
		mut fill_find_data: impl FnMut(&FindData) -> FillDataResult,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		
		if let Entry::Directory(dir) = &context.entry {
			let mut children: HashMap<EntryName, Entry> = HashMap::new();


			// root directory should only display artists
			if file_name.to_string().unwrap() == "\\" {
				for (artist, albums) in &self.info.desired_folders {
					let stat = Stat::new(1, 0, SecurityDescriptor::new_default().unwrap(), Arc::<DirEntry>::downgrade(&dir)).into();
					let artist_children: HashMap<EntryName, Entry> = HashMap::new();
					let artist_dir_entry: DirEntry = DirEntry {
						stat,
						children: artist_children.into(),
					};
					let gen_entry: Entry = Entry::Directory(artist_dir_entry.into());
					let entry_name: EntryName = EntryName(artist.clone().artist.into());
					children.insert(entry_name, gen_entry);
				}
			} else {

				
				// inside an artist folder, show only this artist's albums
				for (artist, albums) in &self.info.desired_folders {
					if file_name.to_string().unwrap().starts_with(&format!("\\{}", artist.artist).to_string()) {
						if file_name.to_string().unwrap() == format!("\\{}", artist.artist).to_string() {
							// we're looking at the artist folder itself
							for album in albums {
								let stat = Stat::new(1, 0, SecurityDescriptor::new_default().unwrap(), Arc::<DirEntry>::downgrade(&dir)).into();
								let tracks: HashMap<EntryName, Entry> = HashMap::new();
								let album_dir_entry: DirEntry = DirEntry {
									stat,
									children: tracks.into(),
								};
								let gen_entry: Entry = Entry::Directory(album_dir_entry.into());
								let entry_name: EntryName = EntryName(album.clone().album.into());
								children.insert(entry_name, gen_entry);
							}
						} else {
							// we're looking at an album inside the artist's folder
							for album in albums {
								if file_name.to_string().unwrap() == format!("\\{}\\{}", artist.artist, album.album).to_string() {
									for song in &album.songs {
										let stat = Stat::new(1, 0, SecurityDescriptor::new_default().unwrap(), Arc::<DirEntry>::downgrade(&dir)).into();
										let song_file_entry: FileEntry = FileEntry {
											stat,
											//data: vec![].into(),
											len: song.get_size().unwrap(),
										};
										let gen_entry: Entry = Entry::File(song_file_entry.into());
										let entry_name: EntryName = EntryName(song.get_filename().into());
										children.insert(entry_name, gen_entry);
									}
								}
							}

						}


					}
				}
			}
			



			for (k, v) in children.iter() {
				let stat = v.stat().read().unwrap();
				fill_find_data(&FindData {
					attributes: stat.attrs.get_output_attrs(v.is_dir()),
					creation_time: stat.ctime,
					last_access_time: stat.atime,
					last_write_time: stat.mtime,
					file_size: match v {
						Entry::File(file) => file.len as u64,
						Entry::Directory(_) => 0,
					},
					file_name: U16CString::from_ustr(&k.0).unwrap(),
				})
				.or_else(ignore_name_too_long)?;
			}
			Ok(())
		} else {
			Err(STATUS_INVALID_DEVICE_REQUEST)
		}
	}

	fn set_file_attributes(
		&'h self,
		_file_name: &U16CStr,
		file_attributes: u32,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		let mut stat = context.entry.stat().write().unwrap();
		stat.attrs = Attributes::new(file_attributes);
		context.update_atime(&mut stat, SystemTime::now());
		Ok(())
	}

	fn set_file_time(
		&'h self,
		_file_name: &U16CStr,
		creation_time: FileTimeOperation,
		last_access_time: FileTimeOperation,
		last_write_time: FileTimeOperation,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		let mut stat = context.entry.stat().write().unwrap();
		let process_time_info = |time_info: &FileTimeOperation,
		                         time: &mut SystemTime,
		                         flag: &AtomicBool| match time_info {
			FileTimeOperation::SetTime(new_time) => {
				if flag.load(Ordering::Relaxed) {
					*time = *new_time
				}
			}
			FileTimeOperation::DisableUpdate => flag.store(false, Ordering::Relaxed),
			FileTimeOperation::ResumeUpdate => flag.store(true, Ordering::Relaxed),
			FileTimeOperation::DontChange => (),
		};
		process_time_info(&creation_time, &mut stat.ctime, &context.ctime_enabled);
		process_time_info(&last_write_time, &mut stat.mtime, &context.mtime_enabled);
		process_time_info(&last_access_time, &mut stat.atime, &context.atime_enabled);
		Ok(())
	}

	fn delete_file(
		&'h self,
		_file_name: &U16CStr,
		info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		if context.entry.stat().read().unwrap().attrs.value & winnt::FILE_ATTRIBUTE_READONLY > 0 {
			return Err(STATUS_CANNOT_DELETE);
		}
		Ok(())
	}

	fn delete_directory(
		&'h self,
		_file_name: &U16CStr,
		info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		if let Entry::Directory(dir) = &context.entry {
			Ok(())
		} else {
			Err(STATUS_INVALID_DEVICE_REQUEST)
		}
	}

	fn move_file(
		&'h self,
		file_name: &U16CStr,
		new_file_name: &U16CStr,
		replace_if_existing: bool,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		Ok(())
	}

	fn set_end_of_file(
		&'h self,
		_file_name: &U16CStr,
		offset: i64,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		Ok(())
	}

	fn set_allocation_size(
		&'h self,
		_file_name: &U16CStr,
		alloc_size: i64,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		Ok(())
	}

	fn get_disk_free_space(
		&'h self,
		_info: &OperationInfo<'c, 'h, Self>,
	) -> OperationResult<DiskSpaceInfo> {
		Ok(DiskSpaceInfo {
			byte_count: 1024 * 1024 * 1024,
			free_byte_count: 512 * 1024 * 1024,
			available_byte_count: 512 * 1024 * 1024,
		})
	}

	fn get_volume_information(
		&'h self,
		_info: &OperationInfo<'c, 'h, Self>,
	) -> OperationResult<VolumeInfo> {
		Ok(VolumeInfo {
			name: U16CString::from_str("SonicDisk").unwrap(),
			serial_number: 0,
			max_component_length: path::MAX_COMPONENT_LENGTH,
			fs_flags: winnt::FILE_CASE_PRESERVED_NAMES
				| winnt::FILE_CASE_SENSITIVE_SEARCH
				| winnt::FILE_UNICODE_ON_DISK
				| winnt::FILE_PERSISTENT_ACLS
				| winnt::FILE_NAMED_STREAMS,
			// Custom names don't play well with UAC.
			fs_name: U16CString::from_str("NTFS").unwrap(),
		})
	}

	fn mounted(
		&'h self,
		_mount_point: &U16CStr,
		_info: &OperationInfo<'c, 'h, Self>,
	) -> OperationResult<()> {
		Ok(())
	}

	fn unmounted(&'h self, _info: &OperationInfo<'c, 'h, Self>) -> OperationResult<()> {
		Ok(())
	}

	fn get_file_security(
		&'h self,
		_file_name: &U16CStr,
		security_information: u32,
		security_descriptor: winnt::PSECURITY_DESCRIPTOR,
		buffer_length: u32,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<u32> {
		context
			.entry
			.stat()
			.read()
			.unwrap()
			.sec_desc
			.get_security_info(security_information, security_descriptor, buffer_length)
	}

	fn set_file_security(
		&'h self,
		_file_name: &U16CStr,
		security_information: u32,
		security_descriptor: winnt::PSECURITY_DESCRIPTOR,
		_buffer_length: u32,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		let mut stat = context.entry.stat().write().unwrap();
		let ret = stat
			.sec_desc
			.set_security_info(security_information, security_descriptor);
		if ret.is_ok() {
			context.update_atime(&mut stat, SystemTime::now());
		}
		ret
	}

	fn find_streams(
		&'h self,
		_file_name: &U16CStr,
		mut fill_find_stream_data: impl FnMut(&FindStreamData) -> FillDataResult,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		if let Entry::File(file) = &context.entry {
			fill_find_stream_data(&FindStreamData {
				size: file.len as i64,
				name: U16CString::from_str("::$DATA").unwrap(),
			})
			.or_else(ignore_name_too_long)?;
		}
		
		Ok(())
	}
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let matches = App::new("SonicDisk")
		.author(env!("CARGO_PKG_AUTHORS"))
		.arg(
			Arg::with_name("mount_point")
				.short("m")
				.long("mount-point")
				.takes_value(true)
				.value_name("MOUNT_POINT")
				.required(true)
				.help("Mount point path."),
		)
		.arg(
			Arg::with_name("username")
			.short("u")
			.long("username")
			.takes_value(true)
			.value_name("USERNAME")
			.required(true)
			.help("Subsonic server username")
		).arg(
			Arg::with_name("password")
			.short("p")
			.long("password")
			.takes_value(true)
			.value_name("PASSWORD")
			.required(true)
			.help("Subsonic server password")
		)
		.arg(
			Arg::with_name("url")
			.short("l")
			.long("url")
			.takes_value(true)
			.value_name("URL")
			.required(true)
			.help("Subsonic server url")
		)
		.arg(
			Arg::with_name("single_thread")
				.short("t")
				.long("single-thread")
				.help("Force a single thread. Otherwise Dokan will allocate the number of threads regarding the workload."),
		)
		.get_matches();
	
	let mount_point = U16CString::from_str(matches.value_of("mount_point").unwrap())?;
	
	let client: sunk::Client = subsonic::client(matches.value_of("url").unwrap(), matches.value_of("username").unwrap(), matches.value_of("password").unwrap());

	let mut flags = MountFlags::ALT_STREAM;
	if matches.is_present("dokan_debug") {
		flags |= MountFlags::DEBUG | MountFlags::STDERR;
	}
	if matches.is_present("removable") {
		flags |= MountFlags::REMOVABLE;
	}

	let options = MountOptions {
		single_thread: matches.is_present("single_thread"),
		flags,
		..Default::default()
	};

	let info = SubsonicInfo::new(client);

	let handler = SubFSHandler::new(info);

	init();

	let mut mounter = FileSystemMounter::new(&handler, &mount_point, &options);

	println!("File system will mount...");

	let file_system = mounter.mount()?;
	
	

	// Another thread can unmount the file system.
	let mount_point = mount_point.clone();
	ctrlc::set_handler(move || {
		if unmount(&mount_point) {
			println!("File system will unmount...")
		} else {
			eprintln!("Failed to unmount file system.");
		}
	})
	.expect("failed to set Ctrl-C handler");

	println!("File system is mounted, press Ctrl-C to unmount.");

	drop(file_system);

	println!("File system is unmounted.");

	shutdown();

	Ok(())
}
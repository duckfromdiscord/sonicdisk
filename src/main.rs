mod path;
mod security;

use std::{
	borrow::Borrow,
	collections::HashMap,
	hash::{Hash, Hasher},
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
	FILE_DELETE_ON_CLOSE,  FILE_MAXIMUM_DISPOSITION,
	FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, 
};
use widestring::{U16CStr, U16CString, U16Str, U16String};
use winapi::{
	shared::{ntdef, ntstatus::*},
	um::{winnt, processthreadsapi::OpenProcessToken},
};

use crate::{path::FullName, security::SecurityDescriptor};

#[derive(Debug)]
struct AltStream {
	handle_count: u32,
	delete_pending: bool,
	data: Vec<u8>,
}

impl AltStream {
	fn new() -> Self {
		Self {
			handle_count: 0,
			delete_pending: false,
			data: Vec::new(),
		}
	}
}

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
	alt_streams: HashMap<EntryName, Arc<RwLock<AltStream>>>,
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
			alt_streams: HashMap::new(),
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
	data: RwLock<Vec<u8>>,
}

impl FileEntry {
	fn new(stat: Stat) -> Self {
		Self {
			stat: RwLock::new(stat),
			data: RwLock::new(Vec::new()),
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
	alt_stream: RwLock<Option<Arc<RwLock<AltStream>>>>,
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
		alt_stream: Option<Arc<RwLock<AltStream>>>,
		delete_on_close: bool,
	) -> Self {
		entry.stat().write().unwrap().handle_count += 1;
		if let Some(s) = &alt_stream {
			s.write().unwrap().handle_count += 1;
		}
		Self {
			entry,
			alt_stream: RwLock::new(alt_stream),
			delete_on_close,
			mtime_delayed: Mutex::new(None),
			atime_delayed: Mutex::new(None),
			ctime_enabled: AtomicBool::new(true),
			mtime_enabled: AtomicBool::new(true),
			atime_enabled: AtomicBool::new(true),
		}
	}

	fn is_dir(&self) -> bool {
		if self.alt_stream.read().unwrap().is_some() {
			false
		} else {
			self.entry.is_dir()
		}
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
		if self.delete_on_close && self.alt_stream.read().unwrap().is_none() {
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
		let alt_stream = self.alt_stream.read().unwrap();
		if let Some(stream) = alt_stream.as_ref() {
			stat.mtime = SystemTime::now();
			let mut stream_locked = stream.write().unwrap();
			if self.delete_on_close {
				stream_locked.delete_pending = true;
			}
			stream_locked.handle_count -= 1;
			if stream_locked.delete_pending && stream_locked.handle_count == 0 {
				let key = stat
					.alt_streams
					.iter()
					.find_map(|(k, v)| {
						if Arc::ptr_eq(stream, v) {
							Some(k)
						} else {
							None
						}
					})
					.unwrap()
					.clone();
				stat.alt_streams
					.remove(Borrow::<EntryNameRef>::borrow(&key))
					.unwrap();
				self.update_atime(&mut stat, SystemTime::now());
			}
		}
	}
}

#[derive(Debug)]
struct MemFsHandler {
	id_counter: AtomicU64,
	root: Arc<DirEntry>,
}

impl MemFsHandler {
	fn new() -> Self {
		Self {
			id_counter: AtomicU64::new(1),
			root: Arc::new(DirEntry::new(Stat::new(
				0,
				0,
				SecurityDescriptor::new_default().unwrap(),
				Weak::new(),
			))),
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
				let stream = Arc::new(RwLock::new(AltStream::new()));
				assert!(stat
					.alt_streams
					.insert(EntryName(stream_info.name.to_owned()), Arc::clone(&stream))
					.is_none());
				Some(stream)
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
			context: EntryHandle::new(entry, stream, delete_on_close),
			is_dir,
			new_file_created: true,
		})
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

impl<'c, 'h: 'c> FileSystemHandler<'c, 'h> for MemFsHandler {
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
		if create_disposition > FILE_MAXIMUM_DISPOSITION {
			return Err(STATUS_INVALID_PARAMETER);
		}
		let delete_on_close = create_options & FILE_DELETE_ON_CLOSE > 0;
		let path_info = path::split_path(&self.root, file_name)?;
		if let Some((name, parent)) = path_info {
			Err(STATUS_ACCESS_DENIED)
			// you can't add a file to a subsonic server
		} else {
			if create_disposition == FILE_OPEN || create_disposition == FILE_OPEN_IF {
				if create_options & FILE_NON_DIRECTORY_FILE > 0 {
					Err(STATUS_FILE_IS_A_DIRECTORY)
				} else {
					Ok(CreateFileInfo {
						context: EntryHandle::new(
							Entry::Directory(Arc::clone(&self.root)),
							None,
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
		_file_name: &U16CStr,
		offset: i64,
		buffer: &mut [u8],
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<u32> {
		let mut do_read = |data: &Vec<_>| {
			let offset = offset as usize;
			let len = std::cmp::min(buffer.len(), data.len() - offset);
			buffer[0..len].copy_from_slice(&data[offset..offset + len]);
			len as u32
		};
		let alt_stream = context.alt_stream.read().unwrap();
		if let Some(stream) = alt_stream.as_ref() {
			Ok(do_read(&stream.read().unwrap().data))
		} else if let Entry::File(file) = &context.entry {
			Ok(do_read(&file.data.read().unwrap()))
		} else {
			Err(STATUS_INVALID_DEVICE_REQUEST)
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
		return Err(STATUS_ACCESS_DENIED);
		// not allowed by subsonic api
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
		let alt_stream = context.alt_stream.read().unwrap();
		Ok(FileInfo {
			attributes: stat.attrs.get_output_attrs(context.is_dir()),
			creation_time: stat.ctime,
			last_access_time: stat.atime,
			last_write_time: stat.mtime,
			file_size: if let Some(stream) = alt_stream.as_ref() {
				stream.read().unwrap().data.len() as u64
			} else {
				match &context.entry {
					Entry::File(file) => file.data.read().unwrap().len() as u64,
					Entry::Directory(_) => 0,
				}
			},
			number_of_links: 1,
			file_index: stat.id,
		})
	}

	fn find_files(
		&'h self,
		_file_name: &U16CStr,
		mut fill_find_data: impl FnMut(&FindData) -> FillDataResult,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		if context.alt_stream.read().unwrap().is_some() {
			return Err(STATUS_INVALID_DEVICE_REQUEST);
		}
		if let Entry::Directory(dir) = &context.entry {
			let children = dir.children.read().unwrap();
			let our_entry_name: EntryName = EntryName {
				0: "test".into()
			};
			let our_file_entry: FileEntry = FileEntry { stat: Stat::new(1, 0, SecurityDescriptor::new_default().unwrap(), Arc::<DirEntry>::downgrade(&dir)).into(), data: Vec::new().into() };
			let our_gen_entry: Entry = Entry::File(our_file_entry.into());
			let mut children: HashMap<EntryName, Entry> = children.clone();
			children.insert(our_entry_name, our_gen_entry);
			
			for (k, v) in children.iter() {
				let stat = v.stat().read().unwrap();
				fill_find_data(&FindData {
					attributes: stat.attrs.get_output_attrs(v.is_dir()),
					creation_time: stat.ctime,
					last_access_time: stat.atime,
					last_write_time: stat.mtime,
					file_size: match v {
						Entry::File(file) => file.data.read().unwrap().len() as u64,
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
		Ok(())
	}

	fn delete_file(
		&'h self,
		_file_name: &U16CStr,
		info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		Err(STATUS_ACCESS_DENIED)
	}

	fn delete_directory(
		&'h self,
		_file_name: &U16CStr,
		info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		if context.alt_stream.read().unwrap().is_some() {
			return Err(STATUS_INVALID_DEVICE_REQUEST);
		}
		if let Entry::Directory(dir) = &context.entry {
			// Lock children first to avoid race conditions.
			let children = dir.children.read().unwrap();
			let mut stat = dir.stat.write().unwrap();
			if stat.parent.upgrade().is_none() {
				// Root directory can't be deleted.
				return Err(STATUS_ACCESS_DENIED);
			}
			if info.delete_on_close() && !children.is_empty() {
				Err(STATUS_DIRECTORY_NOT_EMPTY)
			} else {
				Err(STATUS_ACCESS_DENIED)
			}
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
		Err(STATUS_ACCESS_DENIED)
	}

	fn set_end_of_file(
		&'h self,
		_file_name: &U16CStr,
		offset: i64,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		Err(STATUS_ACCESS_DENIED)
	}

	fn set_allocation_size(
		&'h self,
		_file_name: &U16CStr,
		alloc_size: i64,
		_info: &OperationInfo<'c, 'h, Self>,
		context: &'c Self::Context,
	) -> OperationResult<()> {
		let set_alloc = |data: &mut Vec<_>| {
			let alloc_size = alloc_size as usize;
			let cap = data.capacity();
			if alloc_size < data.len() {
				data.resize(alloc_size, 0);
			} else if alloc_size < cap {
				let mut new_data = Vec::with_capacity(alloc_size);
				new_data.append(data);
				*data = new_data;
			} else if alloc_size > cap {
				data.reserve(alloc_size - cap);
			}
		};
		let alt_stream = context.alt_stream.read().unwrap();
		let ret = if let Some(stream) = alt_stream.as_ref() {
			set_alloc(&mut stream.write().unwrap().data);
			Ok(())
		} else if let Entry::File(file) = &context.entry {
			set_alloc(&mut file.data.write().unwrap());
			Ok(())
		} else {
			Err(STATUS_INVALID_DEVICE_REQUEST)
		};
		if ret.is_ok() {
			context.update_mtime(
				&mut context.entry.stat().write().unwrap(),
				SystemTime::now(),
			);
		}
		ret
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
				size: file.data.read().unwrap().len() as i64,
				name: U16CString::from_str("::$DATA").unwrap(),
			})
			.or_else(ignore_name_too_long)?;
		}
		for (k, v) in context.entry.stat().read().unwrap().alt_streams.iter() {
			let mut name_buf = vec![':' as u16];
			name_buf.extend_from_slice(k.0.as_slice());
			name_buf.extend_from_slice(U16String::from_str(":$DATA").as_slice());
			fill_find_stream_data(&FindStreamData {
				size: v.read().unwrap().data.len() as i64,
				name: U16CString::from_ustr(U16Str::from_slice(&name_buf)).unwrap(),
			})
			.or_else(ignore_name_too_long)?;
		}
		Ok(())
	}
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let matches = App::new("dokan-rust memfs example")
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
			Arg::with_name("single_thread")
				.short("t")
				.long("single-thread")
				.help("Force a single thread. Otherwise Dokan will allocate the number of threads regarding the workload."),
		)
		.arg(
			Arg::with_name("dokan_debug")
				.short("d")
				.long("dokan-debug")
				.help("Enable Dokan's debug output."),
		)
		.arg(
			Arg::with_name("removable")
				.short("r")
				.long("removable")
				.help("Mount as a removable drive."),
		)
		.get_matches();

	let mount_point = U16CString::from_str(matches.value_of("mount_point").unwrap())?;

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

	let handler = MemFsHandler::new();
	
	let mut empty: HashMap<EntryName, Entry> = HashMap::new();
	use widestring::u16str;

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

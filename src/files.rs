use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::protocol::{
    BeginDownloadRequest, BeginDownloadResponse, BeginUploadRequest, FileCapabilitiesResponse,
    FileChunkResponse, FileKind, FileListRequest, FileListResponse, FileMetadata, FileStatRequest,
    FileStatResponse, ReadFileChunkRequest, UploadStatusResponse, WriteFileChunkRequest,
};

pub const FILE_PROTOCOL_VERSION: u32 = 1;
pub const MAX_FILE_CHUNK_SIZE: usize = 1024 * 1024;
const MAX_DIRECTORY_PAGE_SIZE: usize = 500;
const MAX_REMOTE_PATH_SIZE: usize = 16 * 1024;
const MAX_TRACKED_UPLOADS: usize = 128;

#[derive(Debug)]
pub struct FileServiceError {
    pub code: &'static str,
    pub message: String,
}

impl FileServiceError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn io(action: impl fmt::Display, error: std::io::Error) -> Self {
        let code = match error.kind() {
            std::io::ErrorKind::NotFound => "not_found",
            std::io::ErrorKind::AlreadyExists => "conflict",
            std::io::ErrorKind::PermissionDenied => "permission_denied",
            std::io::ErrorKind::InvalidInput => "invalid",
            _ => "filesystem",
        };
        Self::new(code, format!("{action}: {error}"))
    }
}

impl fmt::Display for FileServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FileServiceError {}

pub type FileResult<T> = std::result::Result<T, FileServiceError>;

#[derive(Clone, Debug, Eq, PartialEq)]
enum UploadState {
    Active,
    Completed,
    Aborted,
}

impl UploadState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Clone, Debug)]
struct Upload {
    transfer_id: String,
    requested_path: Vec<u8>,
    target: PathBuf,
    temporary: PathBuf,
    size: u64,
    expected_sha256: Vec<u8>,
    overwrite: bool,
    mode: u32,
    committed_offset: u64,
    state: UploadState,
}

impl Upload {
    fn status(&self) -> UploadStatusResponse {
        UploadStatusResponse {
            transfer_id: self.transfer_id.clone(),
            path: self.requested_path.clone(),
            size: self.size,
            committed_offset: self.committed_offset,
            state: self.state.as_str().into(),
        }
    }
}

/// Per-Unix-user file service. In managed mode this object lives inside the
/// unprivileged user worker, so filesystem access never executes in the root gateway.
#[derive(Clone)]
pub struct FileService {
    root: Arc<PathBuf>,
    uploads: Arc<Mutex<HashMap<String, Upload>>>,
}

impl FileService {
    pub fn new(root: PathBuf) -> FileResult<Self> {
        let root = root.canonicalize().map_err(|error| {
            FileServiceError::io(format!("invalid file root {}", root.display()), error)
        })?;
        if !root.is_dir() {
            return Err(FileServiceError::new(
                "invalid",
                format!("file root {} is not a directory", root.display()),
            ));
        }
        Ok(Self {
            root: Arc::new(root),
            uploads: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn capabilities(&self) -> FileCapabilitiesResponse {
        FileCapabilitiesResponse {
            version: FILE_PROTOCOL_VERSION,
            max_chunk_size: MAX_FILE_CHUNK_SIZE as u32,
            resumable_uploads: true,
            atomic_upload_commit: true,
            chunk_sha256: true,
        }
    }

    pub fn stat(&self, request: FileStatRequest) -> FileResult<FileStatResponse> {
        let path = self.resolve_existing(&request.path, request.follow_symlinks)?;
        let metadata = if request.follow_symlinks {
            fs::metadata(&path)
        } else {
            fs::symlink_metadata(&path)
        }
        .map_err(|error| FileServiceError::io(format!("cannot stat {}", path.display()), error))?;
        Ok(FileStatResponse {
            metadata: Some(file_metadata(&path, &metadata)),
        })
    }

    pub fn list(&self, request: FileListRequest) -> FileResult<FileListResponse> {
        let directory = self.resolve_existing(&request.path, true)?;
        let metadata = fs::metadata(&directory).map_err(|error| {
            FileServiceError::io(format!("cannot stat {}", directory.display()), error)
        })?;
        if !metadata.is_dir() {
            return Err(FileServiceError::new(
                "invalid",
                format!("{} is not a directory", directory.display()),
            ));
        }

        let limit = match request.limit {
            0 => 100,
            value => usize::try_from(value)
                .unwrap_or(MAX_DIRECTORY_PAGE_SIZE)
                .min(MAX_DIRECTORY_PAGE_SIZE),
        };
        let mut entries = fs::read_dir(&directory)
            .map_err(|error| {
                FileServiceError::io(
                    format!("cannot read directory {}", directory.display()),
                    error,
                )
            })?
            .map(|entry| {
                entry.map_err(|error| {
                    FileServiceError::io(
                        format!("cannot read directory entry in {}", directory.display()),
                        error,
                    )
                })
            })
            .collect::<FileResult<Vec<_>>>()?;
        entries.sort_by(|left, right| {
            left.file_name()
                .as_bytes()
                .cmp(right.file_name().as_bytes())
        });

        let mut page = Vec::with_capacity(limit);
        let mut has_more = false;
        for entry in entries {
            let name = entry.file_name();
            if !request.cursor.is_empty() && name.as_bytes() <= request.cursor.as_slice() {
                continue;
            }
            if page.len() == limit {
                has_more = true;
                break;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                FileServiceError::io(format!("cannot stat {}", path.display()), error)
            })?;
            page.push(file_metadata(&path, &metadata));
        }
        let next_cursor = if has_more {
            page.last()
                .map(|entry| entry.name.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(FileListResponse {
            entries: page,
            next_cursor,
        })
    }

    pub fn begin_upload(&self, request: BeginUploadRequest) -> FileResult<UploadStatusResponse> {
        validate_transfer_id(&request.transfer_id)?;
        validate_digest(&request.sha256, true)?;
        let target = self.resolve_new(&request.path)?;
        let temporary = upload_temporary_path(&target, &request.transfer_id)?;
        let mode = request.mode & 0o777;

        let mut uploads = self.lock_uploads()?;
        if let Some(existing) = uploads.get(&request.transfer_id) {
            if existing.target != target
                || existing.size != request.size
                || existing.expected_sha256 != request.sha256
            {
                return Err(FileServiceError::new(
                    "conflict",
                    "transfer ID is already associated with a different upload",
                ));
            }
            return Ok(existing.status());
        }
        if uploads.len() >= MAX_TRACKED_UPLOADS {
            uploads.retain(|_, upload| upload.state == UploadState::Active);
            if uploads.len() >= MAX_TRACKED_UPLOADS {
                return Err(FileServiceError::new(
                    "quota",
                    format!("at most {MAX_TRACKED_UPLOADS} uploads may be active"),
                ));
            }
        }

        if target.exists() {
            let target_metadata = fs::metadata(&target).map_err(|error| {
                FileServiceError::io(format!("cannot stat {}", target.display()), error)
            })?;
            if target_metadata.is_file()
                && target_metadata.len() == request.size
                && request.sha256.len() == 32
                && sha256_file(&target)?.as_slice() == request.sha256.as_slice()
            {
                let completed = Upload {
                    transfer_id: request.transfer_id.clone(),
                    requested_path: request.path,
                    target,
                    temporary,
                    size: request.size,
                    expected_sha256: request.sha256,
                    overwrite: request.overwrite,
                    mode,
                    committed_offset: request.size,
                    state: UploadState::Completed,
                };
                let status = completed.status();
                uploads.insert(request.transfer_id, completed);
                return Ok(status);
            }
            if !request.overwrite {
                return Err(FileServiceError::new(
                    "conflict",
                    format!("destination {} already exists", target.display()),
                ));
            }
        }

        let file = open_upload_file(&temporary)?;
        let committed_offset = file
            .metadata()
            .map_err(|error| {
                FileServiceError::io(
                    format!("cannot inspect upload {}", temporary.display()),
                    error,
                )
            })?
            .len();
        if committed_offset > request.size {
            return Err(FileServiceError::new(
                "conflict",
                format!(
                    "temporary upload has {committed_offset} bytes but expected size is {}",
                    request.size
                ),
            ));
        }
        let upload = Upload {
            transfer_id: request.transfer_id.clone(),
            requested_path: request.path,
            target,
            temporary,
            size: request.size,
            expected_sha256: request.sha256,
            overwrite: request.overwrite,
            mode,
            committed_offset,
            state: UploadState::Active,
        };
        let status = upload.status();
        uploads.insert(request.transfer_id, upload);
        Ok(status)
    }

    pub fn query_upload(&self, transfer_id: &str) -> FileResult<UploadStatusResponse> {
        validate_transfer_id(transfer_id)?;
        self.lock_uploads()?
            .get(transfer_id)
            .map(Upload::status)
            .ok_or_else(|| FileServiceError::new("not_found", "upload transfer is unknown"))
    }

    pub fn write_chunk(&self, request: WriteFileChunkRequest) -> FileResult<UploadStatusResponse> {
        validate_transfer_id(&request.transfer_id)?;
        validate_digest(&request.sha256, false)?;
        if request.data.is_empty() {
            return Err(FileServiceError::new("invalid", "file chunk is empty"));
        }
        if request.data.len() > MAX_FILE_CHUNK_SIZE {
            return Err(FileServiceError::new(
                "invalid",
                format!("file chunk exceeds {MAX_FILE_CHUNK_SIZE} bytes"),
            ));
        }
        if Sha256::digest(&request.data).as_slice() != request.sha256.as_slice() {
            return Err(FileServiceError::new(
                "checksum_mismatch",
                "file chunk SHA-256 does not match its payload",
            ));
        }

        let mut uploads = self.lock_uploads()?;
        let upload = uploads
            .get_mut(&request.transfer_id)
            .ok_or_else(|| FileServiceError::new("not_found", "upload transfer is unknown"))?;
        if upload.state != UploadState::Active {
            return if upload.state == UploadState::Completed {
                Ok(upload.status())
            } else {
                Err(FileServiceError::new("conflict", "upload was aborted"))
            };
        }
        let chunk_length = u64::try_from(request.data.len())
            .map_err(|_| FileServiceError::new("invalid", "file chunk is too large"))?;
        let end = request
            .offset
            .checked_add(chunk_length)
            .ok_or_else(|| FileServiceError::new("invalid", "file chunk offset overflow"))?;
        if end > upload.size {
            return Err(FileServiceError::new(
                "invalid",
                "file chunk extends beyond the declared upload size",
            ));
        }

        let file = open_upload_file(&upload.temporary)?;
        if request.offset < upload.committed_offset {
            if end > upload.committed_offset {
                return Err(FileServiceError::new(
                    "conflict",
                    "file chunk partially overlaps the committed prefix",
                ));
            }
            let existing = read_exact_at(&file, request.offset, request.data.len())?;
            if existing != request.data {
                return Err(FileServiceError::new(
                    "conflict",
                    "replayed file chunk differs from committed data",
                ));
            }
            return Ok(upload.status());
        }
        if request.offset != upload.committed_offset {
            return Err(FileServiceError::new(
                "conflict",
                format!(
                    "expected upload offset {}, received {}",
                    upload.committed_offset, request.offset
                ),
            ));
        }
        write_all_at(&file, request.offset, &request.data)?;
        upload.committed_offset = end;
        Ok(upload.status())
    }

    pub fn commit_upload(&self, transfer_id: &str) -> FileResult<UploadStatusResponse> {
        validate_transfer_id(transfer_id)?;
        let mut uploads = self.lock_uploads()?;
        let upload = uploads
            .get_mut(transfer_id)
            .ok_or_else(|| FileServiceError::new("not_found", "upload transfer is unknown"))?;
        if upload.state == UploadState::Completed {
            return Ok(upload.status());
        }
        if upload.state != UploadState::Active {
            return Err(FileServiceError::new("conflict", "upload was aborted"));
        }
        if upload.committed_offset != upload.size {
            return Err(FileServiceError::new(
                "conflict",
                format!(
                    "upload is incomplete: committed {} of {} bytes",
                    upload.committed_offset, upload.size
                ),
            ));
        }
        if upload.expected_sha256.len() == 32
            && sha256_file(&upload.temporary)?.as_slice() != upload.expected_sha256.as_slice()
        {
            return Err(FileServiceError::new(
                "checksum_mismatch",
                "complete upload SHA-256 does not match",
            ));
        }
        if upload.target.exists() && !upload.overwrite {
            return Err(FileServiceError::new(
                "conflict",
                format!("destination {} already exists", upload.target.display()),
            ));
        }
        let file = open_upload_file(&upload.temporary)?;
        file.sync_all().map_err(|error| {
            FileServiceError::io(
                format!("cannot sync upload {}", upload.temporary.display()),
                error,
            )
        })?;
        if upload.mode != 0 {
            fs::set_permissions(&upload.temporary, fs::Permissions::from_mode(upload.mode))
                .map_err(|error| {
                    FileServiceError::io(
                        format!("cannot set permissions on {}", upload.temporary.display()),
                        error,
                    )
                })?;
        }
        if upload.overwrite {
            fs::rename(&upload.temporary, &upload.target).map_err(|error| {
                FileServiceError::io(
                    format!(
                        "cannot commit upload {} to {}",
                        upload.temporary.display(),
                        upload.target.display()
                    ),
                    error,
                )
            })?;
        } else {
            // A same-directory hard link publishes the fully synced inode atomically and fails
            // if the destination appeared after our earlier check. Removing the private name
            // afterwards gives no-clobber semantics without a check-then-rename race.
            fs::hard_link(&upload.temporary, &upload.target).map_err(|error| {
                FileServiceError::io(
                    format!(
                        "cannot commit upload {} to {} without overwriting",
                        upload.temporary.display(),
                        upload.target.display()
                    ),
                    error,
                )
            })?;
            fs::remove_file(&upload.temporary).map_err(|error| {
                FileServiceError::io(
                    format!(
                        "upload was published but temporary name {} could not be removed",
                        upload.temporary.display()
                    ),
                    error,
                )
            })?;
        }
        if let Some(parent) = upload.target.parent()
            && let Ok(directory) = File::open(parent)
        {
            let _ = directory.sync_all();
        }
        upload.state = UploadState::Completed;
        Ok(upload.status())
    }

    pub fn abort_upload(&self, transfer_id: &str) -> FileResult<UploadStatusResponse> {
        validate_transfer_id(transfer_id)?;
        let mut uploads = self.lock_uploads()?;
        let upload = uploads
            .get_mut(transfer_id)
            .ok_or_else(|| FileServiceError::new("not_found", "upload transfer is unknown"))?;
        if upload.state == UploadState::Active {
            match fs::remove_file(&upload.temporary) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(FileServiceError::io(
                        format!("cannot remove upload {}", upload.temporary.display()),
                        error,
                    ));
                }
            }
            upload.state = UploadState::Aborted;
        }
        Ok(upload.status())
    }

    pub fn begin_download(
        &self,
        request: BeginDownloadRequest,
    ) -> FileResult<BeginDownloadResponse> {
        let path = self.resolve_existing(&request.path, true)?;
        let metadata = fs::metadata(&path).map_err(|error| {
            FileServiceError::io(format!("cannot stat {}", path.display()), error)
        })?;
        if !metadata.is_file() {
            return Err(FileServiceError::new(
                "invalid",
                format!("{} is not a regular file", path.display()),
            ));
        }
        Ok(BeginDownloadResponse {
            metadata: Some(file_metadata(&path, &metadata)),
            snapshot: file_snapshot(&path, &metadata),
            sha256: if request.want_sha256 {
                sha256_file(&path)?.to_vec()
            } else {
                Vec::new()
            },
            max_chunk_size: MAX_FILE_CHUNK_SIZE as u32,
        })
    }

    pub fn read_chunk(&self, request: ReadFileChunkRequest) -> FileResult<FileChunkResponse> {
        if request.snapshot.len() != 32 {
            return Err(FileServiceError::new(
                "invalid",
                "download snapshot must be a SHA-256 value",
            ));
        }
        let length = usize::try_from(request.length)
            .unwrap_or(MAX_FILE_CHUNK_SIZE)
            .min(MAX_FILE_CHUNK_SIZE);
        if length == 0 {
            return Err(FileServiceError::new(
                "invalid",
                "download chunk length is zero",
            ));
        }
        let path = self.resolve_existing(&request.path, true)?;
        let file = File::open(&path).map_err(|error| {
            FileServiceError::io(format!("cannot open {}", path.display()), error)
        })?;
        let metadata = file.metadata().map_err(|error| {
            FileServiceError::io(format!("cannot stat {}", path.display()), error)
        })?;
        if !metadata.is_file() {
            return Err(FileServiceError::new(
                "invalid",
                format!("{} is not a regular file", path.display()),
            ));
        }
        if file_snapshot(&path, &metadata) != request.snapshot {
            return Err(FileServiceError::new(
                "conflict",
                "remote file changed while it was being downloaded",
            ));
        }
        if request.offset > metadata.len() {
            return Err(FileServiceError::new(
                "invalid",
                "download offset is beyond end of file",
            ));
        }
        let remaining = metadata.len() - request.offset;
        let requested = usize::try_from(remaining.min(length as u64)).unwrap_or(length);
        let data = read_up_to_at(&file, request.offset, requested)?;
        let eof = request.offset + data.len() as u64 >= metadata.len();
        Ok(FileChunkResponse {
            offset: request.offset,
            sha256: Sha256::digest(&data).to_vec(),
            data,
            eof,
        })
    }

    pub fn make_directory(&self, raw_path: &[u8]) -> FileResult<()> {
        let path = self.resolve_new(raw_path)?;
        fs::create_dir(&path).map_err(|error| {
            FileServiceError::io(format!("cannot create directory {}", path.display()), error)
        })
    }

    pub fn remove(&self, raw_path: &[u8]) -> FileResult<()> {
        let path = self.resolve_existing(raw_path, false)?;
        if path == *self.root {
            return Err(FileServiceError::new("invalid", "cannot remove file root"));
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            FileServiceError::io(format!("cannot stat {}", path.display()), error)
        })?;
        let result = if metadata.is_dir() {
            fs::remove_dir(&path)
        } else {
            fs::remove_file(&path)
        };
        result.map_err(|error| {
            FileServiceError::io(format!("cannot remove {}", path.display()), error)
        })
    }

    pub fn rename(&self, source: &[u8], destination: &[u8], overwrite: bool) -> FileResult<()> {
        let source = self.resolve_existing(source, false)?;
        let destination = self.resolve_new(destination)?;
        if source == *self.root {
            return Err(FileServiceError::new("invalid", "cannot rename file root"));
        }
        if destination.exists() && !overwrite {
            return Err(FileServiceError::new(
                "conflict",
                format!("destination {} already exists", destination.display()),
            ));
        }
        fs::rename(&source, &destination).map_err(|error| {
            FileServiceError::io(
                format!(
                    "cannot rename {} to {}",
                    source.display(),
                    destination.display()
                ),
                error,
            )
        })
    }

    fn resolve_existing(&self, raw: &[u8], follow_symlinks: bool) -> FileResult<PathBuf> {
        let candidate = self.requested_path(raw)?;
        let resolved = if follow_symlinks || candidate == *self.root {
            candidate.canonicalize().map_err(|error| {
                FileServiceError::io(format!("cannot resolve {}", candidate.display()), error)
            })?
        } else {
            let name = candidate
                .file_name()
                .ok_or_else(|| FileServiceError::new("invalid", "remote path has no file name"))?;
            let parent = candidate.parent().unwrap_or(self.root.as_path());
            let parent = parent.canonicalize().map_err(|error| {
                FileServiceError::io(format!("cannot resolve {}", parent.display()), error)
            })?;
            parent.join(name)
        };
        self.ensure_inside_root(&resolved)?;
        Ok(resolved)
    }

    fn resolve_new(&self, raw: &[u8]) -> FileResult<PathBuf> {
        let candidate = self.requested_path(raw)?;
        let name = candidate
            .file_name()
            .filter(|name| *name != OsStr::new(".") && *name != OsStr::new(".."))
            .ok_or_else(|| FileServiceError::new("invalid", "remote path has no file name"))?;
        let parent = candidate.parent().unwrap_or(self.root.as_path());
        let parent = parent.canonicalize().map_err(|error| {
            FileServiceError::io(format!("cannot resolve {}", parent.display()), error)
        })?;
        self.ensure_inside_root(&parent)?;
        Ok(parent.join(name))
    }

    fn requested_path(&self, raw: &[u8]) -> FileResult<PathBuf> {
        if raw.len() > MAX_REMOTE_PATH_SIZE || raw.contains(&0) {
            return Err(FileServiceError::new("invalid", "remote path is invalid"));
        }
        if raw.is_empty() || raw == b"." || raw == b"~" {
            return Ok(self.root.as_ref().clone());
        }
        let raw = if let Some(remainder) = raw.strip_prefix(b"~/") {
            remainder
        } else {
            raw
        };
        let path = PathBuf::from(OsString::from_vec(raw.to_vec()));
        if path
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(FileServiceError::new(
                "invalid",
                "remote paths must not contain '..'",
            ));
        }
        Ok(if path.is_absolute() {
            path
        } else {
            self.root.join(path)
        })
    }

    fn ensure_inside_root(&self, path: &Path) -> FileResult<()> {
        if path.starts_with(self.root.as_path()) {
            Ok(())
        } else {
            Err(FileServiceError::new(
                "permission_denied",
                format!(
                    "path {} is outside configured file root {}",
                    path.display(),
                    self.root.display()
                ),
            ))
        }
    }

    fn lock_uploads(&self) -> FileResult<std::sync::MutexGuard<'_, HashMap<String, Upload>>> {
        self.uploads
            .lock()
            .map_err(|_| FileServiceError::new("filesystem", "upload registry is poisoned"))
    }
}

fn validate_transfer_id(transfer_id: &str) -> FileResult<()> {
    let parsed = Uuid::parse_str(transfer_id)
        .map_err(|_| FileServiceError::new("invalid", "transfer ID must be a UUID"))?;
    if parsed.to_string() != transfer_id {
        return Err(FileServiceError::new(
            "invalid",
            "transfer ID must use canonical lowercase UUID form",
        ));
    }
    Ok(())
}

fn validate_digest(digest: &[u8], optional: bool) -> FileResult<()> {
    if digest.len() == 32 || (optional && digest.is_empty()) {
        Ok(())
    } else {
        Err(FileServiceError::new(
            "invalid",
            "SHA-256 digest must contain exactly 32 bytes",
        ))
    }
}

fn upload_temporary_path(target: &Path, transfer_id: &str) -> FileResult<PathBuf> {
    let parent = target
        .parent()
        .ok_or_else(|| FileServiceError::new("invalid", "upload destination has no parent"))?;
    Ok(parent.join(format!(".astra-upload-{transfer_id}.part")))
}

fn open_upload_file(path: &Path) -> FileResult<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW);
    let file = options.open(path).map_err(|error| {
        FileServiceError::io(format!("cannot open upload {}", path.display()), error)
    })?;
    let metadata = file.metadata().map_err(|error| {
        FileServiceError::io(format!("cannot inspect upload {}", path.display()), error)
    })?;
    if !metadata.is_file() {
        return Err(FileServiceError::new(
            "invalid",
            format!("upload temporary path {} is not a file", path.display()),
        ));
    }
    Ok(file)
}

fn file_metadata(path: &Path, metadata: &fs::Metadata) -> FileMetadata {
    let file_type = metadata.file_type();
    let kind = if file_type.is_file() {
        FileKind::Regular
    } else if file_type.is_dir() {
        FileKind::Directory
    } else if file_type.is_symlink() {
        FileKind::Symlink
    } else {
        FileKind::Other
    };
    FileMetadata {
        path: path.as_os_str().as_bytes().to_vec(),
        name: path
            .file_name()
            .unwrap_or(path.as_os_str())
            .as_bytes()
            .to_vec(),
        kind: kind as i32,
        size: metadata.len(),
        mode: metadata.mode() & 0o7777,
        modified_unix_ns: metadata
            .mtime()
            .saturating_mul(1_000_000_000)
            .saturating_add(metadata.mtime_nsec()),
    }
}

fn file_snapshot(path: &Path, metadata: &fs::Metadata) -> Vec<u8> {
    let mut digest = Sha256::new();
    digest.update(path.as_os_str().as_bytes());
    digest.update(metadata.dev().to_be_bytes());
    digest.update(metadata.ino().to_be_bytes());
    digest.update(metadata.len().to_be_bytes());
    digest.update(metadata.mtime().to_be_bytes());
    digest.update(metadata.mtime_nsec().to_be_bytes());
    digest.finalize().to_vec()
}

fn sha256_file(path: &Path) -> FileResult<[u8; 32]> {
    let mut file = File::open(path).map_err(|error| {
        FileServiceError::io(
            format!("cannot open {} for checksum", path.display()),
            error,
        )
    })?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let length = file.read(&mut buffer).map_err(|error| {
            FileServiceError::io(
                format!("cannot read {} for checksum", path.display()),
                error,
            )
        })?;
        if length == 0 {
            break;
        }
        digest.update(&buffer[..length]);
    }
    Ok(digest.finalize().into())
}

fn write_all_at(file: &File, offset: u64, data: &[u8]) -> FileResult<()> {
    let mut file = file;
    file.seek(SeekFrom::Start(offset)).map_err(|error| {
        FileServiceError::io(format!("cannot seek upload to offset {offset}"), error)
    })?;
    file.write_all(data).map_err(|error| {
        FileServiceError::io(format!("cannot write upload at offset {offset}"), error)
    })?;
    Ok(())
}

fn read_exact_at(file: &File, offset: u64, length: usize) -> FileResult<Vec<u8>> {
    let mut file = file;
    file.seek(SeekFrom::Start(offset)).map_err(|error| {
        FileServiceError::io(format!("cannot seek upload to offset {offset}"), error)
    })?;
    let mut data = vec![0_u8; length];
    file.read_exact(&mut data).map_err(|error| {
        FileServiceError::io(format!("cannot read upload at offset {offset}"), error)
    })?;
    Ok(data)
}

fn read_up_to_at(file: &File, offset: u64, length: usize) -> FileResult<Vec<u8>> {
    let mut file = file;
    file.seek(SeekFrom::Start(offset)).map_err(|error| {
        FileServiceError::io(format!("cannot seek file to offset {offset}"), error)
    })?;
    let mut data = vec![0_u8; length];
    let mut filled = 0;
    while filled < data.len() {
        let count = file.read(&mut data[filled..]).map_err(|error| {
            FileServiceError::io(format!("cannot read file at offset {offset}"), error)
        })?;
        if count == 0 {
            break;
        }
        filled += count;
    }
    data.truncate(filled);
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(data: &[u8]) -> Vec<u8> {
        Sha256::digest(data).to_vec()
    }

    #[test]
    fn upload_chunks_are_idempotent_and_commit_atomically() {
        let root = tempfile::tempdir().unwrap();
        let service = FileService::new(root.path().to_path_buf()).unwrap();
        let contents = b"persistent upload over reconnect";
        let transfer_id = Uuid::new_v4().to_string();
        let status = service
            .begin_upload(BeginUploadRequest {
                transfer_id: transfer_id.clone(),
                path: b"result.txt".to_vec(),
                size: contents.len() as u64,
                sha256: digest(contents),
                overwrite: false,
                mode: 0o640,
            })
            .unwrap();
        assert_eq!(status.committed_offset, 0);

        let first = &contents[..10];
        let request = WriteFileChunkRequest {
            transfer_id: transfer_id.clone(),
            offset: 0,
            data: first.to_vec(),
            sha256: digest(first),
        };
        assert_eq!(
            service
                .write_chunk(request.clone())
                .unwrap()
                .committed_offset,
            10
        );
        assert_eq!(service.write_chunk(request).unwrap().committed_offset, 10);
        let remainder = &contents[10..];
        service
            .write_chunk(WriteFileChunkRequest {
                transfer_id: transfer_id.clone(),
                offset: 10,
                data: remainder.to_vec(),
                sha256: digest(remainder),
            })
            .unwrap();
        let committed = service.commit_upload(&transfer_id).unwrap();
        assert_eq!(committed.state, "completed");
        assert_eq!(fs::read(root.path().join("result.txt")).unwrap(), contents);
        assert_eq!(
            service.commit_upload(&transfer_id).unwrap().state,
            "completed"
        );
    }

    #[test]
    fn upload_reconstructs_committed_offset_after_service_restart() {
        let root = tempfile::tempdir().unwrap();
        let contents = b"first chunk then reconnect";
        let transfer_id = Uuid::new_v4().to_string();
        let begin = BeginUploadRequest {
            transfer_id: transfer_id.clone(),
            path: b"resumed.txt".to_vec(),
            size: contents.len() as u64,
            sha256: digest(contents),
            overwrite: false,
            mode: 0o600,
        };
        let service = FileService::new(root.path().to_path_buf()).unwrap();
        service.begin_upload(begin.clone()).unwrap();
        service
            .write_chunk(WriteFileChunkRequest {
                transfer_id: transfer_id.clone(),
                offset: 0,
                data: contents[..8].to_vec(),
                sha256: digest(&contents[..8]),
            })
            .unwrap();
        drop(service);

        let restarted = FileService::new(root.path().to_path_buf()).unwrap();
        let status = restarted.begin_upload(begin).unwrap();
        assert_eq!(status.committed_offset, 8);
        restarted
            .write_chunk(WriteFileChunkRequest {
                transfer_id: transfer_id.clone(),
                offset: 8,
                data: contents[8..].to_vec(),
                sha256: digest(&contents[8..]),
            })
            .unwrap();
        assert_eq!(
            restarted.commit_upload(&transfer_id).unwrap().state,
            "completed"
        );
        assert_eq!(fs::read(root.path().join("resumed.txt")).unwrap(), contents);
    }

    #[test]
    fn download_snapshot_rejects_changed_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source.bin");
        fs::write(&path, b"first version").unwrap();
        let service = FileService::new(root.path().to_path_buf()).unwrap();
        let download = service
            .begin_download(BeginDownloadRequest {
                path: b"source.bin".to_vec(),
                want_sha256: true,
            })
            .unwrap();
        fs::write(&path, b"a changed and longer version").unwrap();
        let result = service.read_chunk(ReadFileChunkRequest {
            path: b"source.bin".to_vec(),
            snapshot: download.snapshot,
            offset: 0,
            length: 1024,
        });
        assert_eq!(result.unwrap_err().code, "conflict");
    }

    #[test]
    fn file_paths_cannot_escape_configured_root() {
        let root = tempfile::tempdir().unwrap();
        let service = FileService::new(root.path().to_path_buf()).unwrap();
        assert_eq!(
            service
                .stat(FileStatRequest {
                    path: b"../outside".to_vec(),
                    follow_symlinks: true,
                })
                .unwrap_err()
                .code,
            "invalid"
        );
    }
}

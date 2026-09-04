use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::protocol::{
    BeginDownloadRequest, BeginDownloadResponse, BeginUploadRequest, FileCapabilitiesResponse,
    FileChange, FileChangesResponse, FileChunkResponse, FileKind, FileListRequest,
    FileListResponse, FileMetadata, FileStatRequest, FileStatResponse, GitFileStatus,
    GitStatusRequest, GitStatusResponse, ReadFileChunkRequest, UploadStatusResponse,
    WatchFilesRequest, WriteFileChunkRequest,
};
use crate::resources::{
    QuotaExceeded, ResourceAccount, ResourceClaim, ResourcePolicy, ResourceReservation,
};

pub const FILE_PROTOCOL_VERSION: u32 = 1;
pub const MAX_FILE_CHUNK_SIZE: usize = 1024 * 1024;
const MAX_DIRECTORY_PAGE_SIZE: usize = 500;
const MAX_REMOTE_PATH_SIZE: usize = 16 * 1024;
const MAX_GIT_STATUS_SIZE: usize = 4 * 1024 * 1024;
const MAX_GIT_STATUS_ENTRIES: usize = 20_000;
const MAX_WATCHED_FILES: usize = 128;
const MAX_RECURSIVE_WATCH_ROOTS: usize = 8;
const MAX_PENDING_FILE_EVENTS: usize = 4_096;
const MAX_FILE_CHANGES_PER_EVENT: usize = 1_024;
const FILE_CHANGE_COALESCE_DELAY: Duration = Duration::from_millis(75);

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

    fn quota(error: QuotaExceeded) -> Self {
        Self::new("quota", error.to_string())
    }
}

impl fmt::Display for FileServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FileServiceError {}

pub type FileResult<T> = std::result::Result<T, FileServiceError>;

pub struct FileChangeSubscription {
    _watcher: RecommendedWatcher,
    receiver: mpsc::Receiver<notify::Result<Event>>,
    requested_paths: HashMap<PathBuf, Vec<u8>>,
    recursive_roots: Vec<RecursiveWatchRoot>,
    rescan_required: Arc<AtomicBool>,
}

struct RecursiveWatchRoot {
    resolved: PathBuf,
    requested: PathBuf,
}

impl FileChangeSubscription {
    pub async fn next(&mut self) -> FileResult<FileChangesResponse> {
        loop {
            let event = self.receiver.recv().await.ok_or_else(|| {
                FileServiceError::new("watch", "filesystem watcher stopped unexpectedly")
            })?;
            let mut events = vec![event];
            tokio::time::sleep(FILE_CHANGE_COALESCE_DELAY).await;
            while let Ok(event) = self.receiver.try_recv() {
                events.push(event);
                if events.len() >= MAX_FILE_CHANGES_PER_EVENT {
                    break;
                }
            }

            let mut response = self.coalesce(events);
            if self.rescan_required.swap(false, Ordering::AcqRel) {
                response.rescan_required = true;
            }
            if response.rescan_required || !response.changes.is_empty() {
                return Ok(response);
            }
        }
    }

    fn coalesce(&self, events: Vec<notify::Result<Event>>) -> FileChangesResponse {
        let mut changes = HashMap::<Vec<u8>, String>::new();
        let mut rescan_required = false;
        for event in events {
            let event = match event {
                Ok(event) => event,
                Err(_) => {
                    rescan_required = true;
                    continue;
                }
            };
            let Some(kind) = file_change_kind(&event.kind) else {
                continue;
            };
            if kind == "rescan" {
                rescan_required = true;
            }
            for path in event.paths {
                if let Some(requested_path) = self.requested_paths.get(&path) {
                    changes.insert(requested_path.clone(), kind.into());
                }
                for root in &self.recursive_roots {
                    let Ok(relative) = path.strip_prefix(&root.resolved) else {
                        continue;
                    };
                    let reported = if relative.as_os_str().is_empty() {
                        root.requested.clone()
                    } else {
                        root.requested.join(relative)
                    };
                    changes.insert(reported.as_os_str().as_bytes().to_vec(), kind.into());
                }
            }
        }
        let mut changes = changes
            .into_iter()
            .map(|(path, kind)| FileChange { path, kind })
            .collect::<Vec<_>>();
        changes.sort_by(|left, right| left.path.cmp(&right.path));
        FileChangesResponse {
            changes,
            rescan_required,
        }
    }
}

fn file_change_kind(kind: &EventKind) -> Option<&'static str> {
    match kind {
        EventKind::Access(_) => None,
        EventKind::Create(_) => Some("created"),
        EventKind::Modify(_) => Some("modified"),
        EventKind::Remove(_) => Some("removed"),
        EventKind::Any | EventKind::Other => Some("rescan"),
    }
}

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
    _resources: Option<ResourceReservation>,
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
    resources: ResourceAccount,
}

impl FileService {
    pub fn new(root: PathBuf) -> FileResult<Self> {
        let policy = ResourcePolicy::default();
        let resources = ResourceAccount::standalone("file service", policy.user)
            .map_err(|error| FileServiceError::new("quota", error.to_string()))?;
        Self::with_resources(root, resources)
    }

    pub fn with_resources(root: PathBuf, resources: ResourceAccount) -> FileResult<Self> {
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
            resources,
        })
    }

    pub fn capabilities(&self) -> FileCapabilitiesResponse {
        FileCapabilitiesResponse {
            version: FILE_PROTOCOL_VERSION,
            max_chunk_size: MAX_FILE_CHUNK_SIZE as u32,
            resumable_uploads: true,
            atomic_upload_commit: true,
            chunk_sha256: true,
            file_watch_events: true,
            recursive_file_watch_events: true,
        }
    }

    pub fn watch_files(&self, request: WatchFilesRequest) -> FileResult<FileChangeSubscription> {
        if request.paths.is_empty() {
            return Err(FileServiceError::new(
                "invalid",
                "at least one file path is required",
            ));
        }
        if request.paths.len() > MAX_WATCHED_FILES {
            return Err(FileServiceError::new(
                "quota",
                format!("at most {MAX_WATCHED_FILES} files may be watched"),
            ));
        }
        if request.recursive && request.paths.len() > MAX_RECURSIVE_WATCH_ROOTS {
            return Err(FileServiceError::new(
                "quota",
                format!("at most {MAX_RECURSIVE_WATCH_ROOTS} recursive roots may be watched"),
            ));
        }

        let (sender, receiver) = mpsc::channel(MAX_PENDING_FILE_EVENTS);
        let rescan_required = Arc::new(AtomicBool::new(false));
        let callback_rescan_required = Arc::clone(&rescan_required);
        let mut watcher = notify::recommended_watcher(move |event| {
            if let Err(error) = sender.try_send(event) {
                if matches!(error, mpsc::error::TrySendError::Full(_)) {
                    callback_rescan_required.store(true, Ordering::Release);
                }
            }
        })
        .map_err(|error| FileServiceError::new("watch", error.to_string()))?;
        let mut watched_directories = HashSet::new();
        let mut requested_paths = HashMap::new();
        let mut recursive_roots = Vec::new();
        for raw_path in request.paths {
            let path = self.resolve_existing(&raw_path, request.recursive)?;
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                FileServiceError::io(format!("cannot inspect {}", path.display()), error)
            })?;
            if request.recursive {
                if !metadata.is_dir() {
                    return Err(FileServiceError::new(
                        "invalid",
                        format!("{} is not a directory", path.display()),
                    ));
                }
                if watched_directories.insert(path.clone()) {
                    watcher
                        .watch(&path, RecursiveMode::Recursive)
                        .map_err(|error| FileServiceError::new("watch", error.to_string()))?;
                }
                recursive_roots.push(RecursiveWatchRoot {
                    resolved: path,
                    requested: PathBuf::from(OsString::from_vec(raw_path)),
                });
            } else {
                if metadata.is_dir() {
                    return Err(FileServiceError::new(
                        "invalid",
                        format!("{} is a directory", path.display()),
                    ));
                }
                let parent = path.parent().unwrap_or(self.root.as_path()).to_path_buf();
                if watched_directories.insert(parent.clone()) {
                    watcher
                        .watch(&parent, RecursiveMode::NonRecursive)
                        .map_err(|error| FileServiceError::new("watch", error.to_string()))?;
                }
                requested_paths.insert(path, raw_path);
            }
        }
        Ok(FileChangeSubscription {
            _watcher: watcher,
            receiver,
            requested_paths,
            recursive_roots,
            rescan_required,
        })
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

    pub async fn git_status(&self, request: GitStatusRequest) -> FileResult<GitStatusResponse> {
        let project = self.resolve_existing(&request.path, true)?;
        if !project.is_dir() {
            return Err(FileServiceError::new(
                "invalid",
                format!("{} is not a directory", project.display()),
            ));
        }

        let root_output = run_git(&project, &["rev-parse", "--show-toplevel"]).await?;
        let root_bytes = trim_ascii_whitespace(&root_output.stdout);
        let repository = PathBuf::from(OsString::from_vec(root_bytes.to_vec()))
            .canonicalize()
            .map_err(|error| FileServiceError::io("cannot resolve Git repository root", error))?;
        let relative_root = repository.strip_prefix(self.root.as_path()).map_err(|_| {
            FileServiceError::new(
                "permission_denied",
                "Git repository is outside the file root",
            )
        })?;
        let repository_root = if relative_root.as_os_str().is_empty() {
            b".".to_vec()
        } else {
            relative_root.as_os_str().as_bytes().to_vec()
        };

        let status_output = run_git(
            &project,
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=all",
                "--",
                ".",
            ],
        )
        .await?;
        if status_output.stdout.len() > MAX_GIT_STATUS_SIZE {
            return Err(FileServiceError::new(
                "quota",
                "Git status output exceeds 4 MiB",
            ));
        }
        parse_git_status(repository_root, &status_output.stdout)
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
        uploads.retain(|_, upload| upload.state == UploadState::Active);

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
                    _resources: None,
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

        let resources = self
            .resources
            .reserve(ResourceClaim::upload(request.size))
            .map_err(FileServiceError::quota)?;

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
            _resources: Some(resources),
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
        upload._resources.take();
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
            upload._resources.take();
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

async fn run_git(directory: &Path, arguments: &[&str]) -> FileResult<std::process::Output> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .map_err(|_| FileServiceError::new("timeout", "Git status exceeded 5 seconds"))?
        .map_err(|error| FileServiceError::io("cannot execute Git", error))?;
    if output.status.success() {
        return Ok(output);
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let code = if output.status.code() == Some(128) {
        "not_found"
    } else {
        "git"
    };
    Err(FileServiceError::new(
        code,
        if message.is_empty() {
            "Git command failed".into()
        } else {
            message
        },
    ))
}

fn parse_git_status(repository_root: Vec<u8>, output: &[u8]) -> FileResult<GitStatusResponse> {
    let records = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut response = GitStatusResponse {
        repository_root,
        branch: String::new(),
        detached: false,
        ahead: 0,
        behind: 0,
        files: Vec::new(),
    };
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.head ") {
            if value == b"(detached)" {
                response.detached = true;
            } else {
                response.branch = String::from_utf8_lossy(value).into_owned();
            }
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.ab ") {
            let fields = value.split(|byte| *byte == b' ').collect::<Vec<_>>();
            for field in fields {
                if let Some(ahead) = field.strip_prefix(b"+") {
                    response.ahead = parse_git_count(ahead);
                } else if let Some(behind) = field.strip_prefix(b"-") {
                    response.behind = parse_git_count(behind);
                }
            }
            continue;
        }

        let parsed = if record.starts_with(b"1 ") {
            parse_tracked_git_record(record, 9, Vec::new())
        } else if record.starts_with(b"2 ") {
            let original_path = if let Some(original_path) = records.get(index) {
                index += 1;
                original_path.to_vec()
            } else {
                Vec::new()
            };
            parse_tracked_git_record(record, 10, original_path)
        } else if record.starts_with(b"u ") {
            parse_tracked_git_record(record, 11, Vec::new())
        } else if let Some(path) = record.strip_prefix(b"? ") {
            Some(GitFileStatus {
                path: path.to_vec(),
                index_status: "?".into(),
                worktree_status: "?".into(),
                original_path: Vec::new(),
            })
        } else {
            None
        };
        if let Some(file) = parsed {
            response.files.push(file);
            if response.files.len() > MAX_GIT_STATUS_ENTRIES {
                return Err(FileServiceError::new(
                    "quota",
                    format!("Git status exceeds {MAX_GIT_STATUS_ENTRIES} entries"),
                ));
            }
        }
    }
    Ok(response)
}

fn parse_tracked_git_record(
    record: &[u8],
    field_count: usize,
    original_path: Vec<u8>,
) -> Option<GitFileStatus> {
    let fields = record
        .splitn(field_count, |byte| *byte == b' ')
        .collect::<Vec<_>>();
    let status = *fields.get(1)?;
    let path = fields.last()?.to_vec();
    Some(GitFileStatus {
        path,
        index_status: status.first().copied().map(char::from)?.to_string(),
        worktree_status: status.get(1).copied().map(char::from)?.to_string(),
        original_path,
    })
}

fn parse_git_count(value: &[u8]) -> u32 {
    String::from_utf8_lossy(value).parse().unwrap_or(0)
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
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
    fn capabilities_explicitly_advertise_recursive_file_watch_events() {
        let root = tempfile::tempdir().unwrap();
        let service = FileService::new(root.path().to_path_buf()).unwrap();

        let capabilities = service.capabilities();
        assert!(capabilities.file_watch_events);
        assert!(capabilities.recursive_file_watch_events);
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
    fn upload_count_and_bytes_are_reserved_until_abort() {
        let root = tempfile::tempdir().unwrap();
        let mut policy = ResourcePolicy::default();
        policy.user.uploads = 1;
        policy.user.upload_bytes = 4;
        let resources = ResourceAccount::standalone("test user", policy.user).unwrap();
        let service = FileService::with_resources(root.path().to_path_buf(), resources).unwrap();
        let first_id = Uuid::new_v4().to_string();
        service
            .begin_upload(BeginUploadRequest {
                transfer_id: first_id.clone(),
                path: b"first.bin".to_vec(),
                size: 4,
                sha256: Vec::new(),
                overwrite: false,
                mode: 0o600,
            })
            .unwrap();
        let second_id = Uuid::new_v4().to_string();
        let error = service
            .begin_upload(BeginUploadRequest {
                transfer_id: second_id.clone(),
                path: b"second.bin".to_vec(),
                size: 1,
                sha256: Vec::new(),
                overwrite: false,
                mode: 0o600,
            })
            .unwrap_err();
        assert_eq!(error.code, "quota");
        service.abort_upload(&first_id).unwrap();
        assert!(
            service
                .begin_upload(BeginUploadRequest {
                    transfer_id: second_id,
                    path: b"second.bin".to_vec(),
                    size: 1,
                    sha256: Vec::new(),
                    overwrite: false,
                    mode: 0o600,
                })
                .is_ok()
        );
    }

    #[test]
    fn upload_byte_quota_fails_before_creating_a_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        let mut policy = ResourcePolicy::default();
        policy.user.upload_bytes = 3;
        let resources = ResourceAccount::standalone("test user", policy.user).unwrap();
        let service = FileService::with_resources(root.path().to_path_buf(), resources).unwrap();
        let transfer_id = Uuid::new_v4().to_string();
        let error = service
            .begin_upload(BeginUploadRequest {
                transfer_id: transfer_id.clone(),
                path: b"too-large.bin".to_vec(),
                size: 4,
                sha256: Vec::new(),
                overwrite: false,
                mode: 0o600,
            })
            .unwrap_err();
        assert_eq!(error.code, "quota");
        assert!(
            !root
                .path()
                .join(format!(".astra-upload-{transfer_id}.part"))
                .exists()
        );
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

    #[tokio::test]
    async fn watches_open_files_without_recursively_watching_the_project() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("Sources")).unwrap();
        fs::write(root.path().join("Sources/App.swift"), b"first").unwrap();
        fs::write(root.path().join("Sources/Other.swift"), b"ignored").unwrap();
        let service = FileService::new(root.path().to_path_buf()).unwrap();
        let mut subscription = service
            .watch_files(WatchFilesRequest {
                paths: vec![b"Sources/App.swift".to_vec()],
                recursive: false,
            })
            .unwrap();

        fs::write(root.path().join("Sources/Other.swift"), b"still ignored").unwrap();
        fs::write(root.path().join("Sources/App.swift"), b"second").unwrap();
        let changes = tokio::time::timeout(Duration::from_secs(3), subscription.next())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(changes.changes.len(), 1);
        assert_eq!(changes.changes[0].path, b"Sources/App.swift");
        assert!(matches!(
            changes.changes[0].kind.as_str(),
            "modified" | "created"
        ));
    }

    #[tokio::test]
    async fn recursively_watches_directory_entries_and_reports_exact_changed_paths() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("Sources")).unwrap();
        fs::create_dir(root.path().join("Sources/Nested")).unwrap();
        let service = FileService::new(root.path().to_path_buf()).unwrap();
        let mut subscription = service
            .watch_files(WatchFilesRequest {
                paths: vec![b"Sources".to_vec()],
                recursive: true,
            })
            .unwrap();

        let changed_path = root.path().join("Sources/Nested/New.swift");
        fs::write(&changed_path, b"new").unwrap();
        let changed_path = changed_path.canonicalize().unwrap();
        let reported_path = b"Sources/Nested/New.swift";
        let changes = tokio::time::timeout(Duration::from_secs(3), subscription.next())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(changes.changes.len(), 1);
        assert_eq!(changes.changes[0].path, reported_path);
        assert!(matches!(
            changes.changes[0].kind.as_str(),
            "modified" | "created"
        ));

        tokio::time::sleep(Duration::from_millis(200)).await;
        while subscription.receiver.try_recv().is_ok() {}
        fs::remove_file(&changed_path).unwrap();
        let changes = tokio::time::timeout(Duration::from_secs(3), subscription.next())
            .await
            .unwrap()
            .unwrap();
        assert!(changes.changes.iter().any(|change| {
            change.path == reported_path && matches!(change.kind.as_str(), "removed" | "modified")
        }));
    }

    #[test]
    fn parses_porcelain_v2_branch_counts_and_file_states() {
        let output = b"# branch.oid abcdef\0# branch.head main\0# branch.ab +2 -3\0\
1 .M N... 100644 100644 100644 abc abc src/main.rs\0\
? notes.txt\0\
2 R. N... 100644 100644 100644 abc def R100 src/new.rs\0src/old.rs\0";
        let status = parse_git_status(b"project".to_vec(), output).unwrap();

        assert_eq!(status.repository_root, b"project");
        assert_eq!(status.branch, "main");
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 3);
        assert_eq!(status.files.len(), 3);
        assert_eq!(status.files[0].path, b"src/main.rs");
        assert_eq!(status.files[0].worktree_status, "M");
        assert_eq!(status.files[1].index_status, "?");
        assert_eq!(status.files[2].path, b"src/new.rs");
        assert_eq!(status.files[2].original_path, b"src/old.rs");
    }
}

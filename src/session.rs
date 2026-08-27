use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use prost::Message;
use uuid::Uuid;

use crate::{
    protocol::{
        AttachmentInfo, AttachmentRole, AttachmentState, SessionCatalog, SpawnRequest,
        TerminalInfo, WorkspaceInfo,
    },
    resources::{ResourceAccount, ResourceClaim, ResourcePolicy, ResourceReservation},
    terminal::{Terminal, TerminalManager},
};

const SESSION_CATALOG_SCHEMA_VERSION: u32 = 1;
const MAX_SESSION_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_WORKSPACE_NAME_BYTES: usize = 128;

#[derive(Clone)]
pub struct SessionManager {
    terminals: TerminalManager,
    catalog: Arc<RwLock<SessionCatalog>>,
    catalog_path: Arc<PathBuf>,
    attachments: Arc<RwLock<HashMap<String, AttachmentInfo>>>,
    resources: ResourceAccount,
    resource_policy: ResourcePolicy,
}

pub struct ActiveAttachment {
    manager: SessionManager,
    info: AttachmentInfo,
    _resources: ResourceReservation,
}

impl ActiveAttachment {
    pub fn info(&self) -> AttachmentInfo {
        self.info.clone()
    }

    pub fn set_state(&mut self, state: AttachmentState) -> Result<()> {
        self.manager.set_attachment_state(&self.info.id, state)?;
        self.info.state = state as i32;
        Ok(())
    }
}

impl Drop for ActiveAttachment {
    fn drop(&mut self) {
        self.manager
            .attachments
            .write()
            .expect("attachment registry poisoned")
            .remove(&self.info.id);
    }
}

impl SessionManager {
    pub fn new(session_root: PathBuf, catalog_path: PathBuf) -> Result<Self> {
        let policy = ResourcePolicy::default();
        let resources = ResourceAccount::standalone("session", policy.user)?;
        Self::with_resources(session_root, catalog_path, resources, policy)
    }

    pub fn with_resources(
        session_root: PathBuf,
        catalog_path: PathBuf,
        resources: ResourceAccount,
        resource_policy: ResourcePolicy,
    ) -> Result<Self> {
        resource_policy.validate()?;
        let terminals = TerminalManager::with_resources(
            session_root,
            resources.clone(),
            resource_policy.clone(),
        )?;
        let catalog = load_or_create_catalog(&catalog_path)?;
        Ok(Self {
            terminals,
            catalog: Arc::new(RwLock::new(catalog)),
            catalog_path: Arc::new(catalog_path),
            attachments: Arc::new(RwLock::new(HashMap::new())),
            resources,
            resource_policy,
        })
    }

    pub fn resource_account(&self) -> ResourceAccount {
        self.resources.clone()
    }

    pub fn resource_policy(&self) -> &ResourcePolicy {
        &self.resource_policy
    }

    pub fn session_root(&self) -> &Path {
        self.terminals.session_root()
    }

    pub fn has_active_terminals(&self) -> bool {
        self.terminals.has_active_terminals()
    }

    pub fn default_workspace_id(&self) -> String {
        self.catalog
            .read()
            .expect("session catalog poisoned")
            .default_workspace_id
            .clone()
    }

    pub fn list_workspaces(&self) -> Vec<WorkspaceInfo> {
        let mut workspaces = self
            .catalog
            .read()
            .expect("session catalog poisoned")
            .workspaces
            .clone();
        workspaces.sort_by_key(|workspace| (workspace.created_at_unix_ms, workspace.id.clone()));
        workspaces
    }

    pub fn create_workspace(&self, name: &str) -> Result<WorkspaceInfo> {
        let name = validate_workspace_name(name)?;
        let workspace = WorkspaceInfo {
            id: Uuid::new_v4().to_string(),
            name,
            revision: 1,
            created_at_unix_ms: now_unix_ms()?,
            is_default: false,
        };
        self.update_catalog(|catalog| {
            ensure!(
                !catalog
                    .workspaces
                    .iter()
                    .any(|existing| existing.name == workspace.name),
                "workspace name already exists"
            );
            catalog.workspaces.push(workspace.clone());
            Ok(())
        })?;
        Ok(workspace)
    }

    pub fn rename_workspace(&self, workspace_id: &str, name: &str) -> Result<WorkspaceInfo> {
        validate_uuid(workspace_id, "workspace ID")?;
        let name = validate_workspace_name(name)?;
        let mut renamed = None;
        self.update_catalog(|catalog| {
            ensure!(
                !catalog
                    .workspaces
                    .iter()
                    .any(|workspace| { workspace.id != workspace_id && workspace.name == name }),
                "workspace name already exists"
            );
            let workspace = catalog
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == workspace_id)
                .context("workspace does not exist")?;
            workspace.name = name.clone();
            workspace.revision = workspace
                .revision
                .checked_add(1)
                .context("workspace revision exhausted")?;
            renamed = Some(workspace.clone());
            Ok(())
        })?;
        Ok(renamed.expect("renamed workspace must exist"))
    }

    pub fn delete_workspace(&self, workspace_id: &str) -> Result<()> {
        validate_uuid(workspace_id, "workspace ID")?;
        ensure!(
            self.terminals
                .list_in_workspace(workspace_id, true)
                .is_empty(),
            "workspace is not empty"
        );
        self.update_catalog(|catalog| {
            ensure!(
                catalog.default_workspace_id != workspace_id,
                "cannot delete the default workspace"
            );
            let original = catalog.workspaces.len();
            catalog
                .workspaces
                .retain(|workspace| workspace.id != workspace_id);
            ensure!(
                catalog.workspaces.len() + 1 == original,
                "workspace does not exist"
            );
            Ok(())
        })
    }

    pub fn workspace(&self, workspace_id: &str) -> Result<WorkspaceInfo> {
        validate_uuid(workspace_id, "workspace ID")?;
        self.catalog
            .read()
            .expect("session catalog poisoned")
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .cloned()
            .context("workspace does not exist")
    }

    pub fn list_legacy_terminals(&self) -> Vec<TerminalInfo> {
        self.terminals.list()
    }

    pub fn list_terminals(
        &self,
        workspace_id: &str,
        include_exited: bool,
    ) -> Result<Vec<TerminalInfo>> {
        self.workspace(workspace_id)?;
        Ok(self
            .terminals
            .list_in_workspace(workspace_id, include_exited))
    }

    pub fn spawn(&self, mut request: SpawnRequest, formal: bool) -> Result<Arc<Terminal>> {
        if formal {
            self.workspace(&request.workspace_id)?;
        } else if request.workspace_id.is_empty() {
            request.workspace_id = self.default_workspace_id();
        } else {
            self.workspace(&request.workspace_id)?;
        }
        self.terminals.spawn(request)
    }

    pub fn get_terminal(
        &self,
        workspace_id: &str,
        selector: &str,
        formal: bool,
    ) -> Result<Option<Arc<Terminal>>> {
        let terminal = self.terminals.get(selector);
        if !formal {
            return Ok(terminal);
        }
        validate_uuid(selector, "terminal ID")?;
        self.workspace(workspace_id)?;
        Ok(terminal.filter(|terminal| terminal.info().workspace_id == workspace_id))
    }

    pub fn register_attachment(
        &self,
        connection_id: &str,
        terminal: &TerminalInfo,
        read_only: bool,
    ) -> Result<ActiveAttachment> {
        validate_uuid(connection_id, "connection ID")?;
        self.workspace(&terminal.workspace_id)?;
        let resources = self.resources.reserve(ResourceClaim::attachment())?;
        let info = AttachmentInfo {
            id: Uuid::new_v4().to_string(),
            connection_id: connection_id.to_owned(),
            workspace_id: terminal.workspace_id.clone(),
            terminal_id: terminal.id.clone(),
            role: if read_only {
                AttachmentRole::Viewer as i32
            } else {
                AttachmentRole::Controller as i32
            },
            state: AttachmentState::Subscribing as i32,
            created_at_unix_ms: now_unix_ms()?,
        };
        self.attachments
            .write()
            .expect("attachment registry poisoned")
            .insert(info.id.clone(), info.clone());
        Ok(ActiveAttachment {
            manager: self.clone(),
            info,
            _resources: resources,
        })
    }

    pub fn list_attachments(
        &self,
        workspace_id: &str,
        terminal_id: &str,
    ) -> Result<Vec<AttachmentInfo>> {
        self.workspace(workspace_id)?;
        if !terminal_id.is_empty() {
            validate_uuid(terminal_id, "terminal ID")?;
            let Some(terminal) = self.terminals.get(terminal_id) else {
                bail!("terminal does not exist")
            };
            ensure!(
                terminal.info().workspace_id == workspace_id,
                "terminal does not belong to workspace"
            );
        }
        let mut attachments: Vec<_> = self
            .attachments
            .read()
            .expect("attachment registry poisoned")
            .values()
            .filter(|attachment| {
                attachment.workspace_id == workspace_id
                    && (terminal_id.is_empty() || attachment.terminal_id == terminal_id)
            })
            .cloned()
            .collect();
        attachments
            .sort_by_key(|attachment| (attachment.created_at_unix_ms, attachment.id.clone()));
        Ok(attachments)
    }

    fn set_attachment_state(&self, attachment_id: &str, state: AttachmentState) -> Result<()> {
        let mut attachments = self
            .attachments
            .write()
            .expect("attachment registry poisoned");
        let attachment = attachments
            .get_mut(attachment_id)
            .context("attachment is no longer active")?;
        let current = AttachmentState::try_from(attachment.state)
            .map_err(|_| anyhow::anyhow!("attachment has invalid state"))?;
        ensure!(
            matches!(
                (current, state),
                (AttachmentState::Subscribing, AttachmentState::Snapshotting)
                    | (AttachmentState::Snapshotting, AttachmentState::Live)
                    | (AttachmentState::Live, AttachmentState::Snapshotting)
            ),
            "invalid attachment state transition"
        );
        attachment.state = state as i32;
        Ok(())
    }

    fn update_catalog(&self, update: impl FnOnce(&mut SessionCatalog) -> Result<()>) -> Result<()> {
        let mut catalog = self.catalog.write().expect("session catalog poisoned");
        let mut candidate = catalog.clone();
        update(&mut candidate)?;
        validate_catalog(&candidate)?;
        persist_catalog(&self.catalog_path, &candidate)?;
        *catalog = candidate;
        Ok(())
    }
}

fn load_or_create_catalog(path: &Path) -> Result<SessionCatalog> {
    match fs::read(path) {
        Ok(bytes) => {
            ensure!(
                bytes.len() <= MAX_SESSION_CATALOG_BYTES,
                "session catalog exceeds size limit"
            );
            let catalog = SessionCatalog::decode(bytes.as_slice())
                .context("session catalog is not valid protobuf")?;
            validate_catalog(&catalog)?;
            Ok(catalog)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let id = Uuid::new_v4().to_string();
            let catalog = SessionCatalog {
                schema_version: SESSION_CATALOG_SCHEMA_VERSION,
                default_workspace_id: id.clone(),
                workspaces: vec![WorkspaceInfo {
                    id,
                    name: "Default".into(),
                    revision: 1,
                    created_at_unix_ms: now_unix_ms()?,
                    is_default: true,
                }],
            };
            persist_catalog(path, &catalog)?;
            Ok(catalog)
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to read session catalog {}", path.display()))
        }
    }
}

fn validate_catalog(catalog: &SessionCatalog) -> Result<()> {
    ensure!(
        catalog.schema_version == SESSION_CATALOG_SCHEMA_VERSION,
        "unsupported session catalog schema version"
    );
    validate_uuid(&catalog.default_workspace_id, "default workspace ID")?;
    ensure!(
        !catalog.workspaces.is_empty(),
        "session catalog has no workspace"
    );
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    let mut default_count = 0;
    for workspace in &catalog.workspaces {
        validate_uuid(&workspace.id, "workspace ID")?;
        ensure!(ids.insert(workspace.id.clone()), "duplicate workspace ID");
        let name = validate_workspace_name(&workspace.name)?;
        ensure!(name == workspace.name, "workspace name is not canonical");
        ensure!(names.insert(name), "duplicate workspace name");
        ensure!(workspace.revision > 0, "workspace revision must be nonzero");
        if workspace.is_default {
            default_count += 1;
            ensure!(
                workspace.id == catalog.default_workspace_id,
                "default workspace marker does not match catalog"
            );
        }
    }
    ensure!(
        default_count == 1,
        "session catalog must have one default workspace"
    );
    Ok(())
}

fn persist_catalog(path: &Path, catalog: &SessionCatalog) -> Result<()> {
    validate_catalog(catalog)?;
    let encoded = catalog.encode_to_vec();
    ensure!(
        encoded.len() <= MAX_SESSION_CATALOG_BYTES,
        "session catalog exceeds size limit"
    );
    let parent = path.parent().context("session catalog has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".session-catalog-{}.tmp", Uuid::new_v4()));
    let result: Result<()> = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("failed to persist session catalog {}", path.display()))
}

fn validate_workspace_name(name: &str) -> Result<String> {
    let name = name.trim();
    ensure!(!name.is_empty(), "workspace name cannot be empty");
    ensure!(
        name.len() <= MAX_WORKSPACE_NAME_BYTES,
        "workspace name exceeds 128 bytes"
    );
    ensure!(
        !name.chars().any(char::is_control),
        "workspace name contains control characters"
    );
    Ok(name.to_owned())
}

fn validate_uuid(value: &str, label: &str) -> Result<()> {
    let parsed = Uuid::parse_str(value).with_context(|| format!("{label} is not a UUID"))?;
    ensure!(parsed.to_string() == value, "{label} is not canonical");
    Ok(())
}

fn now_unix_ms() -> Result<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(directory: &tempfile::TempDir) -> SessionManager {
        let root = directory.path().join("home");
        fs::create_dir(&root).unwrap();
        SessionManager::new(root, directory.path().join("session-catalog.pb")).unwrap()
    }

    fn manager_with_policy(
        directory: &tempfile::TempDir,
        policy: ResourcePolicy,
    ) -> SessionManager {
        let root = directory.path().join("home");
        fs::create_dir(&root).unwrap();
        let resources = ResourceAccount::standalone("test user", policy.user).unwrap();
        SessionManager::with_resources(
            root,
            directory.path().join("session-catalog.pb"),
            resources,
            policy,
        )
        .unwrap()
    }

    #[test]
    fn workspace_catalog_persists_uuid_revision_and_safe_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager(&directory);
        let default = manager.list_workspaces()[0].clone();
        let created = manager.create_workspace("  Build  ").unwrap();
        assert_eq!(created.name, "Build");
        let renamed = manager.rename_workspace(&created.id, "Deploy").unwrap();
        assert_eq!(renamed.revision, 2);
        assert!(manager.delete_workspace(&default.id).is_err());

        let restored = SessionManager::new(
            directory.path().join("home"),
            directory.path().join("session-catalog.pb"),
        )
        .unwrap();
        assert_eq!(restored.workspace(&created.id).unwrap(), renamed);
        restored.delete_workspace(&created.id).unwrap();
        assert!(restored.workspace(&created.id).is_err());
    }

    #[test]
    fn attachments_have_unique_identity_and_drop_from_active_registry() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager(&directory);
        let workspace_id = manager.default_workspace_id();
        let terminal = TerminalInfo {
            id: Uuid::new_v4().to_string(),
            workspace_id: workspace_id.clone(),
            ..Default::default()
        };
        let connection_id = Uuid::new_v4().to_string();
        let mut first = manager
            .register_attachment(&connection_id, &terminal, true)
            .unwrap();
        let second = manager
            .register_attachment(&connection_id, &terminal, false)
            .unwrap();
        assert_ne!(first.info.id, second.info.id);
        assert!(first.set_state(AttachmentState::Live).is_err());
        first.set_state(AttachmentState::Snapshotting).unwrap();
        first.set_state(AttachmentState::Live).unwrap();
        assert_eq!(
            manager.list_attachments(&workspace_id, "").unwrap().len(),
            2
        );
        drop(first);
        drop(second);
        assert!(
            manager
                .list_attachments(&workspace_id, "")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn attachment_quota_rejects_before_registry_mutation_and_releases_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let mut policy = ResourcePolicy::default();
        policy.user.attachments = 1;
        let manager = manager_with_policy(&directory, policy);
        let workspace_id = manager.default_workspace_id();
        let terminal = TerminalInfo {
            id: Uuid::new_v4().to_string(),
            workspace_id: workspace_id.clone(),
            ..Default::default()
        };
        let connection_id = Uuid::new_v4().to_string();
        let first = manager
            .register_attachment(&connection_id, &terminal, true)
            .unwrap();
        let error = manager
            .register_attachment(&connection_id, &terminal, true)
            .err()
            .expect("second attachment should exceed quota");
        assert!(
            error
                .downcast_ref::<crate::resources::QuotaExceeded>()
                .is_some()
        );
        assert_eq!(
            manager.list_attachments(&workspace_id, "").unwrap().len(),
            1
        );
        drop(first);
        assert!(
            manager
                .register_attachment(&connection_id, &terminal, true)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn terminal_quota_rejects_new_pty_without_ending_the_active_terminal() {
        let directory = tempfile::tempdir().unwrap();
        let mut policy = ResourcePolicy::default();
        policy.user.terminals = 1;
        let manager = manager_with_policy(&directory, policy);
        let request = SpawnRequest {
            argv: vec!["/bin/sleep".into(), "30".into()],
            workspace_id: manager.default_workspace_id(),
            ..Default::default()
        };
        let first = manager.spawn(request.clone(), true).unwrap();
        let error = manager
            .spawn(request, true)
            .err()
            .expect("second terminal should exceed quota");
        assert!(
            error
                .downcast_ref::<crate::resources::QuotaExceeded>()
                .is_some()
        );
        assert_eq!(first.info().status, "running");
        first.kill().unwrap();
    }

    #[test]
    fn formal_terminal_operations_require_canonical_workspace_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager(&directory);
        let missing_workspace = Uuid::new_v4().to_string();
        assert!(
            manager
                .spawn(
                    SpawnRequest {
                        workspace_id: missing_workspace,
                        ..Default::default()
                    },
                    true,
                )
                .is_err()
        );
        assert!(
            manager
                .get_terminal(&manager.default_workspace_id(), "1", true)
                .is_err()
        );
    }

    #[tokio::test]
    async fn terminal_membership_is_workspace_scoped_and_blocks_container_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager(&directory);
        let default_id = manager.default_workspace_id();
        let workspace = manager.create_workspace("Build").unwrap();
        let terminal = manager
            .spawn(
                SpawnRequest {
                    argv: vec!["/bin/cat".into()],
                    workspace_id: workspace.id.clone(),
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        assert!(
            manager
                .list_terminals(&default_id, false)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            manager.list_terminals(&workspace.id, false).unwrap()[0].id,
            terminal.info().id
        );
        assert!(manager.delete_workspace(&workspace.id).is_err());
        assert!(
            manager
                .get_terminal(&default_id, &terminal.info().id, true)
                .unwrap()
                .is_none()
        );
        terminal.kill().unwrap();
    }
}

use std::cell::RefCell;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::config::AppConfig;
use crate::error::{err, RdevError, Result};
use crate::error_info;
use crate::path::RemotePath;
use crate::ssh::{sh_c, RemoteCheck, SshClient, UploadRequest};
use crate::ssh_tar::{upload_tar, TarUpload};
use crate::sync::{SyncBackend, SyncDeltaRequest, SyncReport, SyncRequest};
use crate::sync_output::{console_output, SyncOutput};

pub struct SftpDeltaBackend<'a> {
    config: &'a AppConfig,
    client: RefCell<Option<SshClient>>,
    output: Arc<dyn SyncOutput>,
}

impl<'a> SftpDeltaBackend<'a> {
    pub fn new(config: &'a AppConfig) -> Self {
        Self {
            config,
            client: RefCell::new(None),
            output: console_output(),
        }
    }

    pub(crate) fn with_output(mut self, output: Arc<dyn SyncOutput>) -> Self {
        self.output = output;
        self
    }

    pub fn check_exec(&self) -> Result<()> {
        self.with_client(|client| {
            client.exec_checked(
                &sh_c(":"),
                RemoteCheck::new(error_info::SYNC_SSH_TAR_FAILED),
            )
        })
    }

    fn sync_delta_impl(&self, request: SyncDeltaRequest) -> Result<SyncReport> {
        let remote_root = RemotePath::parse(self.config.remote.path.as_str())?;
        self.output.line(format!(
            "[sync] delta start transport=internal-sftp uploads={} deletes={}",
            request.uploads.len(),
            request.deletes.len()
        ));
        let started = Instant::now();
        self.with_client_retry_if(
            |error| !request.is_cancelled() && should_retry_with_reconnect(error),
            |client| {
                for path in &request.uploads {
                    if request.is_cancelled() {
                        return Err(err(error_info::SYNC_CANCELLED));
                    }
                    let item_started = Instant::now();
                    let local_path = request.project_root.join(path);
                    if !local_path.exists() {
                        self.output.line(format!(
                            "[sync] upload skipped vanished path={}",
                            path.display()
                        ));
                        continue;
                    }
                    let remote_path = remote_path(&remote_root, path);
                    client.upload(UploadRequest {
                        local_path: &local_path,
                        remote_path: Path::new(&remote_path),
                        cancelled: request.cancelled.as_ref(),
                    })?;
                    self.output.line(format!(
                        "[sync] upload ok path={} elapsed_ms={}",
                        path.display(),
                        item_started.elapsed().as_millis()
                    ));
                }
                for path in &request.deletes {
                    if request.is_cancelled() {
                        return Err(err(error_info::SYNC_CANCELLED));
                    }
                    let item_started = Instant::now();
                    let remote_path = remote_path(&remote_root, path);
                    client.remove(Path::new(&remote_path))?;
                    self.output.line(format!(
                        "[sync] delete ok path={} elapsed_ms={}",
                        path.display(),
                        item_started.elapsed().as_millis()
                    ));
                }
                Ok(())
            },
        )?;
        self.output.line(format!(
            "[sync] delta done elapsed_ms={}",
            started.elapsed().as_millis()
        ));
        Ok(SyncReport::completed_sftp(request.uploads, request.deletes))
    }

    fn sync_full_impl(&self, request: SyncRequest) -> Result<SyncReport> {
        let remote_root = RemotePath::parse(self.config.remote.path.as_str())?;
        if request.dry_run {
            self.output
                .line("[sync] full dry-run transport=ssh-tar".to_owned());
            return Ok(SyncReport::completed_ssh_tar(
                self.config.sync.watch_dirs.clone(),
                true,
            ));
        }
        let started = Instant::now();
        self.output
            .line("[sync] full start transport=ssh-tar".to_owned());
        self.with_client_retry_if(
            |error| !request.is_cancelled() && should_retry_with_reconnect(error),
            |client| {
                upload_tar(
                    client,
                    TarUpload {
                        config: self.config,
                        request: &request,
                        remote_root: &remote_root,
                        output: Arc::clone(&self.output),
                    },
                )
            },
        )?;
        self.output.line(format!(
            "[sync] full done transport=ssh-tar elapsed_ms={}",
            started.elapsed().as_millis()
        ));
        Ok(SyncReport::completed_ssh_tar(
            self.config.sync.watch_dirs.clone(),
            request.dry_run,
        ))
    }

    fn warm_up_impl(&self) -> Result<()> {
        let started = Instant::now();
        self.with_client(|_| Ok(()))?;
        self.output.line(format!(
            "[sync] internal-sftp connected elapsed_ms={}",
            started.elapsed().as_millis()
        ));
        Ok(())
    }

    fn with_client<T>(&self, action: impl FnMut(&mut SshClient) -> Result<T>) -> Result<T> {
        self.with_client_retry_if(should_retry_with_reconnect, action)
    }

    fn with_client_retry_if<T>(
        &self,
        should_retry: impl Fn(&RdevError) -> bool,
        mut action: impl FnMut(&mut SshClient) -> Result<T>,
    ) -> Result<T> {
        match self.with_client_once(&mut action) {
            Ok(value) => Ok(value),
            Err(error) if should_retry(&error) => {
                let first_error = error.to_string();
                self.output.line(format!(
                    "[sync] ssh connection may be stale; reconnecting once after {first_error}"
                ));
                self.client.replace(None);
                match self.with_client_once(&mut action) {
                    Ok(value) => Ok(value),
                    Err(retry_error) => {
                        self.client.replace(None);
                        Err(retry_error.with_hint(format!(
                            "retry after SSH reconnect failed; first error: {first_error}"
                        )))
                    }
                }
            }
            Err(error) => Err(error),
        }
    }

    fn with_client_once<T>(
        &self,
        action: &mut impl FnMut(&mut SshClient) -> Result<T>,
    ) -> Result<T> {
        if self.client.borrow().is_none() {
            let mut client = SshClient::connect(self.config)?;
            client.set_output(Arc::clone(&self.output));
            self.client.replace(Some(client));
        }
        let mut client = self.client.borrow_mut();
        let Some(client) = client.as_mut() else {
            return Err(err(error_info::INTERNAL_UNEXPECTED));
        };
        action(client)
    }
}
fn should_retry_with_reconnect(error: &RdevError) -> bool {
    matches!(
        error.info,
        error_info::SYNC_SFTP_FAILED
            | error_info::SYNC_SSH_TAR_FAILED
            | error_info::REMOTE_SSH_CONNECT_FAILED
    )
}

impl SyncBackend for SftpDeltaBackend<'_> {
    fn warm_up(&self) -> Result<()> {
        self.warm_up_impl()
    }

    fn sync_full(&self, request: SyncRequest) -> Result<SyncReport> {
        self.sync_full_impl(request)
    }

    fn sync_delta(&self, request: SyncDeltaRequest) -> Result<SyncReport> {
        self.sync_delta_impl(request)
    }
}

fn remote_path(remote_root: &RemotePath, relative_path: &Path) -> String {
    format!(
        "{}/{}",
        remote_root.as_str().trim_end_matches('/'),
        path_to_forward_slashes(relative_path)
    )
}

fn path_to_forward_slashes(path: &Path) -> String {
    path.components()
        .filter_map(|component| {
            let item = component.as_os_str().to_string_lossy();
            if item == "." {
                None
            } else {
                Some(item.to_string())
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::error::err;
    use crate::error_info;
    use crate::path::RemotePath;

    use super::{remote_path, should_retry_with_reconnect};

    #[test]
    fn builds_remote_path_with_forward_slashes() {
        let root = parse_root();

        assert_eq!(
            remote_path(&root, &PathBuf::from("src\\main.rs")),
            "/rdev/project/src/main.rs"
        );
    }

    #[test]
    fn reconnect_retry_only_handles_ssh_sync_errors() {
        assert!(should_retry_with_reconnect(&err(
            error_info::SYNC_SFTP_FAILED
        )));
        assert!(should_retry_with_reconnect(&err(
            error_info::SYNC_SSH_TAR_FAILED
        )));
        assert!(should_retry_with_reconnect(&err(
            error_info::REMOTE_SSH_CONNECT_FAILED
        )));
        assert!(!should_retry_with_reconnect(&err(
            error_info::SYNC_CANCELLED
        )));
        assert!(!should_retry_with_reconnect(&err(
            error_info::CONFIG_INVALID
        )));
    }

    fn parse_root() -> RemotePath {
        match RemotePath::parse("/rdev/project") {
            Ok(root) => root,
            Err(error) => panic!("remote path should parse: {error}"),
        }
    }
}

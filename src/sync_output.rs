use std::sync::Arc;

pub(crate) trait SyncOutput: Send + Sync {
    fn line(&self, line: String);
}

#[derive(Debug)]
pub(crate) struct ConsoleSyncOutput;

impl SyncOutput for ConsoleSyncOutput {
    fn line(&self, line: String) {
        println!("{line}");
    }
}

#[derive(Debug)]
pub(crate) struct SilentSyncOutput;

impl SyncOutput for SilentSyncOutput {
    fn line(&self, _line: String) {}
}

pub(crate) fn console_output() -> Arc<dyn SyncOutput> {
    Arc::new(ConsoleSyncOutput)
}

pub(crate) fn silent_output() -> Arc<dyn SyncOutput> {
    Arc::new(SilentSyncOutput)
}

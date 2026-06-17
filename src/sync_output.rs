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

pub(crate) fn console_output() -> Arc<dyn SyncOutput> {
    Arc::new(ConsoleSyncOutput)
}

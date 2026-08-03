//! An in-memory sink, for tests and for `simulate`.

use std::sync::{Arc, Mutex};

use sequencer_core::emit::{Emit, InputSink, SinkError};

/// A sink that records instead of touching the OS.
///
/// Cloneable and shared, so a test can hold onto the recording while the runner owns the
/// sink.
#[derive(Debug, Clone, Default)]
pub struct MockInjector {
    recorded: Arc<Mutex<Vec<Emit>>>,
    released: Arc<Mutex<u32>>,
}

impl MockInjector {
    /// An empty injector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything emitted so far.
    ///
    /// # Panics
    ///
    /// If a previous holder of the lock panicked.
    #[must_use]
    pub fn recorded(&self) -> Vec<Emit> {
        self.recorded
            .lock()
            .expect("recording lock poisoned")
            .clone()
    }

    /// How many times the runner's drop guard released everything.
    ///
    /// # Panics
    ///
    /// If a previous holder of the lock panicked.
    #[must_use]
    pub fn release_all_calls(&self) -> u32 {
        *self.released.lock().expect("release lock poisoned")
    }
}

impl InputSink for MockInjector {
    fn emit(&mut self, emit: &Emit) -> Result<(), SinkError> {
        self.recorded
            .lock()
            .map_err(|_| SinkError::Disconnected)?
            .push(*emit);
        Ok(())
    }

    fn release_all(&mut self) {
        if let Ok(mut count) = self.released.lock() {
            *count += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sequencer_core::emit::EmitAction;
    use sequencer_core::input::Key;
    use sequencer_core::time::Timestamp;

    #[test]
    fn the_injector_records_and_shares() {
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let emit = Emit {
            at: Timestamp::ZERO,
            action: EmitAction::KeyDown(Key::A),
            level: 0,
        };
        sink.emit(&emit).expect("recording should not fail");
        sink.release_all();

        assert_eq!(watcher.recorded(), vec![emit]);
        assert_eq!(watcher.release_all_calls(), 1);
    }
}

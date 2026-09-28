//! Intent orchestration and stage-to-stage event delivery.

use engine_types::IntentEvent;
use tokio::sync::mpsc;

/// A bounded channel prevents unlimited queued work.
pub fn event_bus(capacity: usize) -> (mpsc::Sender<IntentEvent>, mpsc::Receiver<IntentEvent>) {
    mpsc::channel(capacity)
}

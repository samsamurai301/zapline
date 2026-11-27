//! Durable background jobs, stored in the database and retried on failure.
//! `zapline g job <name>` adds one here and registers it.

use zapline::jobs::JobQueue;

pub mod send_welcome;
// zapline:jobs:modules
/// Registers every job type with the app's queue (`App::jobs(jobs::register)`).
pub fn register(queue: JobQueue) -> JobQueue {
    queue.register::<send_welcome::SendWelcome>()
    // zapline:jobs:register
}

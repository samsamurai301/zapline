//! The `send_welcome` job runs and succeeds.

use std::time::Duration;

use zapline::db::Database;
use zapline::jobs::{JobStats, Jobs};

use saas::app;
use saas::jobs::send_welcome::SendWelcome;

#[tokio::test]
async fn send_welcome_runs() {
    let client = app(Database::sqlite_memory()).test();
    let jobs: Jobs = client.resources().await.require().unwrap();
    // An account that no longer exists: the job finishes without sending.
    jobs.enqueue(SendWelcome { user_id: 42 }).await.unwrap();

    // Run it here (unless a background worker got to it first), then wait
    // until the queue is empty: a failure would show up as a retry.
    for _ in 0..500 {
        jobs.run_pending().await.unwrap();
        let stats = jobs.stats().await.unwrap();
        assert_eq!(stats.failed, 0);
        if stats == JobStats::default() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the job did not finish: {:?}", jobs.stats().await.unwrap());
}

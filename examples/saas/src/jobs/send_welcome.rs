//! The welcome email, sent by a durable job after sign-up: if the SMTP
//! server is down the job is retried with backoff instead of the email
//! being lost.

use zapline::db::sqlx::{self, Pool};
use zapline::db::DefaultDb;
use zapline::jobs::Job;
use zapline::mail::{Email, Mailer};
use zapline::prelude::*;

use crate::routes::auth::app_url;

/// Welcome the user with this id.
#[model]
pub struct SendWelcome {
    pub user_id: i64,
}

impl Job for SendWelcome {
    const NAME: &'static str = "send_welcome";

    async fn run(self, res: Resources) -> Result<()> {
        let db: Pool<DefaultDb> = res.require()?;
        let email: Option<String> = sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
            .bind(self.user_id)
            .fetch_optional(&db)
            .await?;
        // The account was deleted before the job ran: nothing to do.
        let Some(email) = email else {
            return Ok(());
        };
        let link = app_url();
        res.require::<Mailer>()?
            .send(Email::to(email).subject("Welcome to saas").text(format!(
                "Thanks for signing up! Your todos are waiting at {link}/\n"
            )))
            .await
    }
}

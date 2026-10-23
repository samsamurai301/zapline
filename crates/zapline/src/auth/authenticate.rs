//! Pluggable authentication: the [`Authenticator`] trait, the
//! [`Authenticate`] middleware and the [`CurrentUser`] extractor.

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use http::HeaderValue;

use super::guard::{HasRoles, PolicyVerified, RoutePolicy, UserAuthenticated};
use super::{forbidden, missing_credentials};
use crate::extract::FromRequestParts;
use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::{Error, IntoResponse, Request, Response, Result};

/// Turns a request into a user. Implement it for any scheme (sessions, API
/// keys, mTLS, an external identity provider...), or pass a closure
/// `Fn(&Request) -> impl Future<Output = Result<Option<User>>>`.
///
/// Return `Ok(None)` when the request carries no credentials for this
/// scheme, `Ok(Some(user))` when they are valid and `Err(..)` (typically a
/// `401`) when they are present but invalid.
///
/// ```
/// use zapline::auth::{constant_time_eq, Authenticate, Authenticator, HasRoles};
/// use zapline::prelude::*;
///
/// #[derive(Clone)]
/// struct User { name: String, roles: Vec<String> }
///
/// impl HasRoles for User {
///     fn roles(&self) -> &[String] { &self.roles }
/// }
///
/// struct StaticKeys;
///
/// impl Authenticator for StaticKeys {
///     type User = User;
///     async fn authenticate(&self, req: &Request) -> Result<Option<User>> {
///         let Some(key) = req.headers().get("x-api-key") else { return Ok(None) };
///         if constant_time_eq(key.as_bytes(), "k-admin") {
///             return Ok(Some(User { name: "ops".into(), roles: vec!["admin".into()] }));
///         }
///         Err(Error::unauthorized().with_code("invalid_credentials"))
///     }
/// }
///
/// let admin = Routes::new()
///     .get("/stats", || async { "stats" }).require_roles(["admin"])
///     .wrap(Authenticate::new(StaticKeys).authorize());
/// # let _ = App::new().nest("/admin", admin);
/// ```
pub trait Authenticator: Send + Sync + 'static {
    /// The authenticated principal, available to handlers as
    /// [`CurrentUser<User>`](CurrentUser).
    type User: Clone + Send + Sync + 'static;

    /// Authenticate `req`: `Ok(None)` for no credentials, `Ok(Some(user))`
    /// for valid ones, `Err` for invalid ones.
    fn authenticate(
        &self,
        req: &Request,
    ) -> impl Future<Output = Result<Option<Self::User>>> + Send;

    /// The `WWW-Authenticate` challenge sent when authentication is required
    /// but no credentials were given (e.g. `Bearer`).
    fn challenge(&self) -> Option<HeaderValue> {
        None
    }
}

impl<F, Fut, U> Authenticator for F
where
    F: Fn(&Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Option<U>>> + Send,
    U: Clone + Send + Sync + 'static,
{
    type User = U;

    fn authenticate(&self, req: &Request) -> impl Future<Output = Result<Option<U>>> + Send {
        self(req)
    }
}

type AuthorizeFn<U> = fn(&U, &RoutePolicy<'_>) -> bool;

/// Route middleware that authenticates requests with an [`Authenticator`],
/// stores the user for the [`CurrentUser`] extractor and enforces the matched
/// route's policies.
///
/// Install it after routing, on a group with [`Routes::wrap`](crate::Routes::wrap)
/// or app-wide with [`App::wrap_route`](crate::App::wrap_route):
///
/// - Requests without credentials pass through anonymously, unless
///   [`required`](Self::required) is set or the route has a policy
///   (`.require_auth()`, `.require_roles(..)`, `.require_scopes(..)`); then
///   they get `401 missing_credentials`.
/// - Invalid credentials are rejected with the authenticator's error, except
///   on routes marked `.allow_anonymous()`.
/// - With [`authorize`](Self::authorize), role and scope policies are
///   checked (`403 forbidden`). A route with role/scope requirements behind
///   an `Authenticate` without a role checker fails closed with a `500`.
///
/// ```
/// use zapline::auth::{Authenticate, CurrentUser};
/// use zapline::prelude::*;
///
/// // Any closure works as an authenticator.
/// let auth = Authenticate::new(|req: &Request| {
///     let user = req.headers().get("x-user").and_then(|v| v.to_str().ok()).map(str::to_owned);
///     async move { Ok(user) }
/// })
/// .required();
///
/// let api = Routes::new()
///     .get("/me", |CurrentUser(name): CurrentUser<String>| async move { name })
///     .wrap(auth);
/// # let _ = App::new().nest("/api", api);
/// ```
pub struct Authenticate<A: Authenticator> {
    authenticator: Arc<A>,
    required: bool,
    authorize: Option<AuthorizeFn<A::User>>,
    challenge: Option<HeaderValue>,
}

impl<A: Authenticator> Clone for Authenticate<A> {
    fn clone(&self) -> Self {
        Authenticate {
            authenticator: self.authenticator.clone(),
            required: self.required,
            authorize: self.authorize,
            challenge: self.challenge.clone(),
        }
    }
}

impl<A: Authenticator> Authenticate<A> {
    /// Authenticate with `authenticator`. Anonymous requests are allowed
    /// unless the route requires authentication or [`required`](Self::required)
    /// is set.
    pub fn new(authenticator: A) -> Self {
        Authenticate {
            challenge: authenticator.challenge(),
            authenticator: Arc::new(authenticator),
            required: false,
            authorize: None,
        }
    }

    /// Reject unauthenticated requests on every route this wraps (except
    /// routes marked `.allow_anonymous()`).
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Allow unauthenticated requests on routes without a policy (undoes
    /// [`required`](Self::required)).
    pub fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    /// The `WWW-Authenticate` challenge sent with `401` responses.
    ///
    /// # Panics
    /// If `value` is not a valid header value.
    pub fn challenge(mut self, value: &str) -> Self {
        self.challenge = Some(
            HeaderValue::try_from(value)
                .unwrap_or_else(|_| panic!("invalid WWW-Authenticate value: {value:?}")),
        );
        self
    }

    /// Run the authenticator for `req`: the user, or the rejection.
    async fn run(&self, req: &mut Request) -> Result<()> {
        if let Some(c) = &self.challenge {
            req.extensions_mut().insert(super::AuthChallenge(c.clone()));
        }
        let policy = req.route().map(RoutePolicy::of).unwrap_or_default();
        let user = match self.authenticator.authenticate(req).await {
            Ok(user) => user,
            Err(_) if policy.is_anonymous() => None,
            Err(e) => return Err(e),
        };
        let Some(user) = user else {
            if policy.requires_auth() || (self.required && !policy.is_anonymous()) {
                return Err(missing_credentials(self.challenge.clone()));
            }
            return Ok(());
        };
        if policy.has_permissions() {
            match self.authorize {
                Some(allows) if allows(&user, &policy) => {}
                Some(_) => return Err(forbidden()),
                None => return Err(no_role_checker(req)),
            }
        }
        if req.route().is_some() {
            // The matched route's policy was checked against this user.
            req.extensions_mut().insert(PolicyVerified);
        }
        req.extensions_mut().insert(UserAuthenticated);
        req.extensions_mut().insert(CurrentUser(user));
        Ok(())
    }
}

impl<A: Authenticator> Authenticate<A>
where
    A::User: HasRoles,
{
    /// Also enforce the matched route's role and scope policies
    /// (`.require_roles(..)`, `.require_scopes(..)`) using [`HasRoles`].
    pub fn authorize(mut self) -> Self {
        self.authorize = Some(allows::<A::User>);
        self
    }
}

fn allows<U: HasRoles>(user: &U, policy: &RoutePolicy<'_>) -> bool {
    policy.allows(user)
}

fn no_role_checker(req: &Request) -> Error {
    Error::internal(format!(
        "route `{}` requires roles or scopes, but its `Authenticate` middleware has no role \
         checker: call `.authorize()` (the user type must implement `HasRoles`)",
        req.matched_path().unwrap_or("?")
    ))
    .with_code("auth_misconfigured")
    .diagnosed("ZL0078", None)
}

impl<A: Authenticator> Middleware for Authenticate<A> {
    fn handle(&self, mut req: Request, next: Next) -> BoxFuture<Response> {
        let this = self.clone();
        Box::pin(async move {
            match this.run(&mut req).await {
                Ok(()) => next.run(req).await,
                Err(e) => e.into_response(),
            }
        })
    }
}

/// The user authenticated by [`Authenticate`]. Rejects with
/// `401 missing_credentials` when there is none; use
/// `Option<CurrentUser<U>>` for optional authentication.
///
/// `U` must be the authenticator's `User` type. It is cloned per
/// extraction, so wrap large users in an `Arc`.
///
/// ```
/// use zapline::auth::CurrentUser;
///
/// async fn me(CurrentUser(name): CurrentUser<String>) -> String {
///     format!("hi {name}")
/// }
/// async fn greeting(user: Option<CurrentUser<String>>) -> String {
///     user.map_or("hi stranger".into(), |u| format!("hi {}", *u))
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentUser<U>(pub U);

impl<U> CurrentUser<U> {
    /// The user.
    pub fn into_inner(self) -> U {
        self.0
    }
}

impl<U> Deref for CurrentUser<U> {
    type Target = U;
    fn deref(&self) -> &U {
        &self.0
    }
}

impl<U> DerefMut for CurrentUser<U> {
    fn deref_mut(&mut self) -> &mut U {
        &mut self.0
    }
}

impl<U: Clone + Send + Sync + 'static> FromRequestParts for CurrentUser<U> {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.extensions()
            .get::<CurrentUser<U>>()
            .cloned()
            .ok_or_else(|| missing_credentials(Some(super::challenge_for(req))))
    }
}

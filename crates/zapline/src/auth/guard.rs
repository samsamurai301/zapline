//! Authorization: route policies (metadata), [`HasRoles`] and the [`Guard`]
//! middleware.
//!
//! Policies are typed route metadata, so tooling (e.g. OpenAPI security
//! requirements) can read them from the route table with
//! [`RoutePolicy::of`]. Each kind of requirement is a separate type; for each
//! kind the most specific declaration wins: a route's own
//! `.require_roles(..)` replaces its group's `.require_roles_all(..)`, while a
//! group's role requirement and a route's scope requirement both apply.

use std::future::ready;
use std::sync::Arc;

use http::Extensions;

use super::authenticate::CurrentUser;
use super::{forbidden, missing_credentials};
use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::{App, Error, IntoResponse, Request, Response, Result, RouteInfo, Routes};

/// A user type that carries roles (and optionally OAuth-style scopes), used
/// by role/scope policies.
///
/// ```
/// use zapline::auth::HasRoles;
///
/// #[derive(Clone)]
/// struct User { roles: Vec<String> }
///
/// impl HasRoles for User {
///     fn roles(&self) -> &[String] { &self.roles }
/// }
/// ```
pub trait HasRoles {
    /// The user's roles.
    fn roles(&self) -> &[String];

    /// The user's scopes (none by default).
    fn scopes(&self) -> &[String] {
        &[]
    }

    /// Whether the user has `role`.
    fn has_role(&self, role: &str) -> bool {
        self.roles().iter().any(|r| r == role)
    }

    /// Whether the user has `scope`.
    fn has_scope(&self, scope: &str) -> bool {
        self.scopes().iter().any(|s| s == scope)
    }
}

impl<T: HasRoles + ?Sized> HasRoles for &T {
    fn roles(&self) -> &[String] {
        (**self).roles()
    }
    fn scopes(&self) -> &[String] {
        (**self).scopes()
    }
}

impl<T: HasRoles + ?Sized> HasRoles for Arc<T> {
    fn roles(&self) -> &[String] {
        (**self).roles()
    }
    fn scopes(&self) -> &[String] {
        (**self).scopes()
    }
}

impl<T: HasRoles + ?Sized> HasRoles for Box<T> {
    fn roles(&self) -> &[String] {
        (**self).roles()
    }
    fn scopes(&self) -> &[String] {
        (**self).scopes()
    }
}

// ---------------------------------------------------------------------------
// Policy metadata

/// Route metadata: the route requires an authenticated user.
/// Set with `.require_auth()` / `.require_auth_all()`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequireAuth;

/// Route metadata: the route is public. Authentication requirements from the
/// group or middleware (`Authenticate::required`) and all role/scope
/// policies are skipped; valid credentials still populate `CurrentUser`.
/// Set with `.allow_anonymous()`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllowAnonymous;

/// Route metadata: the user needs **at least one** of these roles.
/// Set with `.require_roles([..])` / `.require_roles_all([..])`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequireRoles(Arc<[String]>);

/// Route metadata: the user needs **all** of these scopes.
/// Set with `.require_scopes([..])` / `.require_scopes_all([..])`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequireScopes(Arc<[String]>);

macro_rules! string_set {
    ($ty:ident, $getter:ident) => {
        impl $ty {
            /// A requirement for these values (duplicates are dropped).
            pub fn new<I, S>(items: I) -> Self
            where
                I: IntoIterator<Item = S>,
                S: Into<String>,
            {
                let mut v: Vec<String> = Vec::new();
                for s in items.into_iter().map(Into::into) {
                    if !v.contains(&s) {
                        v.push(s);
                    }
                }
                $ty(v.into())
            }

            /// The required values.
            pub fn $getter(&self) -> &[String] {
                &self.0
            }

            /// Union with `other` (keeping order, without duplicates).
            fn merged(&self, other: Self) -> Self {
                let mut v = self.0.to_vec();
                for s in other.0.iter() {
                    if !v.contains(s) {
                        v.push(s.clone());
                    }
                }
                $ty(v.into())
            }

            /// Insert into `meta`, merging with an existing value.
            fn merge_into(self, meta: &mut Extensions) {
                let value = match meta.get::<$ty>() {
                    Some(old) => old.merged(self),
                    None => self,
                };
                meta.insert(value);
            }
        }
    };
}
string_set!(RequireRoles, roles);
string_set!(RequireScopes, scopes);

/// The effective access policy of a route, read from its metadata.
///
/// ```
/// use zapline::auth::RoutePolicy;
/// use zapline::prelude::*;
///
/// let table = App::new()
///     .get("/admin", || async { "" }).require_roles(["admin"])
///     .route_table();
/// let policy = RoutePolicy::of(table[0]);
/// assert!(policy.requires_auth());
/// assert_eq!(policy.roles(), ["admin".to_string()]);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoutePolicy<'a> {
    anonymous: bool,
    auth: bool,
    roles: &'a [String],
    scopes: &'a [String],
}

impl<'a> RoutePolicy<'a> {
    /// The policy declared on `route`.
    pub fn of(route: &'a RouteInfo) -> Self {
        let meta = route.meta_map();
        if meta.is_empty() {
            return Self::default();
        }
        if meta.get::<AllowAnonymous>().is_some() {
            return RoutePolicy {
                anonymous: true,
                ..Self::default()
            };
        }
        let roles = meta.get::<RequireRoles>();
        let scopes = meta.get::<RequireScopes>();
        RoutePolicy {
            anonymous: false,
            auth: meta.get::<RequireAuth>().is_some() || roles.is_some() || scopes.is_some(),
            roles: roles.map_or(&[], |r| r.roles()),
            scopes: scopes.map_or(&[], |s| s.scopes()),
        }
    }

    /// The route is marked `.allow_anonymous()`.
    pub fn is_anonymous(&self) -> bool {
        self.anonymous
    }

    /// An authenticated user is required (any auth, role or scope policy).
    pub fn requires_auth(&self) -> bool {
        self.auth
    }

    /// Accepted roles (any one suffices); empty when there is no role policy.
    pub fn roles(&self) -> &'a [String] {
        self.roles
    }

    /// Required scopes (all are needed).
    pub fn scopes(&self) -> &'a [String] {
        self.scopes
    }

    /// The policy has role or scope requirements.
    pub fn has_permissions(&self) -> bool {
        !self.roles.is_empty() || !self.scopes.is_empty()
    }

    /// Whether `user` satisfies the role and scope requirements.
    pub fn allows<U: HasRoles + ?Sized>(&self, user: &U) -> bool {
        let roles_ok = self.roles.is_empty() || self.roles.iter().any(|r| user.has_role(r));
        roles_ok && self.scopes.iter().all(|s| user.has_scope(s))
    }
}

// ---------------------------------------------------------------------------
// Builder methods

impl Routes {
    /// Require an authenticated user for the most recently registered route.
    /// Enforced by [`Authenticate`](super::Authenticate) or [`Guard`].
    pub fn require_auth(mut self) -> Self {
        self.last_meta_mut().insert(RequireAuth);
        self
    }

    /// Require at least one of `roles` for the most recently registered
    /// route. Repeated calls add roles.
    pub fn require_roles<I, S>(mut self, roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        RequireRoles::new(roles).merge_into(self.last_meta_mut());
        self
    }

    /// Require all of `scopes` for the most recently registered route.
    pub fn require_scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        RequireScopes::new(scopes).merge_into(self.last_meta_mut());
        self
    }

    /// Make the most recently registered route public (see [`AllowAnonymous`]).
    pub fn allow_anonymous(mut self) -> Self {
        self.last_meta_mut().insert(AllowAnonymous);
        self
    }

    /// Require an authenticated user for every route in this group.
    pub fn require_auth_all(mut self) -> Self {
        self.defaults_mut().insert(RequireAuth);
        self
    }

    /// Require at least one of `roles` for every route in this group (routes
    /// with their own `.require_roles(..)` keep theirs).
    pub fn require_roles_all<I, S>(mut self, roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        RequireRoles::new(roles).merge_into(self.defaults_mut());
        self
    }

    /// Require all of `scopes` for every route in this group (routes with
    /// their own `.require_scopes(..)` keep theirs).
    pub fn require_scopes_all<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        RequireScopes::new(scopes).merge_into(self.defaults_mut());
        self
    }
}

impl App {
    /// Require an authenticated user for the most recently registered route.
    pub fn require_auth(mut self) -> Self {
        self.routes_mut().last_meta_mut().insert(RequireAuth);
        self
    }

    /// Require at least one of `roles` for the most recently registered route.
    pub fn require_roles<I, S>(mut self, roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        RequireRoles::new(roles).merge_into(self.routes_mut().last_meta_mut());
        self
    }

    /// Require all of `scopes` for the most recently registered route.
    pub fn require_scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        RequireScopes::new(scopes).merge_into(self.routes_mut().last_meta_mut());
        self
    }

    /// Make the most recently registered route public (see [`AllowAnonymous`]).
    pub fn allow_anonymous(mut self) -> Self {
        self.routes_mut().last_meta_mut().insert(AllowAnonymous);
        self
    }
}

// ---------------------------------------------------------------------------
// Guard

type Check<U> =
    Arc<dyn Fn(Option<&U>, &RoutePolicy<'_>, &'static RouteInfo) -> Result<()> + Send + Sync>;

/// Route middleware that authorizes the [`CurrentUser<U>`](CurrentUser) set
/// by an earlier [`Authenticate`](super::Authenticate) middleware.
///
/// It must run after routing and after authentication: add it to the same
/// group after `Authenticate` (`.wrap(auth).wrap(guard)`), or use
/// [`App::wrap_route`] together with a global `App::wrap(Authenticate::..)`.
/// Routes marked `.allow_anonymous()` are skipped. No user means
/// `401 missing_credentials`; a failed check means `403 forbidden`.
///
/// ```
/// use zapline::auth::{Authenticate, Guard, HasRoles};
/// use zapline::RouteInfo;
/// # use zapline::prelude::*;
/// # #[derive(Clone)] struct User { id: u64, roles: Vec<String> }
/// # impl HasRoles for User { fn roles(&self) -> &[String] { &self.roles } }
/// # let auth = Authenticate::new(|_: &Request| async { Ok(Some(User { id: 1, roles: vec![] })) });
/// // Custom rule: only user 1 may call DELETE routes.
/// let guard = Guard::new(|user: &User, route: &RouteInfo| {
///     route.method() != Some(&Method::DELETE) || user.id == 1
/// });
/// let api = Routes::new().delete("/x", || async { "" }).wrap(auth).wrap(guard);
/// ```
pub struct Guard<U> {
    check: Check<U>,
}

impl<U> Clone for Guard<U> {
    fn clone(&self) -> Self {
        Guard {
            check: self.check.clone(),
        }
    }
}

impl<U: Clone + Send + Sync + 'static> Guard<U> {
    /// A guard with a custom rule. It receives the user and the matched
    /// route (with its metadata).
    pub fn new<F>(rule: F) -> Self
    where
        F: Fn(&U, &RouteInfo) -> bool + Send + Sync + 'static,
    {
        Guard::from_check(move |user, policy, route| {
            if policy.is_anonymous() {
                return Ok(());
            }
            let user = user.ok_or_else(|| missing_credentials(None))?;
            if rule(user, route) {
                Ok(())
            } else {
                Err(forbidden())
            }
        })
    }

    fn from_check<F>(check: F) -> Self
    where
        F: Fn(Option<&U>, &RoutePolicy<'_>, &'static RouteInfo) -> Result<()>
            + Send
            + Sync
            + 'static,
    {
        Guard {
            check: Arc::new(check),
        }
    }
}

impl<U: HasRoles + Clone + Send + Sync + 'static> Guard<U> {
    /// Enforce each route's own policies (`.require_auth()`,
    /// `.require_roles(..)`, `.require_scopes(..)`). Routes without a policy
    /// pass, with or without a user.
    pub fn policies() -> Self {
        Guard::from_check(|user, policy, _| {
            if !policy.requires_auth() {
                return Ok(());
            }
            let user = user.ok_or_else(|| missing_credentials(None))?;
            if policy.allows(user) {
                Ok(())
            } else {
                Err(forbidden())
            }
        })
    }

    /// Require at least one of `roles` on every route this wraps, in addition
    /// to route policies' own checks.
    pub fn roles<I, S>(roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let roles = RequireRoles::new(roles);
        Guard::new(move |user: &U, _| roles.roles().iter().any(|r| user.has_role(r)))
    }

    /// Require all of `scopes` on every route this wraps.
    pub fn scopes<I, S>(scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let scopes = RequireScopes::new(scopes);
        Guard::new(move |user: &U, _| scopes.scopes().iter().all(|s| user.has_scope(s)))
    }
}

impl<U: Clone + Send + Sync + 'static> Middleware for Guard<U> {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let Some(route) = req.route() else {
            let err = Error::internal(
                "`Guard` runs before routing: install it with `App::wrap_route` or `Routes::wrap`",
            )
            .with_code("auth_misconfigured");
            return Box::pin(ready(err.into_response()));
        };
        let policy = RoutePolicy::of(route);
        let user = req.extensions().get::<CurrentUser<U>>().map(|u| &u.0);
        match (self.check)(user, &policy, route) {
            Ok(()) => next.run(req),
            Err(e) => Box::pin(ready(e.into_response())),
        }
    }
}

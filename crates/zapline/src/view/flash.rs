//! The `flash` template global: the request's waiting flash messages,
//! read from its session only when a template looks at them.
//!
//! Rendering is synchronous, so the session of the request being rendered
//! sits in a thread-local for the duration of one render call (set by
//! `Views::with_env`, only when the request has a session).

use std::cell::RefCell;
use std::sync::Arc;

use minijinja::value::{Enumerator, Object, ObjectRepr};
use minijinja::Value;

use crate::session::Session;

thread_local! {
    static CURRENT: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// Restores the previous session (normally none) when the render ends.
pub(super) struct Guard(Option<Session>);

/// Make `session` the one `flash` reads until the guard drops.
pub(super) fn enter(session: Session) -> Guard {
    Guard(CURRENT.with(|c| c.replace(Some(session))))
}

impl Drop for Guard {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = previous);
    }
}

/// `flash` in templates: a list of `{level, message}`, empty without a
/// session. The first look uses the messages up (later looks in the same
/// request see the same list).
#[derive(Debug)]
pub(super) struct FlashGlobal;

impl FlashGlobal {
    fn messages(&self) -> Vec<Value> {
        let session = CURRENT.with(|c| c.borrow().clone());
        match session {
            Some(session) => session
                .take_flash()
                .iter()
                .map(Value::from_serialize)
                .collect(),
            None => Vec::new(),
        }
    }
}

impl Object for FlashGlobal {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let index = usize::try_from(key.clone()).ok()?;
        self.messages().into_iter().nth(index)
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Values(self.messages())
    }
}

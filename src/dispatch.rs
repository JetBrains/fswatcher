use std::collections::{BTreeMap, HashMap, HashSet};
use tracing::trace;

use crate::{
    backend::{Audience, BackendEventContext, SubscriptionId},
    session::SessionId,
};

#[derive(Default)]
pub struct Dispatch {
    subscriptions: BTreeMap<SubscriptionId, SessionId>,
    handlers: HashMap<SessionId, SessionHandler>,
}

pub type SessionHandler = Box<dyn for<'a> FnMut(BackendEventContext<'a>) + Send + 'static>;

impl Dispatch {
    pub fn register_session(&mut self, session_id: SessionId, handler: SessionHandler) {
        trace!("registered new session {session_id:?}");
        self.handlers.insert(session_id, handler);
    }

    pub fn destroy_session(&mut self, session_id: SessionId) {
        self.handlers.remove(&session_id);
    }

    pub fn register_subscription(&mut self, subscription_id: SubscriptionId, session_id: SessionId) {
        trace!(?session_id, "registered new subscription {subscription_id:?}");
        self.subscriptions.insert(subscription_id, session_id);
    }

    pub fn unregister_subscription(&mut self, subscription_id: SubscriptionId) {
        trace!("destroying subscription {subscription_id:?}");
        self.subscriptions.remove(&subscription_id);
    }

    pub fn dispatch_event(&mut self, ctx: BackendEventContext<'_>) {
        match ctx.audience {
            Audience::All => {
                for handler in self.handlers.values_mut() {
                    handler(BackendEventContext {
                        audience: ctx.audience,
                        event: ctx.event,
                        event_path: ctx.event_path,
                        backend: ctx.backend,
                    });
                }
            }
            Audience::Some(subscriptions) => {
                let session_ids = subscriptions
                    .iter()
                    .filter_map(|sub_id| self.subscriptions.get(sub_id).copied())
                    .collect::<HashSet<_>>();
                for session_id in session_ids {
                    let Some(handler) = self.handlers.get_mut(&session_id) else {
                        continue;
                    };
                    handler(BackendEventContext {
                        audience: ctx.audience,
                        event: ctx.event,
                        event_path: ctx.event_path,
                        backend: ctx.backend,
                    });
                }
            }
        }
    }

    #[cfg(test)]
    pub fn debug(&self) -> DispatchDebug {
        DispatchDebug {
            sessions: self.handlers.keys().copied().collect(),
            subscriptions: self.subscriptions.clone(),
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
pub struct DispatchDebug {
    pub sessions: HashSet<SessionId>,
    pub subscriptions: BTreeMap<SubscriptionId, SessionId>,
}

#[cfg(test)]
impl DispatchDebug {
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty() && self.subscriptions.is_empty()
    }
}

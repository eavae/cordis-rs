//! Event dispatch service.
//!
//! Story card B2 provides the minimal `on`/`emit` hook table used by the
//! fiber lifecycle; story card B5 implements the full five dispatch modes
//! and listener lifecycle binding.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::context::Context;
use crate::fiber::{CordisError, EffectHandle};
use crate::service::{Effect, Service};

/// Event dispatch service, available on every context as `ctx.events`.
pub struct EventsService {
    hooks: HookTable,
}

/// A single event listener.
pub(crate) type EventCallback = Rc<dyn Fn(&[Rc<dyn Any>])>;

/// Event name → listeners table.
pub(crate) type HookTable = Rc<RefCell<HashMap<String, Vec<EventCallback>>>>;

impl Default for EventsService {
    fn default() -> Self {
        EventsService {
            hooks: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

impl EventsService {
    /// Registers a listener bound to the fiber of `ctx`.
    ///
    /// Mirrors `ctx.on()` in the TS reference: the registration is an effect
    /// of `ctx`'s fiber, so it is removed when the fiber unloads.
    pub fn on(
        &self,
        ctx: &Context,
        event: &str,
        callback: impl Fn(&[Rc<dyn Any>]) + 'static,
    ) -> Result<Rc<EffectHandle>, CordisError> {
        let event = event.to_string();
        let callback: EventCallback = Rc::new(callback);
        let hooks = Rc::clone(&self.hooks);
        let effect_label = format!("ctx.on({event:?})");
        ctx.fiber().effect(
            move || {
                hooks
                    .borrow_mut()
                    .entry(event.clone())
                    .or_default()
                    .push(callback.clone());
                let hooks = Rc::clone(&hooks);
                let event = event.clone();
                Effect::Disposer(crate::service::sync_disposer(move || {
                    if let Some(list) = hooks.borrow_mut().get_mut(&event)
                        && let Some(position) = list.iter().position(|c| Rc::ptr_eq(c, &callback))
                    {
                        list.remove(position);
                    }
                }))
            },
            &effect_label,
        )
    }

    /// Emits an event synchronously to all listeners.
    pub fn emit(&self, event: &str, args: &[Rc<dyn Any>]) {
        let listeners = self.hooks.borrow().get(event).cloned().unwrap_or_default();
        for listener in listeners {
            listener(args);
        }
    }
}

impl Service for EventsService {
    const NAME: &'static str = "events";
}

//! A minimal example plugin (story card E1).

use std::rc::Rc;

use cordis_sdk::{ApplyFn, Context, Effect, Plugin, service, sync_disposer};

#[service]
struct HelloService;

/// The plugin's apply callback: registers a service, resolves it through the
/// macro-generated traced accessor, logs a greeting and registers an effect.
pub fn apply(ctx: &Context, _config: &Rc<dyn std::any::Any>) -> Effect {
    let ctx = ctx.clone();
    let ctx_for_dispose = ctx.clone();
    // `#[service]` generates a typed accessor that returns a traced handle;
    // the handle derefs to the service itself.
    drop(ctx.provide::<HelloService>(Rc::new(HelloService)).unwrap());
    assert!(ctx.hello_service().is_some());
    ctx.logger().named("hello").info("hello from cordis plugin");
    Effect::Disposer(sync_disposer(move || {
        ctx_for_dispose.logger().named("hello").info("goodbye");
    }))
}

fn main() {
    let plugin = Plugin {
        is_group: false,
        name: Some("hello".to_string()),
        inject: Vec::new(),
        apply: Rc::new(apply) as ApplyFn,
    };
    println!("hello plugin ready: {:?}", plugin.name);
}

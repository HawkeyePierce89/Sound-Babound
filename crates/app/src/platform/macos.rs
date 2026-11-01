//! macOS glue.

use tao::event_loop::EventLoop;
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::TrayIconBuilder;

/// No-op: a macOS binary always keeps the console it was launched from.
pub fn attach_parent_console() {}

/// Runs the app as an accessory: a menu bar item with no Dock icon and no
/// app menu.
pub fn prepare_event_loop<T: 'static>(event_loop: &mut EventLoop<T>) {
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
}

/// Sets a black tray icon as a template image, so the menu bar recolors it
/// for light and dark appearance.
pub fn with_tray_icon(builder: TrayIconBuilder) -> anyhow::Result<TrayIconBuilder> {
    Ok(builder.with_icon_templated(crate::icon::tray_icon([0, 0, 0])?))
}

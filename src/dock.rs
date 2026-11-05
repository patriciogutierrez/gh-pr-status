//! The number on the app's Dock icon. GPUI has no API for it, so this talks to
//! AppKit directly; it must run on the main thread, which GPUI's updates do.

use cocoa::base::{id, nil};
use cocoa::foundation::NSString;
use objc::{class, msg_send, sel, sel_impl};

/// Shows `count` on the Dock icon, or nothing when it is zero.
pub fn set_badge(count: usize) {
    // SAFETY: plain AppKit messages to the shared application's Dock tile,
    // sent from the main thread. The label is released once the tile, which
    // copies it, has it.
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let tile: id = msg_send![app, dockTile];
        if count == 0 {
            let _: () = msg_send![tile, setBadgeLabel: nil];
        } else {
            let label: id = NSString::alloc(nil).init_str(&count.to_string());
            let _: () = msg_send![tile, setBadgeLabel: label];
            let _: () = msg_send![label, release];
        }
    }
}

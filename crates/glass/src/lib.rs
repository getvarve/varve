//! Liquid Glass behind parts of the window on macOS 26.
//!
//! The window paints an opaque background, the webview on top of it is
//! transparent, and every shape the UI reports gets its own
//! `NSGlassEffectView` between the two. So the UI decides where glass is (the
//! sidebar card) and macOS draws it, instead of one glass layer behind the
//! whole window.
//!
//! `NSGlassEffectView` exists from macOS 26. It is looked up at run time, so
//! the app still starts on older systems: there [`supported`] is false and the
//! caller keeps its other material. On other platforms every function does
//! nothing and says so.
//!
//! Every function that touches a view must run on the main thread, where
//! AppKit's views live; elsewhere it does nothing and returns false.

#![cfg_attr(target_os = "macos", allow(unsafe_code))]

/// One glass shape, in points from the top-left corner of the window's
/// content. At a webview zoom of 100 percent these are CSS pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    /// The UI's name for the shape. A shape with a known id is moved, not made again.
    pub id: String,
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
    /// Corner radius.
    pub radius: f64,
}

impl Shape {
    /// Whether the shape can be shown: finite numbers and a size above zero.
    #[must_use]
    pub fn is_drawable(&self) -> bool {
        [self.x, self.y, self.width, self.height, self.radius]
            .iter()
            .all(|n| n.is_finite())
            && self.width > 0.0
            && self.height > 0.0
            && self.radius >= 0.0
    }
}

/// Whether this system has the glass view (macOS 26 and newer).
#[must_use]
pub fn supported() -> bool {
    platform::supported()
}

/// Makes the window paint its own background again. A transparent window
/// (which the transparent webview needs) would otherwise show the desktop
/// through the glass. Returns whether it was done.
pub fn opaque_background(window: &tauri::WebviewWindow) -> bool {
    platform::opaque_background(window)
}

/// Shows exactly these shapes behind the webview of `window`: new ones are
/// made, known ones moved, and the ones no longer listed removed. Shapes that
/// cannot be drawn are left out. Returns whether the glass was updated.
pub fn show(window: &tauri::WebviewWindow, shapes: &[Shape]) -> bool {
    platform::show(window, shapes)
}

#[cfg(target_os = "macos")]
mod platform {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use objc2::rc::{Allocated, Retained};
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{MainThreadMarker, msg_send};
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    use super::Shape;

    /// `NSWindowBelow`: under every other view of the same parent, so under the webview.
    const BELOW: isize = -1;

    thread_local! {
        /// The glass views that exist, by the id the UI gave them. Only the main thread has any.
        static VIEWS: RefCell<HashMap<String, Retained<AnyObject>>> = RefCell::new(HashMap::new());
    }

    fn glass_class() -> Option<&'static AnyClass> {
        AnyClass::get(c"NSGlassEffectView")
    }

    pub(super) fn supported() -> bool {
        glass_class().is_some()
    }

    /// The `NSWindow` of a Tauri window, as an object to send messages to.
    fn ns_window(window: &tauri::WebviewWindow) -> Option<&AnyObject> {
        let pointer = window.ns_window().ok()?.cast::<AnyObject>();
        // SAFETY: Tauri hands out the pointer to the live NSWindow of `window`, which stays
        // alive at least as long as the borrow of `window` this reference is tied to.
        unsafe { pointer.as_ref() }
    }

    pub(super) fn opaque_background(window: &tauri::WebviewWindow) -> bool {
        if MainThreadMarker::new().is_none() {
            return false;
        }
        let (Some(ns_window), Some(colors)) = (ns_window(window), AnyClass::get(c"NSColor")) else {
            return false;
        };
        // SAFETY: on the main thread. `windowBackgroundColor` is a class method of NSColor that
        // returns an autoreleased colour, and `setBackgroundColor:` takes an NSColor or nil.
        unsafe {
            let color: Option<Retained<AnyObject>> = msg_send![colors, windowBackgroundColor];
            let Some(color) = color else {
                return false;
            };
            let _: () = msg_send![ns_window, setBackgroundColor: &*color];
        }
        true
    }

    pub(super) fn show(window: &tauri::WebviewWindow, shapes: &[Shape]) -> bool {
        if MainThreadMarker::new().is_none() {
            return false;
        }
        let (Some(class), Some(ns_window)) = (glass_class(), ns_window(window)) else {
            return false;
        };
        // SAFETY: on the main thread; `contentView` returns the window's content view or nil.
        let content: Option<Retained<AnyObject>> = unsafe { msg_send![ns_window, contentView] };
        let Some(content) = content else {
            return false;
        };
        // SAFETY: `bounds` and `isFlipped` are plain getters of NSView.
        let (bounds, flipped): (NSRect, bool) = unsafe {
            (
                msg_send![&*content, bounds],
                msg_send![&*content, isFlipped],
            )
        };

        let shapes: Vec<&Shape> = shapes.iter().filter(|shape| shape.is_drawable()).collect();
        VIEWS.with_borrow_mut(|views| {
            views.retain(|id, view| {
                if shapes.iter().any(|shape| &shape.id == id) {
                    return true;
                }
                // SAFETY: `view` is an NSView this module made; removing it from its parent is
                // always allowed, also when the parent is already gone.
                let _: () = unsafe { msg_send![&**view, removeFromSuperview] };
                false
            });
            for shape in shapes {
                // AppKit measures from the bottom-left corner unless the view is flipped.
                let y = if flipped {
                    shape.y
                } else {
                    bounds.size.height - shape.y - shape.height
                };
                let frame = NSRect::new(
                    NSPoint::new(shape.x, y),
                    NSSize::new(shape.width, shape.height),
                );
                if !views.contains_key(&shape.id) {
                    // SAFETY: `alloc` then `initWithFrame:` is how an NSView is made; `class` is
                    // NSGlassEffectView, an NSView subclass.
                    let made: Option<Retained<AnyObject>> = unsafe {
                        let empty: Allocated<AnyObject> = msg_send![class, alloc];
                        msg_send![empty, initWithFrame: frame]
                    };
                    let Some(view) = made else {
                        tracing::warn!(id = %shape.id, "could not make a glass view");
                        continue;
                    };
                    // SAFETY: both are live NSViews; a nil sibling puts the view under all others.
                    let _: () = unsafe {
                        msg_send![
                            &*content,
                            addSubview: &*view,
                            positioned: BELOW,
                            relativeTo: Option::<&AnyObject>::None
                        ]
                    };
                    views.insert(shape.id.clone(), view);
                }
                if let Some(view) = views.get(&shape.id) {
                    // SAFETY: `setFrame:` is an NSView setter, `setCornerRadius:` the glass
                    // view's own; both take plain values.
                    unsafe {
                        let _: () = msg_send![&**view, setFrame: frame];
                        let _: () = msg_send![&**view, setCornerRadius: shape.radius];
                    }
                }
            }
        });
        true
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::Shape;

    pub(super) fn supported() -> bool {
        false
    }

    pub(super) fn opaque_background(_window: &tauri::WebviewWindow) -> bool {
        false
    }

    pub(super) fn show(_window: &tauri::WebviewWindow, _shapes: &[Shape]) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::Shape;

    fn shape(width: f64, height: f64, radius: f64) -> Shape {
        Shape {
            id: "sidebar".into(),
            x: 8.0,
            y: 8.0,
            width,
            height,
            radius,
        }
    }

    #[test]
    fn only_shapes_with_a_real_size_are_drawn() {
        assert!(shape(202.0, 700.0, 18.0).is_drawable());
        assert!(!shape(0.0, 700.0, 18.0).is_drawable());
        assert!(!shape(202.0, -1.0, 18.0).is_drawable());
        assert!(!shape(202.0, 700.0, -1.0).is_drawable());
        assert!(!shape(f64::NAN, 700.0, 18.0).is_drawable());
        assert!(!shape(202.0, f64::INFINITY, 18.0).is_drawable());
    }
}

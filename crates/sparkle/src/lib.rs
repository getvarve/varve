//! Sparkle updates on macOS: the standard updater controller, loaded at
//! runtime.
//!
//! The release bundle carries `Sparkle.framework` in `Contents/Frameworks`.
//! Nothing links against it: [`start`] loads it with `NSBundle` and talks to
//! it through the Objective-C runtime, so a dev build or an app bundle
//! without the framework simply has no updater.
//!
//! Sparkle reads `SUPublicEDKey`, `SUEnableAutomaticChecks` and
//! `SUScheduledCheckInterval` from the app's Info.plist. The feed comes from
//! the delegate, so each architecture gets its own appcast. Before
//! relaunching for an update Sparkle asks the delegate: the `ask` callback of
//! [`start`] decides whether to hold the relaunch back, and
//! [`continue_relaunch`] continues it. Declined, the relaunch stays held: the app
//! can continue it later, and otherwise the update installs when the app
//! quits. Right before Sparkle quits the app to relaunch, `will_relaunch` runs.
//!
//! Every function must run on the main thread, where Sparkle's objects live;
//! they do nothing (and say so in their result) elsewhere or before [`start`].
//! On other platforms the crate is empty.

#![cfg(target_os = "macos")]
// The one crate allowed to call Objective-C: every block states why it is sound.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ffi::{CStr, CString};

use block2::{Block, RcBlock};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool, NSObject, NSObjectProtocol};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};

/// Decides, when Sparkle wants to relaunch, whether to hold it back while
/// someone (the UI) is asked; returns true when an answer will come through
/// [`continue_relaunch`].
pub type Ask = Box<dyn Fn() -> bool>;

/// Runs right before Sparkle quits the app to install and relaunch.
pub type WillRelaunch = Box<dyn Fn()>;

/// The running updater.
struct Sparkle {
    controller: Retained<AnyObject>,
    /// Sparkle keeps its delegate weakly: this keeps it alive.
    _delegate: Retained<Delegate>,
}

thread_local! {
    /// Main thread only: the running updater.
    static SPARKLE: RefCell<Option<Sparkle>> = const { RefCell::new(None) };
    /// Main thread only: the block Sparkle gave to continue a held-back relaunch.
    static RELAUNCH: RefCell<Option<RcBlock<dyn Fn()>>> = const { RefCell::new(None) };
}

/// The delegate's state: its feed and the relaunch question.
#[doc(hidden)]
pub struct DelegateIvars {
    feed_url: Retained<AnyObject>,
    ask: Ask,
    will_relaunch: WillRelaunch,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and Delegate does not
    // implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    // Objective-C class names are global: a plain "Delegate" could clash.
    #[name = "VarveSparkleUpdaterDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    // SAFETY: NSObjectProtocol has no requirements beyond being an NSObject.
    unsafe impl NSObjectProtocol for Delegate {}

    impl Delegate {
        /// SPUUpdaterDelegate: the appcast for this architecture.
        // SAFETY: the selector takes the updater and returns an NSString,
        // which the ivar holds for the delegate's lifetime.
        #[unsafe(method_id(feedURLStringForUpdater:))]
        fn feed_url(&self, _updater: &AnyObject) -> Retained<AnyObject> {
            self.ivars().feed_url.clone()
        }

        /// SPUUpdaterDelegate: hold the relaunch back while `ask` gets an
        /// answer; NO (relaunch now) when nobody can answer.
        // SAFETY: the selector takes the updater, the appcast item and a
        // `void (^)(void)` block, and returns BOOL, as declared here.
        #[unsafe(method(updater:shouldPostponeRelaunchForUpdate:untilInvokingBlock:))]
        fn postpone_relaunch(
            &self,
            _updater: &AnyObject,
            _item: &AnyObject,
            install: &Block<dyn Fn()>,
        ) -> Bool {
            RELAUNCH.with(|slot| *slot.borrow_mut() = Some(install.copy()));
            if (self.ivars().ask)() {
                Bool::YES
            } else {
                RELAUNCH.with(|slot| slot.borrow_mut().take());
                Bool::NO
            }
        }

        /// SPUUpdaterDelegate: Sparkle is about to quit the app to install
        /// and relaunch. It quits through `-[NSApp terminate:]`, past the
        /// app's own quit handling, so this is the last moment to clean up.
        // SAFETY: the selector takes the updater and returns void.
        #[unsafe(method(updaterWillRelaunchApplication:))]
        fn will_relaunch(&self, _updater: &AnyObject) {
            (self.ivars().will_relaunch)();
        }
    }
);

impl Delegate {
    fn new(
        mtm: MainThreadMarker,
        feed_url: Retained<AnyObject>,
        ask: Ask,
        will_relaunch: WillRelaunch,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars {
            feed_url,
            ask,
            will_relaunch,
        });
        // SAFETY: NSObject's designated initializer on a freshly allocated
        // instance whose ivars are set.
        unsafe { msg_send![super(this), init] }
    }
}

/// An NSString with `value`, or None for a string with a NUL byte.
fn ns_string(value: &str) -> Option<Retained<AnyObject>> {
    let class = AnyClass::get(c"NSString")?;
    let bytes = CString::new(value).ok()?;
    // SAFETY: stringWithUTF8String: takes a NUL-terminated UTF-8 C string,
    // which `bytes` is and outlives the call; it returns an NSString or nil.
    unsafe { msg_send![class, stringWithUTF8String: bytes.as_ptr()] }
}

/// Loads `Contents/Frameworks/Sparkle.framework` of the running app; false
/// when it has none (dev builds) or the framework does not load.
fn load_framework() -> bool {
    let Some(bundle_class) = AnyClass::get(c"NSBundle") else {
        return false;
    };
    let Some(name) = ns_string("Sparkle.framework") else {
        return false;
    };
    // SAFETY: mainBundle returns the app's NSBundle; privateFrameworksPath
    // returns an NSString or nil (outside a bundle);
    // stringByAppendingPathComponent: takes and returns an NSString;
    // bundleWithPath: returns an NSBundle or nil when nothing is there; load
    // returns BOOL.
    unsafe {
        let main: Option<Retained<AnyObject>> = msg_send![bundle_class, mainBundle];
        let Some(main) = main else {
            return false;
        };
        let frameworks: Option<Retained<AnyObject>> = msg_send![&*main, privateFrameworksPath];
        let Some(frameworks) = frameworks else {
            return false;
        };
        let path: Retained<AnyObject> =
            msg_send![&*frameworks, stringByAppendingPathComponent: &*name];
        let bundle: Option<Retained<AnyObject>> = msg_send![bundle_class, bundleWithPath: &*path];
        match bundle {
            Some(bundle) => {
                let loaded: Bool = msg_send![&*bundle, load];
                loaded.as_bool()
            }
            None => false,
        }
    }
}

/// Starts Sparkle with `feed_url` when the app bundle carries the framework,
/// and returns whether it runs. Call once, on the main thread.
pub fn start(feed_url: &str, ask: Ask, will_relaunch: WillRelaunch) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("Sparkle must start on the main thread");
        return false;
    };
    if is_running() {
        return true;
    }
    if !load_framework() {
        tracing::info!("no Sparkle.framework in the app bundle: updates are off");
        return false;
    }
    let class_name: &CStr = c"SPUStandardUpdaterController";
    let Some(controller_class) = AnyClass::get(class_name) else {
        tracing::warn!("Sparkle.framework loaded without SPUStandardUpdaterController");
        return false;
    };
    let Some(feed) = ns_string(feed_url) else {
        return false;
    };
    let delegate = Delegate::new(mtm, feed, ask, will_relaunch);
    let nil: Option<&AnyObject> = None;
    // SAFETY: alloc returns a fresh instance of the controller class;
    // initWithStartingUpdater:updaterDelegate:userDriverDelegate: is its
    // designated initializer (BOOL, an SPUUpdaterDelegate object, an optional
    // SPUStandardUserDriverDelegate) and returns the controller or nil. The
    // delegate implements only optional protocol methods, with the
    // signatures Sparkle declares.
    let controller: Option<Retained<AnyObject>> = unsafe {
        let allocated: Allocated<AnyObject> = msg_send![controller_class, alloc];
        msg_send![
            allocated,
            initWithStartingUpdater: Bool::YES,
            updaterDelegate: &*delegate,
            userDriverDelegate: nil
        ]
    };
    let Some(controller) = controller else {
        tracing::warn!("Sparkle's updater controller did not start");
        return false;
    };
    SPARKLE.with(|slot| {
        *slot.borrow_mut() = Some(Sparkle {
            controller,
            _delegate: delegate,
        });
    });
    tracing::info!(feed = feed_url, "Sparkle started");
    true
}

/// True once Sparkle runs (main thread).
#[must_use]
pub fn is_running() -> bool {
    SPARKLE.with(|slot| slot.borrow().is_some())
}

/// Opens Sparkle's "Check for Updates" window; false without Sparkle.
pub fn check_for_updates() -> bool {
    SPARKLE.with(|slot| match slot.borrow().as_ref() {
        Some(sparkle) => {
            let nil: Option<&AnyObject> = None;
            // SAFETY: checkForUpdates: is the controller's menu action and
            // takes an optional sender.
            let _: () = unsafe { msg_send![&*sparkle.controller, checkForUpdates: nil] };
            true
        }
        None => false,
    })
}

/// Runs `f` with the controller's SPUUpdater.
fn with_updater<T>(f: impl FnOnce(&AnyObject) -> T) -> Option<T> {
    SPARKLE.with(|slot| {
        let slot = slot.borrow();
        let sparkle = slot.as_ref()?;
        // SAFETY: the controller's `updater` getter returns its SPUUpdater,
        // which exists once the controller started.
        let updater: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*sparkle.controller, updater] };
        updater.map(|updater| f(&updater))
    })
}

/// Sparkle's own "check automatically" preference; None without Sparkle.
#[must_use]
pub fn automatically_checks() -> Option<bool> {
    with_updater(|updater| {
        // SAFETY: automaticallyChecksForUpdates is a BOOL property of SPUUpdater.
        let value: Bool = unsafe { msg_send![updater, automaticallyChecksForUpdates] };
        value.as_bool()
    })
}

/// Sets Sparkle's "check automatically" preference; false without Sparkle.
pub fn set_automatically_checks(on: bool) -> bool {
    with_updater(|updater| {
        // SAFETY: setAutomaticallyChecksForUpdates: is the setter of a BOOL
        // property of SPUUpdater.
        let _: () = unsafe { msg_send![updater, setAutomaticallyChecksForUpdates: Bool::new(on)] };
    })
    .is_some()
}

/// Continues a relaunch Sparkle held back: runs `before`, then lets Sparkle
/// install and relaunch. Returns false, and runs nothing, when no relaunch is
/// held back.
///
/// A declined relaunch is not answered at all: it stays held, so the app can
/// continue it later. Sparkle relaunches without asking again when a previous
/// relaunch was dropped, so dropping it would skip the app's checks.
pub fn continue_relaunch(before: impl FnOnce()) -> bool {
    let install = RELAUNCH.with(|slot| slot.borrow_mut().take());
    match install {
        Some(install) => {
            before();
            install.call(());
            true
        }
        None => false,
    }
}

/// True while Sparkle holds a relaunch back for the app's answer.
#[must_use]
pub fn relaunch_pending() -> bool {
    RELAUNCH.with(|slot| slot.borrow().is_some())
}

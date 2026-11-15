//! Files the system asks FilmCraft to open: a Finder double-click, `open -a FilmCraft file`, a file
//! dropped on the Dock icon. macOS delivers these as an "open documents" Apple Event, never as
//! command-line arguments, and winit does not handle that event, so without this module FilmCraft
//! started with no project.
//!
//! This is a non-media use of `unsafe` in this crate, recorded as this fork's own decision in
//! [`docs/adr/fork-0001-macos-open-documents.md`](../../../docs/adr/fork-0001-macos-open-documents.md):
//! Objective-C needs an object of our own class to receive the event. The `unsafe` is confined to
//! the macOS-only `appkit` module below (the only scope with `#[allow(unsafe_code)]`), each block
//! says why it is sound, and the public API is safe: [`listen`] hands back a receiver of paths.
//!
//! How: the handler is installed with `NSAppleEventManager` for the open-documents event before
//! the event loop starts, and again when the application posts
//! `NSApplicationWillFinishLaunchingNotification` (Apple's documented place, after AppKit's own
//! handlers and before the event that launched the app is delivered). It needs neither `NSApp`
//! nor its delegate, which winit owns. Elsewhere [`listen`] returns an error and the app uses its
//! command line only.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

/// Called after files arrive (the UI uses it to wake a hidden window).
type Wake = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

/// The files the system asked to open, in the order they came. `Send`: it goes to the app.
pub struct OpenDocuments {
    rx: Receiver<PathBuf>,
    wake: Wake,
}

impl OpenDocuments {
    /// The files that arrived since the last call.
    pub fn take(&self) -> Vec<PathBuf> {
        self.rx.try_iter().collect()
    }

    /// Call `wake` whenever files arrive (from the main thread, during the event).
    pub fn on_arrival(&self, wake: impl Fn() + Send + 'static) {
        *self.wake.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(wake));
    }
}

/// Keeps the handler installed. Dropping it removes the handler; keep it for the life of the
/// event loop, on the main thread.
pub struct Listening {
    #[cfg(target_os = "macos")]
    _handler: appkit::Installed,
}

/// The sending side, shared by the event handler and the tests.
struct Deliver {
    tx: Sender<PathBuf>,
    wake: Wake,
}

impl Deliver {
    fn new() -> (Self, OpenDocuments) {
        let (tx, rx) = channel();
        let wake: Wake = Arc::default();
        (Self { tx, wake: Arc::clone(&wake) }, OpenDocuments { rx, wake })
    }

    /// Hand `paths` to the app and wake it. Nothing here can panic (it runs inside an
    /// Objective-C callback); a closed receiver (the app is gone) drops them.
    fn send(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        for p in paths {
            let _ = self.tx.send(p);
        }
        if let Some(wake) = self.wake.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            wake();
        }
    }
}

/// Start receiving the files the system asks FilmCraft to open. Call it on the main thread before
/// the event loop runs, and keep the returned [`Listening`] until the loop has ended.
pub fn listen() -> Result<(Listening, OpenDocuments), String> {
    #[cfg(target_os = "macos")]
    {
        let (deliver, documents) = Deliver::new();
        let installed = appkit::install(deliver)?;
        Ok((Listening { _handler: installed }, documents))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("opening files from the system is handled through the command line here".into())
    }
}

/// The paths of the file descriptors in an Apple Event's direct object: a list of files, or one
/// file. Anything that is not a file URL is skipped. At most [`MAX_FILES`] are taken.
#[cfg(target_os = "macos")]
fn paths_in(direct: &objc2_foundation::NSAppleEventDescriptor) -> Vec<PathBuf> {
    let one = |d: &objc2_foundation::NSAppleEventDescriptor| d.fileURLValue().and_then(|u| u.path()).map(|p| PathBuf::from(p.to_string()));
    let n = direct.numberOfItems();
    if n <= 0 {
        return one(direct).into_iter().collect();
    }
    // descriptors are numbered from 1
    (1..=n.min(MAX_FILES)).filter_map(|i| direct.descriptorAtIndex(i)).filter_map(|d| one(&d)).collect()
}

/// More files than anyone selects in Finder; the rest of a larger event is ignored.
#[cfg(target_os = "macos")]
const MAX_FILES: isize = 4096;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod appkit {
    use super::{Deliver, paths_in};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject};
    use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send, sel};
    use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSNotification, NSNotificationCenter, NSString};

    /// `kCoreEventClass` ('aevt'), `kAEOpenDocuments` ('odoc') and `keyDirectObject` ('----').
    const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
    const OPEN_DOCUMENTS: u32 = u32::from_be_bytes(*b"odoc");
    const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
    const WILL_FINISH_LAUNCHING: &str = "NSApplicationWillFinishLaunchingNotification";

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and `Handler` does not implement
        // `Drop` (its ivars are plain Rust values).
        #[unsafe(super(NSObject))]
        #[name = "FilmCraftOpenDocumentsHandler"]
        #[ivars = Deliver]
        struct Handler;

        impl Handler {
            // SAFETY: the selector matches the signature NSAppleEventManager calls handlers with
            // (two NSAppleEventDescriptor objects, no return value), and the body cannot unwind.
            #[unsafe(method(handleOpenDocuments:withReplyEvent:))]
            fn handle_open_documents(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
                // SAFETY: `paramDescriptorForKeyword:` takes an `AEKeyword` (a 32-bit four-char
                // code) and returns a possibly-nil, autoreleased NSAppleEventDescriptor, which
                // `Option<Retained<_>>` retains.
                let direct: Option<Retained<NSAppleEventDescriptor>> = unsafe { msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT] };
                if let Some(direct) = direct {
                    self.ivars().send(paths_in(&direct));
                }
            }

            // SAFETY: notification observers are called with one NSNotification argument and no
            // return value.
            #[unsafe(method(willFinishLaunching:))]
            fn will_finish_launching(&self, _note: &NSNotification) {
                // AppKit has installed its own handlers by now: take the event back.
                set_handler(self);
            }
        }
    );

    /// Route the open-documents event to `handler`.
    fn set_handler(handler: &Handler) {
        let manager = NSAppleEventManager::sharedAppleEventManager();
        // SAFETY: `handler` is a live object whose class implements the selector with the
        // signature the manager calls (above); the event class and ID are 32-bit codes. The
        // manager does not retain the handler: `Installed` keeps it alive and removes the handler
        // before releasing it.
        unsafe {
            let _: () = msg_send![
                &*manager,
                setEventHandler: handler,
                andSelector: sel!(handleOpenDocuments:withReplyEvent:),
                forEventClass: CORE_EVENT_CLASS,
                andEventID: OPEN_DOCUMENTS
            ];
        }
    }

    /// The installed handler; removing it on drop.
    pub(super) struct Installed {
        handler: Retained<Handler>,
    }

    pub(super) fn install(deliver: Deliver) -> Result<Installed, String> {
        let handler: Retained<Handler> = {
            let this = Handler::alloc().set_ivars(deliver);
            // SAFETY: `init` on a freshly allocated NSObject subclass whose ivars are set.
            unsafe { msg_send![super(this), init] }
        };
        set_handler(&handler);
        let name = NSString::from_str(WILL_FINISH_LAUNCHING);
        let center = NSNotificationCenter::defaultCenter();
        // SAFETY: `handler` implements `willFinishLaunching:` taking one NSNotification; the
        // center does not retain observers, and `Installed::drop` removes it before the handler
        // is released.
        unsafe {
            center.addObserver_selector_name_object(&handler, sel!(willFinishLaunching:), Some(&name), None);
        }
        Ok(Installed { handler })
    }

    impl Drop for Installed {
        fn drop(&mut self) {
            let center = NSNotificationCenter::defaultCenter();
            let observer: &AnyObject = &self.handler;
            // SAFETY: removes the observer this module added, while it is still alive.
            unsafe { center.removeObserver(observer) };
            let manager = NSAppleEventManager::sharedAppleEventManager();
            // SAFETY: the event class and ID are 32-bit codes; the manager then holds no
            // reference to the handler.
            unsafe {
                let _: () = msg_send![&*manager, removeEventHandlerForEventClass: CORE_EVENT_CLASS, andEventID: OPEN_DOCUMENTS];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn files_reach_the_app_and_wake_it() {
        let (deliver, docs) = Deliver::new();
        let woken = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&woken);
        docs.on_arrival(move || {
            count.fetch_add(1, Ordering::SeqCst);
        });
        deliver.send(vec![PathBuf::from("/a/Film.fcproj"), PathBuf::from("/a/clip.mov")]);
        deliver.send(Vec::new());
        assert_eq!(docs.take(), [PathBuf::from("/a/Film.fcproj"), PathBuf::from("/a/clip.mov")]);
        assert!(docs.take().is_empty());
        assert_eq!(woken.load(Ordering::SeqCst), 1, "one wake per event, none for an empty one");
        // the app is gone: nothing panics
        drop(docs);
        deliver.send(vec![PathBuf::from("/late.fcproj")]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn file_lists_and_single_files_are_read_from_the_event() {
        use objc2_foundation::{NSAppleEventDescriptor, NSString, NSURL};
        let url = |p: &str| NSURL::fileURLWithPath(&NSString::from_str(p));
        let list = NSAppleEventDescriptor::listDescriptor();
        list.insertDescriptor_atIndex(&NSAppleEventDescriptor::descriptorWithFileURL(&url("/Films/One.fcproj")), 0);
        list.insertDescriptor_atIndex(&NSAppleEventDescriptor::descriptorWithInt32(7), 0);
        list.insertDescriptor_atIndex(&NSAppleEventDescriptor::descriptorWithFileURL(&url("/Films/two clip.mov")), 0);
        assert_eq!(paths_in(&list), [PathBuf::from("/Films/One.fcproj"), PathBuf::from("/Films/two clip.mov")]);
        let single = NSAppleEventDescriptor::descriptorWithFileURL(&url("/Films/Only.fcproj"));
        assert_eq!(paths_in(&single), [PathBuf::from("/Films/Only.fcproj")]);
        assert!(paths_in(&NSAppleEventDescriptor::listDescriptor()).is_empty());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn elsewhere_the_command_line_is_used() {
        assert!(listen().is_err());
    }
}

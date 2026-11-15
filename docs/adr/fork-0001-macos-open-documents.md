# Fork ADR 0001: macOS open-documents events in `crates/platform`

- **Status:** accepted (2026-11-14, approved by the fork owner). Fork-only: this record and the
  one-line pointer under [AGENTS.md](../../AGENTS.md) §0 item 3 are not upstream text. The
  `fork-` prefix keeps the number apart from upstream ADRs.
- **Extends:** [ADR 0001](0001-platform-ffi.md), which allows `unsafe` in `crates/platform` for OS
  media APIs only and asks for a new decision for anything else.

## Context

On macOS, Finder (a double-click on a `.fcproj`), `open -a FilmCraft file` and a drop on the Dock
icon hand the files to the app as an "open documents" Apple Event, never as command-line
arguments. winit 0.30 (and the Craft fork of it) does not handle that event, and it owns the
application delegate, so FilmCraft started without the project and ignored later opens.

Receiving the event needs an Objective-C object of a class we define, registered with
`NSAppleEventManager`. Defining the class and sending the registration messages through `objc2` is
`unsafe` Rust. There is no safe binding for it, and there is no way around it outside FFI.

## Decision

Allow one more scope in `crates/platform`: the macOS-only `appkit` module inside
[`open_documents.rs`](../../crates/platform/src/open_documents.rs). It is the only scope that
carries `#[allow(unsafe_code)]` for this purpose, and it follows ADR 0001's rules:

1. Every `unsafe` block, and each `unsafe` attribute of the class definition, has a `// SAFETY:`
   comment.
2. The public API is safe: `open_documents::listen()` returns a guard (`Listening`, which removes
   the handler when dropped) and an `OpenDocuments` receiver of `PathBuf`s. No FFI types are
   public.
3. Nothing in the callbacks can panic or unwind into Objective-C: they read file URLs from the
   event (capped at 4096) and send paths on a channel.
4. It does nothing else: no other Apple Events, no delegate, no window or menu changes.

How it works: the handler is registered before the event loop starts and again on
`NSApplicationWillFinishLaunchingNotification`. That is Apple's documented place, after AppKit's
own handlers and before the event that launched the app. The app's start-up (including its
graphics fallback loop) is unchanged. A launch with no command-line files opens what the event
sent. While running, a project goes through the usual unsaved-changes question and other files are
imported, as a drop on the window would be.

## Consequences

- Finder, `open -a` and the Dock open projects in FilmCraft on macOS. For Finder to offer
  FilmCraft for `.fcproj` files, the app bundle's `Info.plist` must also declare the document type
  (`CFBundleDocumentTypes`); that belongs to the bundle packaging, not to this code.
- Covered by tests: the path extraction from real `NSAppleEventDescriptor` lists, the delivery and
  wake-up, the launch routing, and the running app's handling (UI test). The Apple Event callback
  itself is checked by hand with a built app (`open -a <app> file.fcproj` at launch and while
  running).
- Drop this module when winit (or eframe) reports opened files itself.

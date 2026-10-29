// Copyright 2014-2021 The winit contributors
// Copyright 2021-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0

use std::{
  collections::VecDeque,
  ffi::CStr,
  panic::{AssertUnwindSafe, UnwindSafe},
  sync::atomic::{AtomicU64, Ordering},
};

use objc2::{
  exception::Exception,
  rc::Retained,
  runtime::{AnyClass as Class, ClassBuilder as ClassDecl, Sel},
};
use objc2_app_kit::{self as appkit, NSApplication, NSEvent, NSEventType};
use objc2_foundation::NSException;
use std::sync::LazyLock;

use super::{app_state::AppState, event::EventWrapper, util, DEVICE_ID};
use crate::event::{DeviceEvent, ElementState, Event};

pub struct AppClass(pub *const Class);
unsafe impl Send for AppClass {}
unsafe impl Sync for AppClass {}

pub static APP_CLASS: LazyLock<AppClass> = LazyLock::new(|| unsafe {
  let superclass = class!(NSApplication);
  let mut decl =
    ClassDecl::new(CStr::from_bytes_with_nul(b"TaoApp\0").unwrap(), superclass).unwrap();

  decl.add_method(sel!(sendEvent:), send_event as extern "C" fn(_, _, _));

  AppClass(decl.register())
});

// Normally, holding Cmd + any key never sends us a `keyUp` event for that key.
// Overriding `sendEvent:` like this fixes that. (https://stackoverflow.com/a/15294196)
// Fun fact: Firefox still has this bug! (https://bugzilla.mozilla.org/show_bug.cgi?id=1299553)
//
// This function is `extern "C"`, so an unwind cannot leave it: Rust aborts the
// process instead. AppKit and WebKit code reached through `sendEvent:` can
// throw Objective-C exceptions, and `-[NSApplication run]` has a handler for
// them above this frame, so the unwinder reached this frame and aborted.
// Catch the exception here, log its name, and drop the event, which is what
// stock AppKit does for an exception raised during event dispatch.
extern "C" fn send_event(this: &NSApplication, _sel: Sel, event: &NSEvent) {
  let result = catch_objc_exception(AssertUnwindSafe(|| unsafe {
    send_event_unguarded(this, event)
  }));
  if let Err(caught) = result {
    log_caught_exception(&caught);
  }
}

unsafe fn send_event_unguarded(this: &NSApplication, event: &NSEvent) {
  // For posterity, there are some undocumented event types
  // (https://github.com/servo/cocoa-rs/issues/155)
  // but that doesn't really matter here.
  let event_type = event.r#type();
  let modifier_flags = event.modifierFlags();
  if event_type == appkit::NSKeyUp
    && util::has_flag(modifier_flags, appkit::NSEventModifierFlags::Command)
  {
    if let Some(key_window) = this.keyWindow() {
      key_window.sendEvent(event);
    } else {
      log::debug!("skip sending CMD keyEvent - app has no keyWindow");
    }
  } else {
    maybe_dispatch_device_event(event);
    let superclass = util::superclass(this);
    let _: () = msg_send![super(this, superclass), sendEvent: event];
  }
}

/// The most characters of an exception name that reach the log.
const MAX_EXCEPTION_NAME_LEN: usize = 64;

/// After this many caught exceptions, only every `LOG_EVERY_NTH_AFTER_CAP`th
/// one is logged, so an exception on every mouse move cannot flood the log.
const LOG_FIRST_N_EXCEPTIONS: u64 = 10;
const LOG_EVERY_NTH_AFTER_CAP: u64 = 1000;

static CAUGHT_EXCEPTIONS: AtomicU64 = AtomicU64::new(0);

/// An Objective-C exception caught during event dispatch. It keeps only the
/// sanitized exception name. The reason text can carry user content, so this
/// code never reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaughtException {
  pub(crate) name: String,
}

/// Runs `f` and catches an Objective-C exception thrown inside it. A Rust
/// panic inside `f` passes through unchanged.
pub(crate) fn catch_objc_exception(f: impl FnOnce() + UnwindSafe) -> Result<(), CaughtException> {
  objc2::exception::catch(f).map_err(|exception| CaughtException {
    name: exception_name(exception),
  })
}

fn exception_name(exception: Option<Retained<Exception>>) -> String {
  let Some(exception) = exception else {
    return "<nil>".to_owned();
  };
  match NSException::from_exception(exception) {
    Ok(exception) => sanitize_exception_name(&exception.name().to_string()),
    Err(_) => "<non-NSException>".to_owned(),
  }
}

/// Keeps ASCII letters, digits, `_`, `.` and `-`, and caps the length.
/// Exception names are developer constants such as
/// `NSInvalidArgumentException`, so they pass through intact while anything
/// unusual stays bounded.
pub(crate) fn sanitize_exception_name(name: &str) -> String {
  let sanitized: String = name
    .chars()
    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    .take(MAX_EXCEPTION_NAME_LEN)
    .collect();
  if sanitized.is_empty() {
    "<unnamed>".to_owned()
  } else {
    sanitized
  }
}

fn log_caught_exception(caught: &CaughtException) {
  let count = CAUGHT_EXCEPTIONS.fetch_add(1, Ordering::Relaxed) + 1;
  if count <= LOG_FIRST_N_EXCEPTIONS || count % LOG_EVERY_NTH_AFTER_CAP == 0 {
    log::error!(
      target: "tao::send_event",
      "caught Objective-C exception during sendEvent: (name={}, count={}); event dropped",
      caught.name,
      count
    );
  }
}

unsafe fn maybe_dispatch_device_event(event: &NSEvent) {
  let event_type = event.r#type();
  match event_type {
    NSEventType::MouseMoved
    | NSEventType::LeftMouseDragged
    | NSEventType::OtherMouseDragged
    | NSEventType::RightMouseDragged => {
      let mut events = VecDeque::with_capacity(3);

      let delta_x = event.deltaX() as f64;
      let delta_y = event.deltaY() as f64;

      if delta_x != 0.0 {
        events.push_back(EventWrapper::StaticEvent(Event::DeviceEvent {
          device_id: DEVICE_ID,
          event: DeviceEvent::Motion {
            axis: 0,
            value: delta_x,
          },
        }));
      }

      if delta_y != 0.0 {
        events.push_back(EventWrapper::StaticEvent(Event::DeviceEvent {
          device_id: DEVICE_ID,
          event: DeviceEvent::Motion {
            axis: 1,
            value: delta_y,
          },
        }));
      }

      if delta_x != 0.0 || delta_y != 0.0 {
        events.push_back(EventWrapper::StaticEvent(Event::DeviceEvent {
          device_id: DEVICE_ID,
          event: DeviceEvent::MouseMotion {
            delta: (delta_x, delta_y),
          },
        }));
      }

      AppState::queue_events(events);
    }
    NSEventType::LeftMouseDown | NSEventType::RightMouseDown | NSEventType::OtherMouseDown => {
      let mut events = VecDeque::with_capacity(1);

      events.push_back(EventWrapper::StaticEvent(Event::DeviceEvent {
        device_id: DEVICE_ID,
        event: DeviceEvent::Button {
          button: event.buttonNumber() as u32,
          state: ElementState::Pressed,
        },
      }));

      AppState::queue_events(events);
    }
    NSEventType::LeftMouseUp | NSEventType::RightMouseUp | NSEventType::OtherMouseUp => {
      let mut events = VecDeque::with_capacity(1);

      events.push_back(EventWrapper::StaticEvent(Event::DeviceEvent {
        device_id: DEVICE_ID,
        event: DeviceEvent::Button {
          button: event.buttonNumber() as u32,
          state: ElementState::Released,
        },
      }));

      AppState::queue_events(events);
    }
    _ => (),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use objc2_foundation::NSString;
  use std::os::unix::process::ExitStatusExt;
  use std::process::Command;

  const DEATH_TEST_ENV: &str = "TAO_SEND_EVENT_DEATH_TEST";
  const GUARDED_CHILD_SURVIVED: &str = "guarded child survived";

  fn raise_test_exception() {
    let name = NSString::from_str("NSInvalidArgumentException");
    let reason = NSString::from_str("reason text that must not reach the log");
    NSException::new(&name, Some(&reason), None)
      .expect("create NSException")
      .raise();
  }

  // Stands in for the old `send_event`: an `extern "C"` frame with no guard.
  extern "C" fn unguarded_callback() {
    raise_test_exception();
  }

  // Stands in for the new `send_event`: the same frame with the guard.
  extern "C" fn guarded_callback() {
    let result = catch_objc_exception(raise_test_exception);
    assert_eq!(
      result,
      Err(CaughtException {
        name: "NSInvalidArgumentException".to_owned()
      })
    );
  }

  #[test]
  fn catch_returns_ok_without_exception() {
    let mut ran = false;
    let result = catch_objc_exception(AssertUnwindSafe(|| ran = true));
    assert_eq!(result, Ok(()));
    assert!(ran);
  }

  #[test]
  fn catch_returns_exception_name_only() {
    let result = catch_objc_exception(raise_test_exception);
    let caught = result.expect_err("exception should be caught");
    assert_eq!(caught.name, "NSInvalidArgumentException");
    assert!(!format!("{caught:?}").contains("reason text"));
  }

  #[test]
  fn sanitize_keeps_plain_names() {
    assert_eq!(
      sanitize_exception_name("NSInternalInconsistencyException"),
      "NSInternalInconsistencyException"
    );
    assert_eq!(
      sanitize_exception_name("com.example.Some_Name-1"),
      "com.example.Some_Name-1"
    );
  }

  #[test]
  fn sanitize_strips_unexpected_characters_and_caps_length() {
    assert_eq!(sanitize_exception_name("bad name: x@y"), "badnamexy");
    assert_eq!(sanitize_exception_name(" :/ "), "<unnamed>");
    assert_eq!(
      sanitize_exception_name(&"a".repeat(500)).len(),
      MAX_EXCEPTION_NAME_LEN
    );
  }

  /// Runs only as a child of `exception_in_extern_c_frame_aborts_only_without_guard`.
  #[test]
  fn death_test_child() {
    let Ok(mode) = std::env::var(DEATH_TEST_ENV) else {
      return;
    };
    // `-[NSApplication run]` wraps event dispatch in a handler. This outer
    // catch plays that part, so the unwind reaches the `extern "C"` frame.
    let callback: extern "C" fn() = match mode.as_str() {
      "unguarded" => unguarded_callback,
      "guarded" => guarded_callback,
      other => panic!("unknown death test mode {other}"),
    };
    let outer = objc2::exception::catch(|| callback());
    assert!(outer.is_ok(), "the exception escaped the guard");
    println!("{GUARDED_CHILD_SURVIVED}");
  }

  fn run_death_test_child(mode: &str) -> std::process::Output {
    let module = module_path!()
      .split_once("::")
      .map(|(_, rest)| rest)
      .expect("module path has a crate prefix");
    Command::new(std::env::current_exe().expect("current test binary"))
      .args([
        "--exact",
        &format!("{module}::death_test_child"),
        "--nocapture",
        "--test-threads=1",
      ])
      .env(DEATH_TEST_ENV, mode)
      .output()
      .expect("spawn death test child")
  }

  #[test]
  fn exception_in_extern_c_frame_aborts_only_without_guard() {
    let unguarded = run_death_test_child("unguarded");
    assert_eq!(
      unguarded.status.signal(),
      Some(libc::SIGABRT),
      "the unguarded child should abort, got {:?}",
      unguarded.status
    );

    let guarded = run_death_test_child("guarded");
    assert!(
      guarded.status.success(),
      "the guarded child should exit 0, got {:?}",
      guarded.status
    );
    assert!(String::from_utf8_lossy(&guarded.stdout).contains(GUARDED_CHILD_SURVIVED));
  }
}

---
"tao": patch
---

Windows: don't re-enter the event handler from the `WM_ENDSESSION` arm when it is already taken. `WM_ENDSESSION` can arrive during a nested Win32 message pump, such as WebView2 creation inside `Event::NewEvents(StartCause::Init)`, where `event_handler` is `None`. Calling `loop_destroyed` there re-entered `call_event_handler` and panicked instead of exiting. The arm now skips `loop_destroyed` when the runner says the handler is unavailable and still calls `process::exit(0)`. A nested Win32 message pump already interrupts the loop, and Windows may terminate the process as soon as `WM_ENDSESSION` returns, so deferring the message is not an option. Top-level session end and canceled session end behave as before.

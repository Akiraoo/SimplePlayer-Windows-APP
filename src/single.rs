//! One running copy only. Starting Simple Player again (e.g. while it sits in the tray)
//! wakes the running copy instead of opening a second one in the background.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(attrs: *const c_void, initial_owner: i32, name: *const u16) -> Handle;
        fn CreateEventW(
            attrs: *const c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> Handle;
        fn SetEvent(event: Handle) -> i32;
        fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
        fn GetLastError() -> u32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn AllowSetForegroundWindow(process_id: u32) -> i32;
    }

    const ERROR_ALREADY_EXISTS: u32 = 183;
    const WAIT_OBJECT_0: u32 = 0;
    const ASFW_ANY: u32 = u32::MAX;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Kept alive for the whole run (the handles are released when the process exits).
    pub struct Instance {
        show_event: Handle,
    }

    /// `None` = another copy is already running; it has been asked to show its window.
    pub fn acquire() -> Option<Instance> {
        acquire_named("SimplePlayer.SingleInstance", "SimplePlayer.ShowWindow")
    }

    /// Same with own names (the mini player is a separate single-instance program).
    pub fn acquire_named(mutex: &str, event: &str) -> Option<Instance> {
        unsafe {
            let event_name = wide(&format!("Local\\{event}"));
            let show_event = CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr());
            let mutex_name = wide(&format!("Local\\{mutex}"));
            let mutex = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
            if !mutex.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
                // let the running copy take the foreground, then wake it up
                AllowSetForegroundWindow(ASFW_ANY);
                if !show_event.is_null() {
                    SetEvent(show_event);
                }
                return None;
            }
            Some(Instance { show_event })
        }
    }

    impl Instance {
        /// True once each time another launch asked us to show the window.
        pub fn show_requested(&self) -> bool {
            !self.show_event.is_null()
                && unsafe { WaitForSingleObject(self.show_event, 0) } == WAIT_OBJECT_0
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub struct Instance;
    pub fn acquire() -> Option<Instance> {
        Some(Instance)
    }
    pub fn acquire_named(_: &str, _: &str) -> Option<Instance> {
        Some(Instance)
    }
    impl Instance {
        pub fn show_requested(&self) -> bool {
            false
        }
    }
}

pub use imp::{acquire, acquire_named, Instance};

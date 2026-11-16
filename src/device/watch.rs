//! Hot-plug notifications: tells the UI when to refresh the drive list.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::thread::JoinHandle;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Ioctl::{GUID_DEVINTERFACE_DISK, GUID_DEVINTERFACE_VOLUME};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, KillTimer, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterDeviceNotificationW, SetTimer, SetWindowLongPtrW, UnregisterDeviceNotification,
    DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_DEVICEINTERFACE,
    DEVICE_NOTIFY_WINDOW_HANDLE, DEV_BROADCAST_DEVICEINTERFACE_W, GWLP_USERDATA, HDEVNOTIFY, HMENU,
    HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_DESTROY, WM_DEVICECHANGE,
    WM_TIMER, WNDCLASSW,
};

/// Bursts of notifications (one stick produces several) are folded into one
/// callback this many milliseconds after the first of the burst.
const DEBOUNCE_MS: u32 = 500;
const TIMER_ID: usize = 1;

/// Private message that is treated like a device arrival. The system refuses
/// to post a real `WM_DEVICECHANGE` (it carries a pointer), so the tests use
/// this to exercise the debounce path.
const WM_SIMULATED_CHANGE: u32 = 0x8000 + 0x0BF1; // WM_APP + n

/// Per-window state, owned by the watcher thread and reached from the window
/// procedure through `GWLP_USERDATA`.
struct State {
    on_change: Box<dyn Fn()>,
    /// A debounce timer is running.
    pending: bool,
}

/// Watches for disks and volumes arriving or leaving.
///
/// A background thread owns a hidden message-only window registered for
/// disk and volume device-interface notifications. `on_change` is called on
/// that thread, at most once per ~500 ms, after something changed; it should
/// only do something cheap such as requesting a repaint or setting a flag.
/// Dropping the `Watcher` stops and joins the thread.
pub struct Watcher {
    /// The window handle as an integer (0 is never stored).
    hwnd: Option<isize>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Starts watching. If the notification window cannot be created the
    /// watcher is inert: `on_change` is simply never called.
    pub fn spawn(on_change: impl Fn() + Send + 'static) -> Watcher {
        let (tx, rx) = mpsc::channel::<isize>();
        let thread = std::thread::Builder::new()
            .name("bootforge-device-watcher".into())
            .spawn(move || run(Box::new(on_change), tx))
            .ok();
        // The thread reports its window (or 0) once it is set up; if it could
        // not even start, the sender is gone and `recv` fails.
        let hwnd = rx.recv().ok().filter(|h| *h != 0);
        Watcher { hwnd, thread }
    }

    /// Whether notifications are actually being received.
    pub fn is_active(&self) -> bool {
        self.hwnd.is_some()
    }

    #[cfg(test)]
    fn simulate_arrival(&self) {
        if let Some(hwnd) = self.hwnd {
            // SAFETY: posting a message to our own window.
            unsafe {
                let _ = PostMessageW(
                    HWND(hwnd as *mut core::ffi::c_void),
                    WM_SIMULATED_CHANGE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some(hwnd) = self.hwnd.take() {
            // SAFETY: posting to a window of our own thread; if the window is
            // already gone the call fails harmlessly and the thread has ended.
            unsafe {
                let _ = PostMessageW(
                    HWND(hwnd as *mut core::ffi::c_void),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Registers `hwnd` for arrival/removal notifications of one interface class.
unsafe fn register(hwnd: HWND, class: windows::core::GUID) -> Option<HDEVNOTIFY> {
    let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
        dbcc_size: std::mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
        dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
        dbcc_reserved: 0,
        dbcc_classguid: class,
        dbcc_name: [0],
    };
    RegisterDeviceNotificationW(
        HANDLE(hwnd.0),
        &filter as *const DEV_BROADCAST_DEVICEINTERFACE_W as *const core::ffi::c_void,
        DEVICE_NOTIFY_WINDOW_HANDLE,
    )
    .ok()
}

/// Body of the watcher thread: create the window, pump messages until it is
/// destroyed, clean up.
fn run(on_change: Box<dyn Fn()>, ready: mpsc::Sender<isize>) {
    // SAFETY: ordinary window creation and message loop, all on this thread.
    // `state` is a leaked Box that outlives the window and is reclaimed after
    // the loop, when no more messages can reach the window procedure.
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(PCWSTR::null())
            .map(Into::into)
            .unwrap_or_default();
        let class_name = w!("BootForgeDeviceWatcher");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        // Fails harmlessly when a previous watcher already registered the class.
        RegisterClassW(&class);
        let created = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("BootForge device watcher"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            // A message-only window: never visible, not in the taskbar.
            HWND_MESSAGE,
            HMENU::default(),
            instance,
            None,
        );
        let hwnd = match created {
            Ok(h) if !h.0.is_null() => h,
            _ => {
                let _ = ready.send(0);
                return;
            }
        };
        let state = Box::into_raw(Box::new(State {
            on_change,
            pending: false,
        }));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        let notifications = [
            register(hwnd, GUID_DEVINTERFACE_DISK),
            register(hwnd, GUID_DEVINTERFACE_VOLUME),
        ];
        let _ = ready.send(hwnd.0 as isize);

        let mut msg = MSG::default();
        // GetMessage returns 0 on WM_QUIT and -1 on error; stop on both.
        while GetMessageW(&mut msg, HWND::default(), 0, 0).0 > 0 {
            DispatchMessageW(&msg);
        }

        for n in notifications.into_iter().flatten() {
            let _ = UnregisterDeviceNotification(n);
        }
        drop(Box::from_raw(state));
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    match msg {
        WM_DEVICECHANGE | WM_SIMULATED_CHANGE => {
            let event = wparam.0 as u32;
            let relevant = msg == WM_SIMULATED_CHANGE
                || event == DBT_DEVICEARRIVAL
                || event == DBT_DEVICEREMOVECOMPLETE;
            if relevant && !state.is_null() && !(*state).pending {
                // Start the debounce timer; further events of the burst are
                // absorbed until it fires.
                (*state).pending = SetTimer(hwnd, TIMER_ID, DEBOUNCE_MS, None) != 0;
            }
            LRESULT(1)
        }
        WM_TIMER if wparam.0 == TIMER_ID => {
            let _ = KillTimer(hwnd, TIMER_ID);
            if !state.is_null() {
                (*state).pending = false;
                // A panic must not unwind into the system's message dispatcher.
                let _ = catch_unwind(AssertUnwindSafe(|| ((*state).on_change)()));
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = KillTimer(hwnd, TIMER_ID);
            // Detach the state first so late messages cannot reach it.
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn spawn_and_drop() {
        let watcher = Watcher::spawn(|| {});
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        drop(watcher);
        // Dropping stops the thread promptly.
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_burst_of_events_gives_one_callback() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let watcher = Watcher::spawn(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        if !watcher.is_active() {
            return; // no window station (unusual service context): nothing to test
        }
        for _ in 0..6 {
            watcher.simulate_arrival();
        }
        // Nothing fires before the debounce interval has passed.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(300));
        // One call for the burst (a real device event during the test could add one).
        let n = calls.load(Ordering::SeqCst);
        assert!((1..=2).contains(&n), "{n} callbacks");
        // A later event is reported again.
        watcher.simulate_arrival();
        let deadline = Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) == n && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(calls.load(Ordering::SeqCst) > n);
        drop(watcher);
        let after = calls.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(calls.load(Ordering::SeqCst), after, "callback after drop");
    }
}

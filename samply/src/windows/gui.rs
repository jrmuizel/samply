use winsafe::{self as w, co, prelude::*};
use winsafe::msg;

/// Stores the profile path produced by the UI recording thread so the main
/// thread can serve it after the window closes.
static UI_RESULT_PATH: std::sync::Mutex<Option<std::path::PathBuf>> =
    std::sync::Mutex::new(None);

const CLASS_NAME: &str = "SamplyWindowClass";
const RECORD_BUTTON_ID: u16 = 1001;
const CONFIGURE_BTN_ID: u16 = 1002;
const BROWSERS_CHECK_ID: u16 = 1003;
const GRAPHICS_CHECK_ID: u16 = 1004;
const SYMBOLS_SERVER_CHECK_ID: u16 = 1005;
const MOZILLA_SERVER_CHECK_ID: u16 = 1006;

const CW_USEDEFAULT: i32 = 0x80000000_u32 as i32;

const WINDOW_W: i32 = 320;
const COLLAPSED_H: i32 = 140;
const EXPANDED_H: i32 = 315;

unsafe fn create_button(
    parent: &w::HWND,
    text: &str,
    style: co::WS,
    x: i32, y: i32, cw: i32, ch: i32,
    id: u16,
) -> Option<w::HWND> {
    unsafe {
        w::HWND::CreateWindowEx(
            co::WS_EX::NoValue,
            w::AtomStr::from_str("BUTTON"),
            Some(text),
            style,
            w::POINT { x, y },
            w::SIZE { cx: cw, cy: ch },
            Some(parent),
            w::IdMenu::Id(id),
            &w::HINSTANCE::NULL,
            None,
        ).ok()
    }
}

unsafe fn create_static(
    parent: &w::HWND,
    text: &str,
    style: co::WS,
    x: i32, y: i32, cw: i32, ch: i32,
) -> Option<w::HWND> {
    unsafe {
        w::HWND::CreateWindowEx(
            co::WS_EX::NoValue,
            w::AtomStr::from_str("STATIC"),
            Some(text),
            style,
            w::POINT { x, y },
            w::SIZE { cx: cw, cy: ch },
            Some(parent),
            w::IdMenu::None,
            &w::HINSTANCE::NULL,
            None,
        ).ok()
    }
}

unsafe fn get_state_mut(hwnd: &w::HWND) -> Option<&'static mut UiState> {
    (hwnd.GetWindowLongPtr(co::GWLP::USERDATA) as *mut UiState).as_mut()
}

unsafe fn is_checked(hwnd: Option<&w::HWND>) -> bool {
    if let Some(h) = hwnd {
        (unsafe { h.SendMessage(msg::bm::GetCheck {}) }) == co::BST::CHECKED
    } else {
        false
    }
}

// Wraps an HWND as a usize so it can be sent across threads.
// HWND is process-wide and safe to use across threads for PostMessage.
struct SendHwnd(usize);
unsafe impl Send for SendHwnd {}

struct UiState {
    stop_tx: Option<std::sync::mpsc::SyncSender<()>>,
    button_hwnd: Option<w::HWND>,
    configure_expanded: bool,
    configure_btn: Option<w::HWND>,
    providers_label: Option<w::HWND>,
    browsers_check: Option<w::HWND>,
    graphics_check: Option<w::HWND>,
    symbols_label: Option<w::HWND>,
    symbols_server_check: Option<w::HWND>,
    mozilla_server_check: Option<w::HWND>,
}

// The window has two sections:
//   - Always visible: "Start Recording" button and a "More options" toggle.
//   - Collapsible area with the additional options
//
// Overall flow:
//   WM_COMMAND/RECORD_BUTTON_ID → spawn recording thread, pass a SyncSender
//     so the thread blocks until stop is requested.
//   Second click → drop the SyncSender, unblocking the thread.
//   Recording thread → saves the profile, then posts WM_APP back to the window.
//   WM_APP → resets the button, reads symbol-server checkboxes, spawns a
//     server thread that opens the finished profile in the browser.
//
// UiState is heap-allocated and stored in the window's GWLP_USERDATA slot;
// WM_DESTROY drops it.
extern "system" fn window_proc(
    hwnd: w::HWND,
    msg_id: co::WM,
    wparam: usize,
    lparam: isize,
) -> isize {
    match msg_id {
        co::WM::CREATE => unsafe {
            let rc = hwnd.GetClientRect().unwrap_or_default();
            let client_w = rc.right - rc.left;
            let btn_w = 140i32;
            let cfg_btn_w = 130i32;
            let btn_x = (client_w - btn_w) / 2;
            let cfg_btn_x = (client_w - cfg_btn_w) / 2;

            let vis_btn = co::WS::CHILD | co::WS::VISIBLE
                | co::WS::from_raw(co::BS::PUSHBUTTON.raw());
            let button_hwnd = create_button(
                &hwnd, "Start Recording", vis_btn, btn_x, 14, btn_w, 32, RECORD_BUTTON_ID,
            );
            let configure_btn = create_button(
                &hwnd, "More options \u{25BC}", vis_btn, cfg_btn_x, 58, cfg_btn_w, 24,
                CONFIGURE_BTN_ID,
            );

            // Config section controls start hidden (no WS_VISIBLE).
            let hidden = co::WS::CHILD;
            let hidden_check = co::WS::CHILD | co::WS::from_raw(co::BS::AUTOCHECKBOX.raw());
            let providers_label = create_static(&hwnd, "Providers:", hidden, 15, 96, 100, 20);
            let browsers_check =
                create_button(&hwnd, "Browsers", hidden_check, 25, 120, 150, 20, BROWSERS_CHECK_ID);
            let graphics_check =
                create_button(&hwnd, "Graphics", hidden_check, 25, 145, 150, 20, GRAPHICS_CHECK_ID);
            let symbols_label = create_static(&hwnd, "Symbols:", hidden, 15, 175, 100, 20);
            let symbols_server_check = create_button(
                &hwnd, "Use Microsoft symbol server", hidden_check, 25, 200, 250, 20,
                SYMBOLS_SERVER_CHECK_ID,
            );
            let mozilla_server_check = create_button(
                &hwnd, "Use Mozilla symbol server", hidden_check, 25, 225, 250, 20,
                MOZILLA_SERVER_CHECK_ID,
            );

            if let Ok(font) = w::HFONT::GetStockObject(co::STOCK_FONT::DEFAULT_GUI) {
                for ctrl in [
                    button_hwnd.as_ref(), configure_btn.as_ref(), providers_label.as_ref(),
                    browsers_check.as_ref(), graphics_check.as_ref(), symbols_label.as_ref(),
                    symbols_server_check.as_ref(), mozilla_server_check.as_ref(),
                ] {
                    if let Some(h) = ctrl {
                        h.SendMessage(msg::wm::SetFont { hfont: font.raw_copy(), redraw: true });
                    }
                }
            }

            let state = Box::new(UiState {
                stop_tx: None,
                button_hwnd,
                configure_expanded: false,
                configure_btn,
                providers_label,
                browsers_check,
                graphics_check,
                symbols_label,
                symbols_server_check,
                mozilla_server_check,
            });
            hwnd.SetWindowLongPtr(co::GWLP::USERDATA, Box::into_raw(state) as isize);
            0
        }
        co::WM::COMMAND => {
            let control_id = (wparam & 0xFFFF) as u16;
            let notification = ((wparam >> 16) & 0xFFFF) as u16;

            if control_id == CONFIGURE_BTN_ID && notification == co::BN::CLICKED.raw() {
                let Some(state) = (unsafe { get_state_mut(&hwnd) }) else {
                    return 0;
                };
                state.configure_expanded = !state.configure_expanded;
                let expanded = state.configure_expanded;
                let show_cmd = if expanded { co::SW::SHOW } else { co::SW::HIDE };
                let label = if expanded {
                    "Fewer options \u{25B2}"
                } else {
                    "More options \u{25BC}"
                };

                for ctrl in [
                    state.providers_label.as_ref(), state.browsers_check.as_ref(),
                    state.graphics_check.as_ref(), state.symbols_label.as_ref(),
                    state.symbols_server_check.as_ref(), state.mozilla_server_check.as_ref(),
                ] {
                    if let Some(h) = ctrl {
                        h.ShowWindow(show_cmd);
                    }
                }
                if let Some(btn) = &state.configure_btn {
                    let _ = btn.SetWindowText(label);
                }
                let new_h = if expanded { EXPANDED_H } else { COLLAPSED_H };
                let _ = hwnd.SetWindowPos(
                    w::HwndPlace::None,
                    w::POINT { x: 0, y: 0 },
                    w::SIZE { cx: WINDOW_W, cy: new_h },
                    co::SWP::NOMOVE | co::SWP::NOZORDER,
                );
                return 0;
            }

            if control_id == RECORD_BUTTON_ID && notification == co::BN::CLICKED.raw() {
                let send_hwnd = SendHwnd(hwnd.ptr() as usize);
                let Some(state) = (unsafe { get_state_mut(&hwnd) }) else {
                    return 0;
                };

                if state.stop_tx.is_some() {
                    // Drop the sender to unblock the recording thread's recv().
                    state.stop_tx = None;
                    if let Some(btn) = &state.button_hwnd {
                        let _ = btn.SetWindowText("Processing...");
                    }
                } else {
                    // Read checkbox states before spawning the recording thread.
                    let gfx = unsafe { is_checked(state.graphics_check.as_ref()) };
                    let browsers = unsafe { is_checked(state.browsers_check.as_ref()) };
                    let unknown_event_markers = gfx;

                    let (stop_tx, stop_rx) = std::sync::mpsc::sync_channel::<()>(0);
                    state.stop_tx = Some(stop_tx);

                    let output_path = std::env::temp_dir().join("samply-profile.json.gz");

                    std::thread::spawn(move || {
                        use crate::shared::prop_types::{
                            CoreClrProfileProps, ProfileCreationProps, RecordingMode, RecordingProps,
                        };
                        let recording_props = RecordingProps {
                            output_file: output_path.clone(),
                            time_limit: None,
                            interval: std::time::Duration::from_millis(1),
                            vm_hack: false,
                            gfx,
                            browsers,
                            keep_etl: false,
                        };
                        let profile_creation_props = ProfileCreationProps {
                            profile_name: None,
                            fallback_profile_name: "UI Recording".to_string(),
                            main_thread_only: false,
                            reuse_threads: false,
                            fold_recursive_prefix: false,
                            unlink_aux_files: false,
                            create_per_cpu_threads: false,
                            arg_count_to_include_in_process_name: 0,
                            override_arch: None,
                            presymbolicate: false,
                            coreclr: CoreClrProfileProps::default(),
                            unknown_event_markers,
                            should_emit_jit_markers: false,
                            should_emit_cswitch_markers: false,
                        };
                        let success = match super::profiler::run(
                            RecordingMode::All,
                            recording_props,
                            profile_creation_props,
                            Some(stop_rx),
                        ) {
                            Ok((profile, _)) => {
                                crate::shared::save_profile::save_profile_to_file(
                                    &profile,
                                    &output_path,
                                )
                                .is_ok()
                            }
                            Err(_) => false,
                        };
                        if success {
                            *UI_RESULT_PATH.lock().unwrap() = Some(output_path);
                        }
                        let hwnd = unsafe { w::HWND::from_ptr(send_hwnd.0 as *mut _) };
                        let _ = unsafe {
                            hwnd.PostMessage(msg::WndMsg {
                                msg_id: co::WM::APP,
                                wparam: 0,
                                lparam: 0,
                            })
                        };
                    });

                    if let Some(btn) = &state.button_hwnd {
                        let _ = btn.SetWindowText("Stop Recording");
                    }
                }
                return 0;
            }

            unsafe { hwnd.DefWindowProc(msg::WndMsg { msg_id: msg_id, wparam, lparam }) }
        }
        co::WM::APP => {
            // Reset button so the user can record again.
            let mut use_ms_symbols = false;
            let mut use_mozilla_symbols = false;
            if let Some(state) = unsafe { get_state_mut(&hwnd) } {
                if let Some(btn) = &state.button_hwnd {
                    let _ = btn.SetWindowText("Start Recording");
                }
                use_ms_symbols = unsafe { is_checked(state.symbols_server_check.as_ref()) };
                use_mozilla_symbols = unsafe { is_checked(state.mozilla_server_check.as_ref()) };
            }
            // Open the profile in the browser on a background thread.
            let profile_path = UI_RESULT_PATH.lock().unwrap().take();
            if let Some(path) = profile_path {
                let mut windows_symbol_server = Vec::new();
                if use_ms_symbols {
                    windows_symbol_server
                        .push("https://msdl.microsoft.com/download/symbols".to_string());
                }
                if use_mozilla_symbols {
                    windows_symbol_server.push("https://symbols.mozilla.org/".to_string());
                }
                std::thread::spawn(move || {
                    crate::run_server_serving_profile(
                        &path,
                        crate::server::ServerProps {
                            address: "127.0.0.1".parse().unwrap(),
                            port_selection: crate::server::PortSelection::TryMultiple(3000..3100),
                            verbose: false,
                            open_in_browser: true,
                        },
                        crate::shared::prop_types::SymbolProps {
                            symbol_dir: Vec::new(),
                            windows_symbol_server,
                            windows_symbol_cache: None,
                            breakpad_symbol_server: Vec::new(),
                            breakpad_symbol_dir: Vec::new(),
                            breakpad_symbol_cache: None,
                            simpleperf_binary_cache: None,
                        },
                    );
                });
            }
            0
        }
        co::WM::DESTROY => {
            let state_ptr = hwnd.GetWindowLongPtr(co::GWLP::USERDATA) as *mut UiState;
            if !state_ptr.is_null() {
                drop(unsafe { Box::from_raw(state_ptr) });
                unsafe { hwnd.SetWindowLongPtr(co::GWLP::USERDATA, 0) };
            }
            w::PostQuitMessage(0);
            0
        }
        _ => unsafe { hwnd.DefWindowProc(msg::WndMsg { msg_id: msg_id, wparam, lparam }) },
    }
}

pub fn run() {
    let result = (|| -> w::SysResult<()> {
        unsafe {
            let mut iccx = w::INITCOMMONCONTROLSEX::default();
            iccx.icc = co::ICC::STANDARD_CLASSES;
            w::InitCommonControlsEx(&iccx)?;

            let hinstance = w::HINSTANCE::GetModuleHandle(None)?;

            // Load IDC_ARROW; DestroyCursor is a no-op for system cursors so the
            // copy we take from the guard remains valid after the guard drops.
            let cursor_guard =
                w::HINSTANCE::NULL.LoadCursor(w::IdIdcStr::Idc(co::IDC::ARROW))?;
            let hcursor = (&*cursor_guard).raw_copy();

            let mut class_name = w::WString::from_str(CLASS_NAME);
            let mut wcx = w::WNDCLASSEX::default();
            wcx.style = co::CS::HREDRAW | co::CS::VREDRAW;
            wcx.lpfnWndProc = Some(window_proc);
            wcx.hInstance = hinstance.raw_copy();
            wcx.hCursor = hcursor;
            wcx.hbrBackground = w::HBRUSH::from_sys_color(co::COLOR::BTNFACE);
            wcx.set_lpszClassName(Some(&mut class_name));

            w::SetLastError(co::ERROR::SUCCESS);
            w::RegisterClassEx(&wcx)?;

            let hwnd = w::HWND::CreateWindowEx(
                co::WS_EX::NoValue,
                w::AtomStr::from_str(CLASS_NAME),
                Some("Samply"),
                co::WS::OVERLAPPED | co::WS::CAPTION | co::WS::SYSMENU | co::WS::VISIBLE,
                w::POINT { x: CW_USEDEFAULT, y: CW_USEDEFAULT },
                w::SIZE { cx: WINDOW_W, cy: COLLAPSED_H },
                None,
                w::IdMenu::None,
                &hinstance,
                None,
            )?;

            hwnd.ShowWindow(co::SW::SHOW);

            let mut msg = w::MSG::default();
            loop {
                match w::GetMessage(&mut msg, None, 0, 0) {
                    Err(e) => return Err(e),
                    Ok(false) => break,
                    Ok(true) => {
                        w::TranslateMessage(&msg);
                        w::DispatchMessage(&msg);
                    }
                }
            }

            Ok(())
        }
    })();

    if let Err(e) = result {
        eprintln!("UI error: {e}");
    }
}

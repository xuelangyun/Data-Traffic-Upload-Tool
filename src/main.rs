#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod conn_tester;
mod downloader;
mod engine;
mod state;
mod sys_stats;
#[cfg(windows)]
mod ui;
mod uploader;

use std::sync::Arc;

use crate::engine::EngineHandle;
use crate::state::AppState;

#[cfg(windows)]
fn show_fatal_error(msg: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::{PCWSTR, w};

    let wide: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(wide.as_ptr()),
            w!("启动失败 - WinNetStress"),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn main() {
    #[cfg(windows)]
    {
        let state = Arc::new(AppState::default());
        let engine = match EngineHandle::new(state.clone()) {
            Ok(e) => e,
            Err(err) => {
                show_fatal_error(&err);
                std::process::exit(1);
            }
        };

        if let Err(err) = ui::run_gui(state, engine) {
            show_fatal_error(&err);
            std::process::exit(1);
        }
    }

    #[cfg(not(windows))]
    {
        let _state = Arc::new(AppState::default());
        let _engine_fn = EngineHandle::new;
        eprintln!(
            "win_net_stress is a native Windows Win32 API application. Build and run on Windows (e.g. `cargo build --release --target x86_64-pc-windows-msvc`)."
        );
    }
}

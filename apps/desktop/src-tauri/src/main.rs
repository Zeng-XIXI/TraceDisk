#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Some(exit_code) = tracedisk_desktop_lib::run_helper_from_args() {
        std::process::exit(exit_code);
    }
    tracedisk_desktop_lib::run();
}

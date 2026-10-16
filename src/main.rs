#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("rfm requires Windows 10 or later");

#[macro_use]
mod i18n;

mod actions;
mod breadcrumb;
mod chrome;
mod directory_events;
mod fs;
mod icons;
mod preferences;
mod preview;
mod quick_access;
mod settings;
mod shell;
mod terminal;
mod ui;
mod update_install;
mod update_net;
mod update_ui;

fn main() {
    if let Some(code) = update_install::run_helper_if_requested() {
        std::process::exit(code);
    }
    ui::run();
}

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

fn main() {
    ui::run();
}

//! dumBot: bots on the left, the selected bot's markdown in the middle, and its opencode runs
//! on the right. Bots wake on their own schedules while the app runs, each in its own Docker
//! container.
//!
//! Built without the `gui` feature it's only the command line, which is what runs inside
//! bots' containers (`dumbot wake`, `dumbot shell`).
#![cfg_attr(not(feature = "gui"), allow(dead_code))]

#[cfg(feature = "gui")]
mod app;
mod appbundle;
mod bots;
mod cli;
mod container;
mod env;
mod opencode;
mod schedule;
mod store;
mod update;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if appbundle::cli(&args) || cli::run(&args) {
        return;
    }
    #[cfg(feature = "gui")]
    app::main();
    #[cfg(not(feature = "gui"))]
    {
        eprintln!("dumbot: this build has no app; run `dumbot help`");
        std::process::exit(2);
    }
}

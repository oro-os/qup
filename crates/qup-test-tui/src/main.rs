//! ratatui-based live monitor for QUP nodes.
//!
//! The TUI stays open across disconnects, retries the configured address, and
//! repopulates a live key table whenever a connection is re-established.

#![allow(
    clippy::missing_docs_in_private_items,
    reason = "the executable is an internal utility and its private implementation details are intentionally undocumented"
)]
#![allow(
    clippy::std_instead_of_alloc,
    reason = "the TUI intentionally uses std terminal IO and collections"
)]
#![forbid(unsafe_code)]

mod app;
mod network;
mod ui;

use std::{
    io,
    time::{Duration, Instant},
};

use app::{App, handle_event, poll_input};
use clap::Parser as ClapParser;
use network::connection_manager;
use tokio::sync::mpsc;
use ui::{Tui, UiGeometry, draw};

const DEFAULT_ADDR: &str = "127.0.0.1:3400";
const DEFAULT_RETRY_SECS: u64 = 3;
const INPUT_POLL: Duration = Duration::from_millis(100);
const LAST_READ_TICK: Duration = Duration::from_secs(1);

#[derive(Debug, ClapParser)]
#[command(name = "qup-test-tui")]
struct Args {
    #[arg(short = 'a', long = "addr", default_value = DEFAULT_ADDR)]
    addr: String,
    #[arg(long = "retry-secs", default_value_t = DEFAULT_RETRY_SECS)]
    retry_secs: u64,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let worker = tokio::spawn(connection_manager(
        args.addr.clone(),
        Duration::from_secs(args.retry_secs),
        tx,
        command_rx,
    ));

    let mut terminal = Tui::new()?;
    let mut app = App::new(args.addr, args.retry_secs);
    let mut geometry = UiGeometry::default();
    let mut last_clock_redraw = Instant::now();
    let mut needs_redraw = true;

    loop {
        while let Ok(update) = rx.try_recv() {
            app.apply(update);
            needs_redraw = true;
        }

        let now = Instant::now();
        if now.duration_since(last_clock_redraw) >= LAST_READ_TICK {
            last_clock_redraw = now;
            needs_redraw = true;
        }

        if needs_redraw {
            terminal.draw(|frame| draw(frame, &app, now, &mut geometry))?;
            needs_redraw = false;
        }

        if let Some(event) = poll_input(INPUT_POLL)? {
            let outcome = handle_event(&mut app, &geometry, event, &command_tx);
            needs_redraw |= outcome.needs_redraw;
            if outcome.quit {
                break;
            }
        }
    }

    worker.abort();
    let _ = worker.await;

    Ok(())
}

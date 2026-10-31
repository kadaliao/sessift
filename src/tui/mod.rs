//! The interactive session picker (`sessift pick`, or bare `sessift`).
//!
//! It draws on /dev/tty, so a shell binding can capture stdout for the chosen command.
//! Interaction follows memex: type to search, ↓ or Enter into the list, single keys there.
//! The mouse works too: click filters, rows and hints; double-click resumes.

pub mod app;
pub mod load;
pub mod md;
pub mod theme;
mod view;

use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::fanout::Runner;
use crate::pick::ShellKind;
use app::{App, Effect, Exit};
use load::{Loader, PreviewKey};

pub struct TuiOptions {
    pub machines: Vec<String>,
    pub timeout: Duration,
    pub query: String,
    /// Set when a shell binding launched the picker; also picks the quoting style.
    pub shell: Option<ShellKind>,
}

struct Screen {
    terminal: Terminal<CrosstermBackend<File>>,
}

fn restore(tty: &mut impl Write) {
    let _ = disable_raw_mode();
    let _ = execute!(
        tty,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

impl Screen {
    fn enter() -> Result<Self> {
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .context("sessift pick needs a terminal")?;
        enable_raw_mode().context("enable raw mode")?;
        let mut out = tty.try_clone()?;
        execute!(
            out,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        // Leave the terminal usable even if something panics mid-draw.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
                restore(&mut tty);
            }
            previous(info);
        }));
        let terminal = Terminal::new(CrosstermBackend::new(tty))?;
        Ok(Self { terminal })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        restore(self.terminal.backend_mut());
    }
}

pub async fn run(runner: Arc<Runner>, options: TuiOptions) -> Result<Exit> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let loader = Loader {
        runner,
        tx,
        timeout: options.timeout,
        shell: options.shell.unwrap_or(ShellKind::Sh),
    };
    let mut app = App::new(options.machines, options.query, options.shell.is_some());
    let mut screen = Screen::enter()?;

    let (input_tx, mut input) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if input_tx.send(event).is_err() {
                break;
            }
        }
    });

    let mut fetches: Vec<JoinHandle<()>> = Vec::new();
    let mut preview: Option<(PreviewKey, JoinHandle<()>)> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let exit = loop {
        for effect in app.take_effects() {
            match effect {
                Effect::Fetch {
                    generation,
                    machines,
                    op,
                } => {
                    // Dropping the old requests cancels them on every machine.
                    for task in fetches.drain(..) {
                        task.abort();
                    }
                    fetches = loader.rows(generation, &machines, &op);
                }
                Effect::Preview(key) => {
                    if let Some((old, task)) = preview.take()
                        && !task.is_finished()
                    {
                        task.abort();
                        if matches!(app.previews.get(&old), Some(app::PreviewState::Loading)) {
                            app.previews.remove(&old);
                        }
                    }
                    let task = loader.preview(key.clone());
                    preview = Some((key, task));
                }
                Effect::Page { key, offset } => {
                    loader.reader_page(key, offset);
                }
                Effect::Command { key, purpose } => {
                    loader.command(key, purpose);
                }
                Effect::Copy(value) => {
                    if crate::pick::copy(&value).is_err() {
                        app.set_status("复制失败");
                    }
                }
            }
        }
        if let Some(exit) = app.exit.take() {
            break exit;
        }
        screen.terminal.draw(|frame| view::draw(frame, &mut app))?;
        tokio::select! {
            Some(event) = input.recv() => match event {
                Event::Key(key) => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                Event::Paste(text) => app.on_paste(&text),
                _ => {}
            },
            Some(msg) = rx.recv() => app.on_msg(msg),
            _ = tick.tick() => app.on_tick(),
        }
    };
    for task in fetches {
        task.abort();
    }
    drop(screen);
    Ok(exit)
}

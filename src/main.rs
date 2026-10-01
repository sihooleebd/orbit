//! orbit: a terminal music player.

mod app;
mod art;
mod command;
mod config;
mod download;
mod dsp;
mod ipc;
mod keymap;
mod library;
mod lyrics;
mod media;
mod notify;
mod player;
mod playlist;
mod queue;
mod radio;
mod search;
mod state;
mod theme;
mod ui;
mod visualizer;

use std::backtrace::Backtrace;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::aot::Shell;
use ratatui::backend::IntoCrossterm;
use ratatui::crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::style::{Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use ratatui::crossterm::{cursor, execute, queue, terminal};

use crate::app::{App, MsgKind};
use crate::config::{Config, Paths};
use crate::ipc::IpcServer;
use crate::keymap::{Action, Keymap};

const AFTER_HELP: &str = "\
Examples:
  orbit ~/Music/Jpop                    play a folder right away
  orbit --no-resume --volume 40%        fresh session, quieter start
  orbit ctl toggle                      control a running orbit (see `orbit commands`)
  orbit ctl status \"{artist} - {title}\" formatted status for scripts and status bars
  orbit completions zsh > ~/.zfunc/_orbit

Press ? inside orbit for keybindings (`orbit keys` lists them too).";

#[derive(Parser, Debug, Default)]
#[command(name = "orbit", version, about = "A terminal music player", after_help = AFTER_HELP)]
pub struct Cli {
    /// Files or folders to play right away (added to the queue)
    pub paths: Vec<PathBuf>,
    /// Config file to use instead of ~/.config/orbit/config.toml
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Theme for this session (see --list-themes)
    #[arg(long)]
    pub theme: Option<String>,
    /// Start volume in percent, 0-150 ("40" or "40%"), at most playback.max_volume
    #[arg(long, value_name = "PERCENT", value_parser = parse_volume)]
    pub volume: Option<u8>,
    /// Don't restore the previous session
    #[arg(long)]
    pub no_resume: bool,
    /// Ignore the metadata cache and re-read every file
    #[arg(long)]
    pub rescan: bool,
    /// Print the default config (with comments) and exit
    #[arg(long)]
    pub print_config: bool,
    /// Write the default config file if it doesn't exist, create orbit's folders, show where things live
    #[arg(long)]
    pub init_config: bool,
    /// List built-in themes (with color swatches on a terminal) and exit
    #[arg(long)]
    pub list_themes: bool,
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Control a running orbit, e.g. `orbit ctl next`, `orbit ctl vol +5`, `orbit ctl status "{artist} - {title}"`
    Ctl {
        /// A command and its arguments (see `orbit commands`)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Print a shell completion script, e.g. `orbit completions zsh > ~/.zfunc/_orbit`
    #[command(after_help = "\
Install:
  bash        orbit completions bash > ~/.local/share/bash-completion/completions/orbit
  zsh         orbit completions zsh > ~/.zfunc/_orbit   (with fpath+=~/.zfunc before compinit)
  fish        orbit completions fish > ~/.config/fish/completions/orbit.fish
  elvish      orbit completions elvish >> ~/.config/elvish/rc.elv
  powershell  orbit completions powershell >> $PROFILE")]
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
    /// List every action with its keys (defaults plus your [keys]) and the :command bindings
    Keys,
    /// List the commands understood by `orbit ctl`, the : prompt and ":command" key bindings
    Commands,
    /// Check the config file and report every problem (exit status 1 if there are any)
    Check,
}

/// "40" or "40%", 0..=150 (above 100 amplifies).
fn parse_volume(s: &str) -> Result<u8, String> {
    let digits = s.trim().trim_end_matches('%');
    match digits.parse::<u16>() {
        Ok(v) if v <= 150 => Ok(v as u8),
        Ok(v) => Err(format!("{v} is too loud (0-150)")),
        Err(_) => Err(format!("\"{s}\" is not a volume (a percentage, 0-150)")),
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut paths = Paths::new(cli.config.clone());

    if cli.print_config {
        return print_out(config::DEFAULT_CONFIG);
    }
    if cli.list_themes {
        return list_themes(&paths, cli.theme.as_deref());
    }
    if cli.init_config {
        return init_config(&paths);
    }
    match &cli.cmd {
        Some(Cmd::Completions { shell }) => return completions(*shell),
        Some(Cmd::Commands) => return list_commands(),
        Some(Cmd::Check) => return check(&paths),
        _ => {}
    }

    let loaded = Config::load_with_warnings(&paths.config_file);
    if let Some(socket) = loaded.as_ref().ok().and_then(|(cfg, _)| cfg.ipc.socket.as_deref()) {
        // App::new binds paths.socket: the running orbit and `orbit ctl` must use the same path
        paths.socket = config::expand_tilde(socket);
    }
    // `orbit ctl` only needs the socket: a config broken mid-edit must not cut off a running orbit
    if let Some(Cmd::Ctl { command }) = &cli.cmd {
        return ctl(&paths.socket, command);
    }
    let (cfg, config_warnings) = loaded?;
    if let Some(Cmd::Keys) = &cli.cmd {
        return list_keys(&cfg);
    }

    if let Some(v) = cli.volume.filter(|v| *v > cfg.playback.max_volume) {
        anyhow::bail!(
            "--volume {v} is above playback.max_volume ({}); raise max_volume in {} to allow it",
            cfg.playback.max_volume,
            paths.config_file.display()
        );
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!("orbit needs a terminal (stdin and stdout must be a tty); to control a running orbit from a script, use `orbit ctl`");
    }
    run_tui(&cli, cfg, config_warnings, paths)
}

fn run_tui(cli: &Cli, cfg: Config, config_warnings: Vec<String>, paths: Paths) -> anyhow::Result<()> {
    let _ = paths.ensure_dirs();
    // One orbit per data folder: a second one would play over the first, and whichever quit last
    // would overwrite the other's state.json (favorites, play counts). Held until we return.
    let lock = File::create(paths.data_dir.join("orbit.lock"));
    if let Ok(Err(fs::TryLockError::WouldBlock)) = lock.as_ref().map(File::try_lock) {
        anyhow::bail!("orbit is already running: quit it first, or control it with `orbit ctl` (see `orbit commands`)");
    }
    let crash_log = paths.cache_dir.join("crash.log");
    let signals = Signals::install()?;
    if cfg.ui.terminal_title {
        // before App::new, which may already set a title when it resumes playback
        notify::terminal_title_begin();
    }
    let mut app = App::new(cfg, paths, cli).inspect_err(|_| notify::terminal_title_end())?;
    app.add_config_warnings(config_warnings);
    let mut terminal = ratatui::try_init().inspect_err(|_| restore_terminal())?;
    // pasted text arrives as one Event::Paste (app.rs inserts it into the open input) instead of keys
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    install_panic_hook(crash_log);
    if app.cfg.ui.mouse {
        let _ = execute!(io::stdout(), EnableMouseCapture);
    }
    let result = panic::catch_unwind(AssertUnwindSafe(|| run(&mut terminal, &mut app, &signals)));
    // Save the session even after a crash (the hook has already restored the terminal and printed
    // the panic): losing the queue and play counts is worse than saving slightly odd state.
    app.shutdown();
    restore_terminal();
    // not drop(): Terminal's Drop shows the cursor again (restore_terminal just did) and panics
    // printing the error when the terminal has hung up
    std::mem::forget(terminal);
    result.unwrap_or_else(|panic| panic::resume_unwind(panic))
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App, signals: &Signals) -> anyhow::Result<()> {
    let mut size = terminal.size()?;
    let mut mouse = app.cfg.ui.mouse;
    loop {
        app.tick();
        if let Some(msg) = take_worker_panic() {
            app.flash(msg, MsgKind::Error);
        }
        if signals.quit.load(Ordering::Relaxed) {
            app.should_quit = true;
        }
        if app.should_quit {
            return Ok(());
        }
        if signals.resumed.swap(false, Ordering::Relaxed) {
            resume(terminal, app)?;
        }
        // A resize normally arrives as an event (SIGWINCH), which a process without a
        // controlling terminal never receives; noticing it here costs one ioctl.
        let now = terminal.size()?;
        if now != size {
            size = now;
            app.dirty = true;
        }
        // ui.mouse changed by a config reload
        if app.cfg.ui.mouse != mouse {
            mouse = app.cfg.ui.mouse;
            if mouse { execute!(io::stdout(), EnableMouseCapture)? } else { execute!(io::stdout(), DisableMouseCapture)? }
        }
        if app.force_clear {
            // ctrl+l: repaint every cell, not just what ratatui thinks changed (resize rather than
            // clear(), which asks the terminal for the cursor position and fails without an answer)
            terminal.resize(now.into())?;
            app.force_clear = false;
            app.dirty = true;
        }
        if app.needs_redraw() {
            terminal.draw(|f| ui::draw(f, app))?;
            app.dirty = false;
        }
        // Wait here, not in event::poll: once the terminal hangs up (its window was closed),
        // crossterm's reader spins forever on read() returning 0 and never gets back to us. A SIGHUP
        // that cut the wait short may mean the same, so the tty isn't handed to crossterm then either.
        let quit = || signals.quit.load(Ordering::Relaxed);
        let Some(stdin_ready) = wait_for_input(app.frame_timeout(), app.ipc.as_ref()).filter(|_| !quit()) else {
            app.should_quit = true;
            continue;
        };
        // crossterm reads 1 KiB per wake-up and wakes only for new input: if it sees nothing although
        // stdin is readable, it left bytes behind, and it waits for more itself (poll(2) would spin)
        if event::poll(Duration::ZERO)? || (stdin_ready && event::poll(app.frame_timeout())?) {
            // drain everything that's queued so key repeat never lags behind
            loop {
                match event::read()? {
                    // raw mode turns these into keys instead of SIGINT / SIGTSTP
                    Event::Key(key) if is_ctrl(&key, 'c') => app.should_quit = true,
                    Event::Key(key) if is_ctrl(&key, 'z') && app.keymap.get(&key).is_none() => suspend(terminal, app, signals)?,
                    ev => app.handle_event(ev),
                }
                if app.should_quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
}

/// Block until stdin or a remote-control socket has input, `timeout` passes or a signal arrives.
/// Some(stdin is readable), or None once the terminal has hung up.
fn wait_for_input(timeout: Duration, ipc: Option<&IpcServer>) -> Option<bool> {
    let watch = |fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let mut fds: Vec<_> = std::iter::once(libc::STDIN_FILENO).chain(ipc.into_iter().flat_map(IpcServer::fds)).map(watch).collect();
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `fds` is a valid array of `fds.len()` pollfds
    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
    let stdin = fds[0].revents;
    (stdin & (libc::POLLHUP | libc::POLLNVAL) == 0).then_some(stdin & libc::POLLIN != 0)
}

fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.kind != KeyEventKind::Release && key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char(c)
}

/// Undo everything `ratatui::init` and orbit turned on. Safe to call more than once.
fn restore_terminal() {
    let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste, cursor::Show);
    notify::terminal_title_end();
    // not ratatui::restore(), which panics printing the error when the terminal has hung up
    let _ = ratatui::try_restore();
}

/// Ctrl+Z: give the terminal back to the shell and stop; carry on where we were after `fg`.
fn suspend(terminal: &mut ratatui::DefaultTerminal, app: &mut App, signals: &Signals) -> io::Result<()> {
    restore_terminal();
    // SIGTSTP rather than SIGSTOP: the kernel ignores it when no job-control shell could resume
    // us (orbit launched directly by a terminal emulator), so it can never freeze the terminal.
    let _ = signal_hook::low_level::raise(signal_hook::consts::SIGTSTP);
    signals.resumed.store(false, Ordering::Relaxed);
    resume(terminal, app)
}

/// Back to TUI mode after a suspend (or a SIGCONT after being stopped from outside, when the shell
/// may have reset the terminal and drawn over the screen), with a full repaint.
fn resume(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> io::Result<()> {
    // after an outside stop the shell has reset the tty, but crossterm still thinks raw mode is
    // on and enable_raw_mode would do nothing
    let _ = terminal::disable_raw_mode();
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen, EnableBracketedPaste)?;
    if app.cfg.ui.mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    if app.cfg.ui.terminal_title {
        notify::terminal_title_begin();
    }
    // resize rather than clear(), which asks the terminal for the cursor position: this also
    // adopts a size change made while we were stopped, and the next draw repaints everything
    let size = terminal.size()?;
    terminal.resize(size.into())?;
    app.dirty = true;
    Ok(())
}

/// Flags set by signal handlers, polled by the main loop.
struct Signals {
    /// SIGTERM / SIGINT / SIGHUP: save and quit. If that gets stuck, another SIGTERM or SIGINT a
    /// second later exits at once (status 128 + signal), the terminal restored. Not a second
    /// SIGHUP: a closing terminal often sends two (kernel and shell), and the session must still be saved.
    quit: Arc<AtomicBool>,
    /// SIGCONT: we were stopped and continued, the terminal needs setting up again.
    resumed: Arc<AtomicBool>,
}

impl Signals {
    fn install() -> io::Result<Signals> {
        use signal_hook::consts::{SIGCONT, SIGHUP, SIGINT, SIGTERM};
        let quit = Arc::new(AtomicBool::new(false));
        for sig in [SIGTERM, SIGINT, SIGHUP] {
            signal_hook::flag::register(sig, Arc::clone(&quit))?;
        }
        let mut forced = signal_hook::iterator::Signals::new([SIGTERM, SIGINT])?;
        std::thread::Builder::new().name("signals".into()).spawn(move || {
            let mut signals = forced.forever();
            signals.next(); // the main loop saves and quits
            let first = Instant::now();
            for sig in signals {
                // a quick repeat (kill twice) just lets that finish
                if first.elapsed() >= Duration::from_secs(1) {
                    // on the side: a stuck terminal write (holding the stdout lock) mustn't keep us
                    std::thread::spawn(restore_terminal);
                    std::thread::sleep(Duration::from_millis(100));
                    std::process::exit(128 + sig);
                }
            }
        })?;
        let resumed = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(SIGCONT, Arc::clone(&resumed))?;
        // REVIEW FIX: when the terminal goes away, crossterm's event poll spins on the tty's EOF and
        // the main loop never sees the flag; don't outlive the terminal (autosave keeps the session).
        let flag = Arc::clone(&quit);
        std::thread::Builder::new().name("orbit-quit-watchdog".into()).spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            std::thread::sleep(Duration::from_secs(3));
            std::process::exit(128 + SIGHUP);
        })?;
        Ok(Signals { quit, resumed })
    }
}

/// The latest panic of a background thread, waiting to be shown in the status line.
static WORKER_PANIC: Mutex<Option<String>> = Mutex::new(None);

fn take_worker_panic() -> Option<String> {
    WORKER_PANIC.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// Wrap ratatui's panic hook (restore the terminal, then print the panic) so it also turns off
/// mouse capture, shows the cursor and restores the window title, and log every panic with a
/// backtrace to `crash_log`. A panic on a background thread (scanner, decoder, ...) must not tear
/// the screen down under the still-running UI: it is logged and flashed in the status line.
fn install_panic_hook(crash_log: PathBuf) {
    let restore_and_print = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let logged = log_panic(&crash_log, name, info).is_ok();
        if name == "main" {
            let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste, cursor::Show);
            notify::terminal_title_end();
            restore_and_print(info);
            if logged {
                eprintln!("orbit crashed; details saved to {}", crash_log.display());
            }
        } else {
            let what = info.payload_as_str().unwrap_or("panic");
            let details = if logged { format!(" (details in {})", crash_log.display()) } else { String::new() };
            *WORKER_PANIC.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!("internal error in {name}: {what}{details}"));
        }
    }));
}

fn log_panic(path: &Path, thread: &str, info: &PanicHookInfo) -> io::Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let version = env!("CARGO_PKG_VERSION");
    writeln!(file, "--- orbit {version}, unix time {now}, thread '{thread}' {info}\n{}\n", Backtrace::force_capture())
}

/// `orbit ctl ...`: print the reply. Exit status 1 for an "error: " reply or when orbit isn't running.
fn ctl(socket: &Path, words: &[String]) -> anyhow::Result<()> {
    match ipc::send(socket, &ctl_line(words)) {
        Ok(reply) if reply.starts_with("error: ") => {
            eprintln!("{reply}");
            std::process::exit(1)
        }
        Ok(reply) if reply.is_empty() => Ok(()),
        Ok(reply) => print_out(reply + "\n"),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1)
        }
    }
}

/// Join `orbit ctl` arguments into one command line. File and folder arguments of `play` / `add`
/// are made absolute (the running orbit has its own working directory, and doesn't expand `~`),
/// `add` arguments containing spaces are quoted so each stays one path, and a habitual `--`
/// (`orbit ctl seek -- -10`) is dropped.
fn ctl_line(words: &[String]) -> String {
    let words: Vec<String> = words.iter().filter(|w| *w != "--").cloned().collect();
    let Some((verb, args)) = words.split_first() else { return String::new() };
    match verb.trim_start_matches(':') {
        "play" if !args.is_empty() => format!("{verb} {}", absolute(&args.join(" "))),
        "add" => std::iter::once(verb.clone()).chain(args.iter().map(|a| quote(&absolute(a)))).collect::<Vec<_>>().join(" "),
        // two names, each one argument: `orbit ctl playlist rename "Road Trip" Drive`
        "playlist" | "pl" if args.first().is_some_and(|a| matches!(a.to_lowercase().as_str(), "rename" | "mv")) => {
            std::iter::once(verb.clone()).chain(args.iter().map(|a| quote(a))).collect::<Vec<_>>().join(" ")
        }
        _ => words.join(" "),
    }
}

/// `arg` as an absolute path when it names an existing file or folder, else unchanged. Relative
/// paths are resolved against our working directory and `~` is expanded; absolute paths are kept
/// as given (resolving symlinks could stop them matching the library's paths).
fn absolute(arg: &str) -> String {
    let path = config::expand_tilde(arg);
    let resolved = if path.is_absolute() { Some(path).filter(|p| p.exists()) } else { fs::canonicalize(&path).ok() };
    resolved.map_or_else(|| arg.to_string(), |p| p.to_string_lossy().into_owned())
}

fn quote(arg: &str) -> String {
    if arg.contains(char::is_whitespace) { format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\"")) } else { arg.to_string() }
}

/// Write to stdout; a closed pipe (`orbit keys | head`) is not an error.
fn print_out(text: impl AsRef<[u8]>) -> anyhow::Result<()> {
    let mut out = io::stdout().lock();
    match out.write_all(text.as_ref()).and_then(|()| out.flush()) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(()),
    }
}

fn completions(shell: Shell) -> anyhow::Result<()> {
    // generated into memory first: clap_complete panics on write errors such as a closed pipe
    let mut script = Vec::new();
    clap_complete::aot::generate(shell, &mut Cli::command(), "orbit", &mut script);
    print_out(script)
}

/// Names only, one per line, when piped; on a terminal (unless NO_COLOR) each theme also gets a
/// swatch of its visualizer gradient and main accents, on its own background. `*` marks the
/// theme in use.
fn list_themes(paths: &Paths, session_theme: Option<&str>) -> anyhow::Result<()> {
    const SWATCH: usize = 24;
    let names = theme::names();
    let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    if !color {
        return print_out(names.iter().map(|n| format!("{n}\n")).collect::<String>());
    }
    let current = match session_theme {
        Some(name) => name.to_string(),
        None => Config::load(&paths.config_file).map(|c| c.ui.theme).unwrap_or_default(),
    };
    let width = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
    let mut out = Vec::new();
    for name in names {
        let Some(theme) = theme::builtin(name) else { continue };
        let mark = if name == current { '*' } else { ' ' };
        queue!(out, Print(format!("{mark} {name:<width$}  ")), SetBackgroundColor(theme.bg.into_crossterm()), Print(' '))?;
        for i in 0..SWATCH {
            let c = theme.gradient_at(i as f32 / (SWATCH - 1) as f32);
            queue!(out, SetForegroundColor(c.into_crossterm()), Print('█'))?;
        }
        for c in [theme.accent, theme.accent2, theme.playing, theme.sel_bg] {
            queue!(out, SetForegroundColor(c.into_crossterm()), Print(" ●"))?;
        }
        queue!(out, Print(' '), ResetColor, Print('\n'))?;
    }
    print_out(out)
}

/// `--init-config`: write the default config unless one exists, create orbit's folders, and show
/// where everything lives.
fn init_config(paths: &Paths) -> anyhow::Result<()> {
    let file = &paths.config_file;
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    // create_new: never clobbers a config that appeared in the meantime
    let status = match OpenOptions::new().write(true).create_new(true).open(file) {
        Ok(mut f) => {
            f.write_all(config::DEFAULT_CONFIG.as_bytes())?;
            "written"
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => "already exists, left unchanged",
        Err(e) => anyhow::bail!("{}: {e}", file.display()),
    };
    paths.ensure_dirs()?;
    let cfg = Config::load(file).unwrap_or_default();
    let socket = cfg.ipc.socket.as_deref().map(config::expand_tilde).unwrap_or_else(|| paths.socket.clone());
    let rows = [
        ("config", file.display().to_string(), status),
        ("music", cfg.library.dirs.join(", "), "folders scanned, see [library] dirs"),
        ("state", paths.state_file.display().to_string(), "session, play counts, favorites"),
        ("playlists", format!("{}/", paths.playlists_dir.display()), ".m3u8 files, yours to edit"),
        ("cache", paths.library_cache.display().to_string(), "library metadata, safe to delete"),
        ("socket", ipc::socket_path(&socket).display().to_string(), "for `orbit ctl`, while orbit runs"),
    ];
    let mut out: String = rows.iter().map(|(what, path, note)| format!("{what:<10} {path}  ({note})\n")).collect();
    out.push_str(
        "\nEdit the config, then press ctrl+r in orbit (or run `orbit ctl reload`) to apply it.\n\
         `orbit check` finds mistakes; `orbit --print-config` shows every option with its default.\n",
    );
    print_out(out)
}

fn list_commands() -> anyhow::Result<()> {
    const W: usize = 34;
    let mut out = String::from("Commands for `orbit ctl`, the : prompt, and \":command\" key bindings:\n\n");
    for (usage, description) in command::COMMANDS {
        if usage.chars().count() <= W {
            out.push_str(&format!("  {usage:<W$} {description}\n"));
        } else {
            out.push_str(&format!("  {usage}\n  {:<W$} {description}\n", ""));
        }
    }
    print_out(out)
}

fn list_keys(cfg: &Config) -> anyhow::Result<()> {
    let (keymap, warnings) = Keymap::new(&cfg.keys);
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    let mut out = String::new();
    let mut category = "";
    for &action in Action::ALL {
        if action.category() != category {
            category = action.category();
            out.push_str(&format!("{}{category}\n", if out.is_empty() { "" } else { "\n" }));
        }
        let keys = keymap.keys_for(action);
        // not "-": that is a key (volume-down)
        let keys = if keys.is_empty() { "(unbound)".to_string() } else { keys.join(", ") };
        out.push_str(&format!("  {keys:<20} {:<24} {}\n", action.name(), action.description()));
    }
    let commands = keymap.command_bindings();
    if !commands.is_empty() {
        out.push_str("\nCommands\n");
        for (key, command) in commands {
            out.push_str(&format!("  {key:<20} :{command}\n"));
        }
    }
    print_out(out)
}

/// `orbit check`: every problem in the config at once, instead of only the first one flashed at
/// startup. Exit status 1 when there are any.
fn check(paths: &Paths) -> anyhow::Result<()> {
    let file = &paths.config_file;
    // the same lenient load as startup: unknown options, bad values and `validate` all reported
    let (cfg, mut problems) = match Config::load_with_warnings(file) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1)
        }
    };
    for (dir, path) in cfg.library.dirs.iter().zip(cfg.music_dirs()) {
        if !path.is_dir() {
            problems.push(format!("[library] dirs: \"{dir}\" is not a folder"));
        }
    }
    for dir in &cfg.lyrics.dirs {
        if !config::expand_tilde(dir).is_dir() {
            problems.push(format!("[lyrics] dirs: \"{dir}\" is not a folder"));
        }
    }

    let mut out = if file.exists() {
        format!("{}: parsed\n", file.display())
    } else {
        format!("{}: not found, using defaults (`orbit --init-config` writes one)\n", file.display())
    };
    for p in &problems {
        out.push_str(&format!("  warning: {p}\n"));
    }
    out.push_str(&match problems.len() {
        0 => "no problems found\n".to_string(),
        1 => "1 problem\n".to_string(),
        n => format!("{n} problems\n"),
    });
    print_out(out)?;
    if !problems.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cli_parses() {
        let cli = Cli::try_parse_from(["orbit", "--volume", "40%", "--theme", "nord", "--no-resume", "a.mp3", "Jpop"]).unwrap();
        assert_eq!((cli.volume, cli.theme.as_deref(), cli.no_resume), (Some(40), Some("nord"), true));
        assert_eq!(cli.paths, [PathBuf::from("a.mp3"), PathBuf::from("Jpop")]);
        assert!(cli.cmd.is_none());

        let ctl = |args: &[&str]| match Cli::try_parse_from(args).unwrap().cmd {
            Some(Cmd::Ctl { command }) => command,
            other => panic!("{other:?}"),
        };
        assert_eq!(ctl(&["orbit", "ctl", "vol", "-5"]), ["vol", "-5"]);
        assert_eq!(ctl(&["orbit", "ctl", "seek", "--", "-10"]), ["seek", "--", "-10"], "ctl_line drops the --");
        assert_eq!(ctl(&["orbit", "ctl", "status", "{artist} - {title}"]), ["status", "{artist} - {title}"]);
        assert!(Cli::try_parse_from(["orbit", "ctl"]).is_err());

        assert!(matches!(Cli::try_parse_from(["orbit", "completions", "fish"]).unwrap().cmd, Some(Cmd::Completions { shell: Shell::Fish })));
        assert!(Cli::try_parse_from(["orbit", "completions", "tcsh"]).is_err());
        assert!(matches!(Cli::try_parse_from(["orbit", "keys"]).unwrap().cmd, Some(Cmd::Keys)));
        assert!(matches!(Cli::try_parse_from(["orbit", "--config", "x.toml", "check"]).unwrap().cmd, Some(Cmd::Check)));
    }

    #[test]
    fn volume_is_validated() {
        assert_eq!(parse_volume("0"), Ok(0));
        assert_eq!(parse_volume("70%"), Ok(70));
        assert_eq!(parse_volume(" 150 "), Ok(150));
        assert!(parse_volume("151").unwrap_err().contains("0-150"));
        assert!(parse_volume("999").is_err());
        assert!(parse_volume("-1").is_err());
        assert!(parse_volume("loud").is_err());
        assert!(parse_volume("").is_err());
        assert!(Cli::try_parse_from(["orbit", "--volume", "200"]).is_err());
    }

    #[test]
    fn completion_scripts_for_every_shell() {
        for shell in Shell::value_variants() {
            let mut script = Vec::new();
            clap_complete::aot::generate(*shell, &mut Cli::command(), "orbit", &mut script);
            let script = String::from_utf8(script).unwrap();
            assert!(script.contains("completions") && script.contains("list-themes"), "{shell}");
        }
    }

    #[test]
    fn ctl_lines() {
        assert_eq!(ctl_line(&words(&["vol", "+5"])), "vol +5");
        assert_eq!(ctl_line(&words(&["status", "{artist} - {title}"])), "status {artist} - {title}");
        assert_eq!(ctl_line(&words(&["play"])), "play");
        assert_eq!(ctl_line(&words(&["seek", "--", "-10"])), "seek -10");
        assert_eq!(ctl_line(&words(&["status", "{title} -- {artist}"])), "status {title} -- {artist}");
        assert_eq!(ctl_line(&words(&["play", "some", "search", "text"])), "play some search text");
        assert_eq!(ctl_line(&words(&["playlist", "rename", "Road Trip", "Long Drive"])), "playlist rename \"Road Trip\" \"Long Drive\"");
        // cargo runs tests from the crate root
        let manifest = fs::canonicalize("Cargo.toml").unwrap();
        let src = fs::canonicalize("src").unwrap();
        assert_eq!(ctl_line(&words(&["play", "src"])), format!("play {}", src.display()));
        assert_eq!(
            ctl_line(&words(&[":add", "Cargo.toml", "/no such/x.mp3", "a\"b"])),
            format!(":add {} \"/no such/x.mp3\" a\"b", manifest.display())
        );
        // absolute paths are passed as given, not canonicalized (/tmp is a symlink on macOS)
        assert_eq!(ctl_line(&words(&["add", "/tmp"])), "add /tmp");
        let home = std::env::var("HOME").unwrap();
        assert_eq!(ctl_line(&words(&["add", "~"])), format!("add {home}"));
    }
}

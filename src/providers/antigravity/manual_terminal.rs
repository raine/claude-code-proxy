//! One owned nonblocking terminal, no stdin worker or general escape parser.
use super::super::{
    auth::{Auth, Store},
    config::Config,
    replay::Result,
};
use super::{INPUT_LIMIT, INVALID};
use std::{
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    time::Duration,
};
use tokio::{
    signal::unix::{SignalKind, signal},
    time::Instant,
};

const CANCELLED: &str = "Manual OAuth login cancelled";
const TIMED_OUT: &str = "Manual OAuth input timed out";
const CLOSED: &str = "Manual OAuth terminal closed or unavailable";
const RESTORE_FAILED: &str = "Manual OAuth terminal cleanup failed; no further exchange attempted";
const WIDTH_REQUIRED: &str = "Manual OAuth terminal requires at least 10 columns";
const RESIZED: &str = "Manual OAuth terminal width changed; login cancelled";
const OUTPUT_TRANSLATES: &str =
    "Manual OAuth terminal output must preserve carriage returns and letter case";
const PROMPT: &[u8] =
    b"\nPaste the displayed authorization code (visible), then Enter; Ctrl-C cancels.\r\n";
const NOTICE: &[u8] = b"Experimental manual Antigravity login. Open this URL in your browser and choose the intended account.\nThe configured OAuth client must permit https://antigravity.google/oauth-callback.\nCopy the CODE DISPLAYED by the page, not the address bar (the page removes the code from its URL).\nCancel if the browser reports an error (including redirect_uri_mismatch) or shows no code.\nNo browser is opened automatically; no callback listener, credential import, or fallback is used.\nWarning: the code is sensitive and VISIBLE; keep your screen, scrollback and recordings private.\nThe input row shows [character count]; < means only the end fits. Backspace edits; changing terminal width cancels.\n";

pub(crate) enum CliOutcome {
    Success,
    HandledFailure,
}

/// Early refusal remains an ordinary CLI error. After terminal acquisition,
/// every failure is handled here, never by Rust main's blocking stderr report.
/// All auth/runtime/terminal guards are gone before returning the typed outcome.
pub(crate) fn run_cli() -> Result<CliOutcome> {
    let mut terminal = Terminal::open()?;
    let result = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => {
            let result = runtime.block_on(async {
                let auth = Auth::new(Config::load()?, Store::configured())?;
                login_on_terminal(&auth, &mut terminal, Duration::from_secs(180)).await
            });
            // No reactor-dependent reporting or async RAII survives this point.
            drop(runtime);
            result
        }
        Err(_) => Err("Manual OAuth runtime initialization failed"),
    };
    let outcome = match result {
        Ok(()) => CliOutcome::Success,
        Err(error) => {
            // Cleanup has already been attempted by the input/login core. A
            // retry is best effort; a failed report must not fall back to stderr.
            let error = terminal.restore().err().unwrap_or(error);
            terminal.report_error(error);
            CliOutcome::HandledFailure
        }
    };
    drop(terminal);
    Ok(outcome)
}

#[cfg(test)]
pub(crate) async fn login(auth: &Auth, timeout: Duration) -> Result<()> {
    let mut terminal = Terminal::open()?;
    login_on_terminal(auth, &mut terminal, timeout).await
}

async fn login_on_terminal(auth: &Auth, terminal: &mut Terminal, timeout: Duration) -> Result<()> {
    // Tokio registrations persist process-wide. Keep both receivers in the
    // select through exchange and confirmation, not just terminal input. The
    // post-runtime diagnostic uses its own synchronous deadline, not receivers.
    // This path is never installed by serve or automatic callback login.
    let mut interrupt =
        signal(SignalKind::interrupt()).map_err(|_| "Manual OAuth signal setup failed")?;
    let mut terminate =
        signal(SignalKind::terminate()).map_err(|_| "Manual OAuth signal setup failed")?;
    let result = tokio::select! {
        biased;
        _ = interrupt.recv() => Err(CANCELLED),
        _ = terminate.recv() => Err(CANCELLED),
        result = auth.login_manual_with_completion(|authorization, _| {
            terminal.complete(authorization, timeout)
        }) => result,
    };
    // Also covers cancellation while complete() was suspended. Drop is backup,
    // not the success path: a cleanup error must never authorize an exchange.
    terminal.restore()?;
    result?;

    // The credential is durably committed now. Confirmation is bounded, best
    // effort only: a signal, closed tty, or backpressure must not relabel a
    // committed profile as a cancelled/failed login and invite a second login.
    let deadline = Instant::now() + Duration::from_secs(2);
    tokio::select! {
        biased;
        _ = interrupt.recv() => (),
        _ = terminate.recv() => (),
        _ = async {
            terminal.output(super::super::auth::LOGIN_SAVED.as_bytes(), deadline).await?;
            terminal.output(b"\n", deadline).await
        } => (),
    }
    Ok(())
}

struct Terminal {
    file: File,
    saved: libc::termios,
    active: bool,
}
impl Terminal {
    fn open() -> Result<Self> {
        if !io::stdin().is_terminal() {
            return Err("Manual OAuth login requires terminal stdin; pipes are unsupported");
        }
        // A fresh open file description, NOT dup(stdin): flags cannot affect
        // the shell's shared stdin descriptor. /dev/tty is the input authority.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
            .open("/dev/tty")
            .map_err(|_| CLOSED)?;
        foreground(&file)?;
        let mut saved = std::mem::MaybeUninit::uninit();
        // SAFETY: owned live tty fd and writable termios storage.
        if unsafe { libc::tcgetattr(file.as_raw_fd(), saved.as_mut_ptr()) } != 0 {
            return Err(CLOSED);
        }
        Ok(Self {
            file,
            saved: unsafe { saved.assume_init() },
            active: false,
        })
    }
    fn report_error(&mut self, message: &'static str) {
        // The runtime has shut down, so neither Tokio time nor its persistent
        // signal registrations may be used to drive this best-effort report.
        // The fd is our original independent O_NONBLOCK /dev/tty description.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let diagnostic = format!("\r\nError: {message}\r\n");
        let mut bytes = diagnostic.as_bytes();
        let Ok(_mask) = TtouMask::block() else {
            return;
        };
        while !bytes.is_empty() {
            let now = std::time::Instant::now();
            if now >= deadline || foreground(&self.file).is_err() {
                return;
            }
            let mut descriptor = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // One absolute budget across partial writes, EAGAIN and EINTR.
            let remaining = deadline.saturating_duration_since(now);
            let wait_ms = remaining.as_millis().clamp(1, 20) as libc::c_int;
            let ready = unsafe { libc::poll(&mut descriptor, 1, wait_ms) };
            if ready < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            if descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return;
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            if descriptor.revents & libc::POLLOUT == 0 {
                continue;
            }
            match self.file.write(bytes) {
                Ok(0) => return,
                Ok(n) => bytes = &bytes[n..],
                Err(error) if retryable(&error) => (),
                Err(_) => return,
            }
        }
    }
    fn manage_input(&mut self) -> Result<()> {
        foreground(&self.file)?;
        let mut raw = self.saved;
        raw.c_iflag &= !(libc::IGNBRK
            | libc::BRKINT
            | libc::PARMRK
            | libc::ISTRIP
            | libc::INLCR
            | libc::IGNCR
            | libc::ICRNL
            | libc::IXON
            | libc::IXOFF
            | libc::IXANY
            | libc::INPCK);
        raw.c_lflag &= !(libc::ECHO
            | libc::ECHOE
            | libc::ECHOK
            | libc::ECHONL
            | libc::ECHOCTL
            | libc::ECHOPRT
            | libc::ECHOKE
            | libc::ICANON
            | libc::IEXTEN
            | libc::ISIG);
        raw.c_cflag = (raw.c_cflag & !(libc::CSIZE | libc::PARENB)) | libc::CS8;
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // In particular preserve c_oflag/OPOST/ONLCR: authorization URL output
        // must remain readable rather than stair-stepping across the terminal.
        let _mask = TtouMask::block()?;
        self.active = true;
        if unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &raw) } != 0 {
            return Err("Manual OAuth terminal setup failed");
        }
        Ok(())
    }
    fn restore(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let _mask = TtouMask::block().map_err(|_| RESTORE_FAILED)?;
        // Discard queued input BEFORE restoring echo. Never drain a producer;
        // this cannot promise disposal of future bytes arriving after exit.
        let flushed = unsafe { libc::tcflush(self.file.as_raw_fd(), libc::TCIFLUSH) } == 0;
        // TCSADRAIN/TCSAFLUSH can wait indefinitely for output: use TCSANOW.
        let restored =
            unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &self.saved) } == 0;
        if restored {
            self.active = false;
        }
        if flushed && restored {
            Ok(())
        } else {
            Err(RESTORE_FAILED)
        }
    }
    async fn complete(&mut self, authorization: url::Url, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let result = async {
            self.validate_output()?;
            if self.columns()? < 10 {
                return Err(WIDTH_REQUIRED);
            }
            self.manage_input()?;
            self.output(NOTICE, deadline).await?;
            self.output(authorization.as_str().as_bytes(), deadline)
                .await?;
            self.output(b"\n", deadline).await?;
            self.output(PROMPT, deadline).await?;
            self.input(deadline).await
        }
        .await;
        self.restore()?;
        result
    }
    async fn output(&mut self, mut bytes: &[u8], deadline: Instant) -> Result<()> {
        while !bytes.is_empty() {
            self.ready(libc::POLLOUT, deadline, None).await?;
            match self.file.write(bytes) {
                Ok(0) => return Err(CLOSED),
                Ok(n) => bytes = &bytes[n..],
                Err(error) if retryable(&error) => (),
                Err(_) => return Err(CLOSED),
            }
        }
        Ok(())
    }
    fn validate_output(&self) -> Result<()> {
        let flags = self.saved.c_oflag;
        if flags & libc::OPOST == 0 {
            return Ok(()); // Translation flags are inert without postprocessing.
        }
        if flags & libc::OCRNL != 0 {
            return Err(OUTPUT_TRANSLATES);
        }
        // Apple and BSDs other than OpenBSD do not expose OLCUC. Do not reject
        // harmless output flags or silently normalize the caller's saved mode.
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "openbsd",
            target_os = "solaris",
            target_os = "illumos",
            target_os = "aix",
            target_os = "haiku",
            target_os = "hurd",
            target_os = "nto",
            target_os = "redox",
            target_os = "emscripten",
            target_os = "cygwin",
            target_os = "fuchsia",
            target_os = "l4re"
        ))]
        if flags & libc::OLCUC != 0 {
            return Err(OUTPUT_TRANSLATES);
        }
        Ok(())
    }
    fn columns(&self) -> Result<u16> {
        let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
        // SAFETY: owned live tty descriptor and writable winsize storage.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCGWINSZ, size.as_mut_ptr()) } != 0 {
            return Err(CLOSED);
        }
        Ok(unsafe { size.assume_init() }.ws_col)
    }
    async fn input(&mut self, deadline: Instant) -> Result<String> {
        let columns = self.columns()?;
        if columns < 10 {
            return Err(WIDTH_REQUIRED);
        }
        let mut line = Line::new();
        let mut frame = Frame::new();
        let mut dirty = true;
        let mut completion = None;
        let mut buffer = [0u8; 256];
        loop {
            if frame.done() && dirty {
                frame.render(&line, columns, completion.is_some());
                dirty = false;
            }
            // Always service input, even when a frame cannot drain. In particular
            // keyboard cancellation must not depend on output becoming writable.
            let events = libc::POLLIN | if frame.done() { 0 } else { libc::POLLOUT };
            let ready = self.ready(events, deadline, Some(columns)).await?;
            if ready & libc::POLLIN != 0 {
                match self.file.read(&mut buffer) {
                    Ok(0) => return Err(CLOSED),
                    Ok(n) => {
                        for byte in &buffer[..n] {
                            if Instant::now() >= deadline {
                                return Err(TIMED_OUT);
                            }
                            if completion.is_some() {
                                // Enter freezes the completion. Discard trailing
                                // data; still honor cancellation during final output.
                                if cancels(*byte) {
                                    return Err(CANCELLED);
                                }
                                continue;
                            }
                            match line.feed(*byte)? {
                                Input::Pending | Input::Empty => dirty = true,
                                Input::Complete(code) => {
                                    completion = Some(code);
                                    dirty = true;
                                    // Scan the rest only for cancellation, as above.
                                }
                            }
                        }
                    }
                    Err(error) if retryable(&error) => (),
                    Err(_) => return Err(CLOSED),
                }
            }
            // One immutable in-flight frame, not an output queue. Edits arriving
            // during a partial write are coalesced into the next latest snapshot.
            if frame.done() && dirty {
                frame.render(&line, columns, completion.is_some());
                dirty = false;
            }
            if ready & libc::POLLOUT != 0 && !frame.done() {
                if self.columns()? != columns {
                    return Err(RESIZED);
                }
                if Instant::now() >= deadline {
                    return Err(TIMED_OUT);
                }
                match self.file.write(&frame.bytes[frame.written..frame.len]) {
                    Ok(0) => return Err(CLOSED),
                    Ok(n) => frame.written += n,
                    Err(error) if retryable(&error) => (),
                    Err(_) => return Err(CLOSED),
                }
                if frame.done() && frame.final_frame {
                    // Kernel acceptance of the final display/newline is required;
                    // physical rendering by an external terminal is not observable.
                    return completion.ok_or(INVALID);
                }
            }
        }
    }
    async fn ready(
        &self,
        events: libc::c_short,
        deadline: Instant,
        columns: Option<u16>,
    ) -> Result<libc::c_short> {
        loop {
            // A suspension on EVERY iteration gives signal delivery priority
            // even under a continuously readable/writable tty or EINTR storm.
            tokio::time::sleep_until((Instant::now() + Duration::from_millis(2)).min(deadline))
                .await;
            if Instant::now() >= deadline {
                return Err(TIMED_OUT);
            }
            foreground(&self.file)?;
            if let Some(expected) = columns
                && self.columns()? != expected
            {
                return Err(RESIZED);
            }
            let mut descriptor = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events,
                revents: 0,
            };
            // SAFETY: one live descriptor; timeout zero never blocks a runtime thread.
            let polled = unsafe { libc::poll(&mut descriptor, 1, 0) };
            if polled < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(CLOSED);
            }
            if descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(CLOSED);
            }
            if descriptor.revents & events != 0 {
                return Ok(descriptor.revents);
            }
        }
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
fn foreground(file: &File) -> Result<()> {
    // SAFETY: live owned descriptor; neither call mutates process/terminal state.
    let group = unsafe { libc::tcgetpgrp(file.as_raw_fd()) };
    if group < 0 || group != unsafe { libc::getpgrp() } {
        Err("Manual OAuth login requires a foreground controlling terminal")
    } else {
        Ok(())
    }
}
fn retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

// The foreground group can change after validation. Block SIGTTOU only on the
// current thread around synchronous termios/flush calls, restoring its exact
// prior mask before any await. Cleanup must not stop the process in the kernel.
struct TtouMask(libc::sigset_t);
impl TtouMask {
    fn block() -> Result<Self> {
        let mut set = std::mem::MaybeUninit::uninit();
        let mut old = std::mem::MaybeUninit::uninit();
        unsafe {
            if libc::sigemptyset(set.as_mut_ptr()) != 0 {
                return Err(RESTORE_FAILED);
            }
            let mut set = set.assume_init();
            if libc::sigaddset(&mut set, libc::SIGTTOU) != 0
                || libc::pthread_sigmask(libc::SIG_BLOCK, &set, old.as_mut_ptr()) != 0
            {
                return Err(RESTORE_FAILED);
            }
            Ok(Self(old.assume_init()))
        }
    }
}
impl Drop for TtouMask {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut());
        }
    }
}

// A row never reaches the last terminal column, so erasure needs neither
// cursor-up sequences nor assumptions about autowrap. Only accepted Line bytes
// reach the display. Width changes cancel instead of trying to repair reflow.
const MAX_VIEWPORT: usize = INPUT_LIMIT + 8;
struct Frame {
    bytes: [u8; 2 * MAX_VIEWPORT + 4],
    len: usize,
    written: usize,
    final_frame: bool,
}
impl Frame {
    fn new() -> Self {
        Self {
            bytes: [0; 2 * MAX_VIEWPORT + 4],
            len: 0,
            written: 0,
            final_frame: false,
        }
    }
    fn done(&self) -> bool {
        self.written == self.len
    }
    fn render(&mut self, line: &Line, columns: u16, complete: bool) {
        let width = usize::from(columns - 1).min(MAX_VIEWPORT);
        self.bytes[..width + 2].fill(b' ');
        self.bytes[0] = b'\r';
        self.bytes[width + 1] = b'\r';
        self.len = width + 2;
        self.written = 0;
        self.final_frame = complete;
        let count = format!("[{}] ", line.len); // At most seven ASCII bytes.
        self.append(count.as_bytes());
        let mut available = width - count.len();
        if line.len > available {
            self.append(b"<");
            available -= 1;
        }
        self.append(&line.bytes[line.len.saturating_sub(available)..line.len]);
        if complete {
            self.append(b"\r\n");
        }
    }
    fn append(&mut self, bytes: &[u8]) {
        self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }
}

fn cancels(byte: u8) -> bool {
    matches!(byte, 3 | 4 | 26 | 28)
}

enum Input {
    Pending,
    Empty,
    Complete(String),
}
struct Line {
    bytes: [u8; INPUT_LIMIT],
    len: usize,
    escape: usize,
    paste: bool,
    empty: u8,
}
impl Line {
    fn new() -> Self {
        Self {
            bytes: [0; INPUT_LIMIT],
            len: 0,
            escape: 0,
            paste: false,
            empty: 0,
        }
    }
    fn feed(&mut self, byte: u8) -> Result<Input> {
        if cancels(byte) {
            return Err(CANCELLED);
        }
        if self.escape != 0 {
            let expected = if self.paste {
                b"\x1b[201~"
            } else {
                b"\x1b[200~"
            };
            if byte != expected[self.escape] {
                return Err(INVALID);
            }
            self.escape += 1;
            if self.escape == expected.len() {
                self.escape = 0;
                self.paste = !self.paste;
            }
            return Ok(Input::Pending);
        }
        match byte {
            0x1b => self.escape = 1,
            b'\r' | b'\n' if !self.paste => {
                if self.len != 0 {
                    // Only printable ASCII entered the fixed buffer.
                    return Ok(Input::Complete(
                        String::from_utf8(self.bytes[..self.len].to_vec()).map_err(|_| INVALID)?,
                    ));
                }
                self.empty += 1;
                if self.empty > 3 {
                    return Err("Manual OAuth empty input limit exceeded");
                }
                return Ok(Input::Empty);
            }
            8 | 127 => self.len = self.len.saturating_sub(1),
            0x21..=0x7e if self.len < INPUT_LIMIT => {
                self.bytes[self.len] = byte;
                self.len += 1;
            }
            _ => return Err(INVALID),
        }
        Ok(Input::Pending)
    }
}

#[cfg(test)]
#[path = "manual_terminal_tests.rs"]
mod tests;

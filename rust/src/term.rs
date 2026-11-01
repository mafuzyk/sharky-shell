use std::io::{self, Read};

use nix::libc;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Up,
    Down,
    Tab,
    ShiftTab,
    Ctrl(char),
    Alt(char),
    Escape,
    PasteStart,
    PasteEnd,
    Unknown,
}

pub struct RawMode {
    original: libc::termios,
}

static RAW_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ALTERNATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static mut SAVED_TERMIOS: Option<libc::termios> = None;

pub extern "C" fn restore_terminal_and_exit(signal: libc::c_int) {
    // SAFETY: the handler only reads the saved termios and exits immediately.
    unsafe {
        if RAW_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
            let saved = std::ptr::addr_of!(SAVED_TERMIOS);
            if let Some(termios) = (*saved).as_ref() {
                libc::tcsetattr(0, libc::TCSANOW, termios);
            }
        }
        let clear = b"\x1b[?2004l";
        libc::write(1, clear.as_ptr() as *const _, clear.len());
        if ALTERNATE.load(std::sync::atomic::Ordering::Relaxed) {
            let leave = b"\x1b[?1049l";
            libc::write(1, leave.as_ptr() as *const _, leave.len());
        }
        libc::_exit(128 + signal);
    }
}

impl RawMode {
    pub fn enable() -> Option<Self> {
        // SAFETY: termios is a plain C struct initialised by tcgetattr.
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut termios) != 0 {
                return None;
            }
            let original = termios;
            termios.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
            termios.c_iflag &= !(libc::IXON | libc::ICRNL);
            termios.c_cc[libc::VMIN] = 1;
            termios.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSADRAIN, &termios) != 0 {
                return None;
            }
            SAVED_TERMIOS = Some(original);
            RAW_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = write_stdout("\x1b[?2004h");
            Some(RawMode { original })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = write_stdout("\x1b[?2004l");
        RAW_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
        // SAFETY: restoring the termios captured in enable().
        unsafe {
            libc::tcsetattr(0, libc::TCSADRAIN, &self.original);
        }
    }
}

pub fn is_tty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

pub fn width() -> usize {
    // SAFETY: winsize is a plain C struct filled by TIOCGWINSZ on a valid fd.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0 {
            size.ws_col as usize
        } else {
            80
        }
    }
}

pub fn read_paste() -> io::Result<Vec<u8>> {
    const MARKER: &[u8] = b"\x1b[201~";
    let mut bytes = Vec::new();
    let mut matched = 0;
    loop {
        let mut byte = [0u8; 1];
        let count = unsafe { libc::read(0, byte.as_mut_ptr() as *mut _, 1) };
        if count <= 0 {
            bytes.extend(MARKER[..matched].iter());
            break;
        }
        let byte = byte[0];
        if matched > 0 && byte == MARKER[matched] {
            matched += 1;
            if matched == MARKER.len() {
                break;
            }
            continue;
        }
        if matched > 0 {
            bytes.extend(MARKER[..matched].iter());
            matched = 0;
        }
        if byte == MARKER[0] {
            matched = 1;
        } else {
            bytes.push(byte);
        }
    }
    Ok(bytes)
}

pub fn enter_alternate_screen() -> io::Result<()> {
    ALTERNATE.store(true, std::sync::atomic::Ordering::Relaxed);
    write_stdout("\x1b[?1049h")
}

pub fn leave_alternate_screen() -> io::Result<()> {
    ALTERNATE.store(false, std::sync::atomic::Ordering::Relaxed);
    write_stdout("\x1b[?1049l")
}

pub fn read_key_timeout(millis: i32) -> io::Result<Option<Key>> {
    let mut pollfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd for standard input.
    let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
    if ready <= 0 {
        return Ok(None);
    }
    read_key().map(Some)
}

pub fn read_key() -> io::Result<Key> {
    let mut byte = [0u8; 1];
    let count = unsafe { libc::read(0, byte.as_mut_ptr() as *mut _, 1) };
    if count <= 0 {
        return Ok(Key::Escape);
    }

    let key = match byte[0] {
        0x03 => Key::Ctrl('c'),
        0x04 => Key::Ctrl('d'),
        0x0c => Key::Ctrl('l'),
        0x01 => Key::Ctrl('a'),
        0x05 => Key::Ctrl('e'),
        0x07 => Key::Ctrl('g'),
        0x12 => Key::Ctrl('r'),
        0x0b => Key::Ctrl('k'),
        0x15 => Key::Ctrl('u'),
        0x17 => Key::Ctrl('w'),
        0x1f => Key::Ctrl('_'),
        0x7f | 0x08 => Key::Backspace,
        b'\r' | b'\n' => Key::Enter,
        b'\t' => Key::Tab,
        0x1b => read_escape()?,
        byte @ 0x01..=0x1a => Key::Ctrl((b'a' + byte - 1) as char),
        byte if byte < 0x20 => Key::Unknown,
        byte if byte < 0x80 => Key::Char(byte as char),
        byte => match read_utf8(byte)? {
            Some(char) => Key::Char(char),
            None => Key::Unknown,
        },
    };

    Ok(key)
}

fn read_utf8(first: u8) -> io::Result<Option<char>> {
    let extra = match first {
        0xc0..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf7 => 3,
        _ => return Ok(None),
    };

    let mut bytes = vec![first];
    for _ in 0..extra {
        let mut byte = [0u8; 1];
        let count = unsafe { libc::read(0, byte.as_mut_ptr() as *mut _, 1) };
        if count <= 0 {
            return Ok(None);
        }
        bytes.push(byte[0]);
    }

    Ok(String::from_utf8(bytes).ok().and_then(|text| text.chars().next()))
}

fn read_escape() -> io::Result<Key> {
    let Some(next) = read_byte_with_timeout(20)? else {
        return Ok(Key::Escape);
    };
    if next != b'[' && next != b'O' {
        return Ok(Key::Alt(next as char));
    }

    let Some(code) = read_byte_with_timeout(20)? else {
        return Ok(Key::Escape);
    };

    let key = match code {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'Z' => Key::ShiftTab,
        digit => {
            let mut number = String::from(digit as char);
            while let Some(byte) = read_byte_with_timeout(20)? {
                if byte == b'~' {
                    break;
                }
                number.push(byte as char);
            }
            match number.as_str() {
                "1" | "7" => Key::Home,
                "4" | "8" => Key::End,
                "3" => Key::Delete,
                "200" => Key::PasteStart,
                "201" => Key::PasteEnd,
                _ => Key::Unknown,
            }
        }
    };

    Ok(key)
}

fn read_byte_with_timeout(milliseconds: i32) -> io::Result<Option<u8>> {
    let mut pollfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: single valid pollfd on the stack.
    let ready = unsafe { libc::poll(&mut pollfd, 1, milliseconds) };
    if ready <= 0 {
        return Ok(None);
    }

    let mut byte = [0u8; 1];
    let count = unsafe { libc::read(0, byte.as_mut_ptr() as *mut _, 1) };
    if count <= 0 {
        return Ok(None);
    }
    Ok(Some(byte[0]))
}

pub const MARK_PROMPT_START: &str = "\x1b]133;A\x1b\\";
pub const MARK_INPUT_START: &str = "\x1b]133;B\x1b\\";

pub fn mark_command_start(command: &str) -> String {
    format!(
        "\x1b]133;C;cmdline_url={}\x1b\\",
        percent_encode(command, false)
    )
}

pub fn mark_command_end(status: i32) -> String {
    format!("\x1b]133;D;{status}\x1b\\")
}

pub fn mark_working_directory(path: &std::path::Path) -> String {
    format!(
        "\x1b]7;file://{}{}\x1b\\",
        hostname(),
        percent_encode(&path.to_string_lossy(), true)
    )
}

pub fn percent_encode(value: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~')
            || (keep_slash && *byte == b'/');
        if keep {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_string())
        .unwrap_or_default()
}

pub fn write_stdout(text: &str) -> io::Result<()> {
    use std::io::Write;
    let mut out = io::stdout();
    out.write_all(text.as_bytes())?;
    out.flush()
}

pub fn read_stdin_lines() -> io::Result<String> {
    let mut text = String::new();
    io::stdin().read_to_string(&mut text)?;
    Ok(text)
}

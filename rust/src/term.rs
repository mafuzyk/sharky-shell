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
    Ctrl(char),
    Escape,
    Unknown,
}

pub struct RawMode {
    original: libc::termios,
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
            Some(RawMode { original })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
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
        0x0b => Key::Ctrl('k'),
        0x15 => Key::Ctrl('u'),
        0x17 => Key::Ctrl('w'),
        0x7f | 0x08 => Key::Backspace,
        b'\r' | b'\n' => Key::Enter,
        b'\t' => Key::Tab,
        0x1b => read_escape()?,
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
        return Ok(Key::Escape);
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

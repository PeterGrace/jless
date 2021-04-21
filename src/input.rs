use signal_hook::consts::SIGWINCH;
use signal_hook::low_level::pipe;
use termion::event::{parse_event, Event, Key, MouseEvent};

use std::fs::File;
use std::io;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;

const POLL_INFINITE_TIMEOUT: i32 = -1;
const SIGWINCH_PIPE_INDEX: usize = 0;
const BUFFER_SIZE: usize = 1024;

pub fn get_input() -> impl Iterator<Item = io::Result<TuiEvent>> {
    let tty = File::open("/dev/tty").unwrap();
    let (sigwinch_read, sigwinch_write) = UnixStream::pair().unwrap();
    pipe::register(SIGWINCH, sigwinch_write).unwrap();
    TuiInput::new(tty, sigwinch_read)
}

fn read_and_retry_on_interrupt(input: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match input.read(buf) {
            res @ Ok(_) => {
                return res;
            }
            Err(err) => {
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
                // Otherwise just try again
            }
        }
    }
}
struct BufferedInput<const N: usize> {
    input: File,
    buffer: [u8; N],
    buffer_size: usize,
    buffer_index: usize,
    might_have_more_data: bool,
}

impl<const N: usize> BufferedInput<N> {
    fn new(input: File) -> BufferedInput<N> {
        BufferedInput {
            input,
            buffer: [0; N],
            buffer_size: 0,
            buffer_index: 0,
            might_have_more_data: false,
        }
    }

    fn next_u8(&mut self) -> u8 {
        if self.buffer_index >= self.buffer_size {
            panic!("No data in buffer");
        }

        let val = self.buffer[self.buffer_index];
        self.buffer_index += 1;
        val
    }

    fn might_have_buffered_data(&self) -> bool {
        self.might_have_more_data || self.has_buffered_data()
    }

    fn has_buffered_data(&self) -> bool {
        self.buffer_index < self.buffer_size
    }
}

impl<const N: usize> Iterator for BufferedInput<N> {
    type Item = io::Result<u8>;

    fn next(&mut self) -> Option<io::Result<u8>> {
        if self.has_buffered_data() {
            return Some(Ok(self.next_u8()));
        }

        // buffer has been exhausted, clear it and read from its input again.
        self.buffer_size = 0;
        self.buffer_index = 0;
        self.might_have_more_data = false;

        match read_and_retry_on_interrupt(&mut self.input, &mut self.buffer) {
            Ok(bytes_read) => {
                self.buffer_size = bytes_read;
                self.might_have_more_data = bytes_read == N;
                return Some(Ok(self.next_u8()));
            }
            Err(err) => {
                return Some(Err(err));
            }
        }
    }
}

struct TuiInput {
    poll_fds: [libc::pollfd; 2],
    sigwinch_pipe: UnixStream,
    buffered_input: BufferedInput<BUFFER_SIZE>,
}

impl TuiInput {
    fn new(input: File, sigwinch_pipe: UnixStream) -> TuiInput {
        let sigwinch_fd = sigwinch_pipe.as_raw_fd();
        let stdin_fd = input.as_raw_fd();

        let poll_fds: [libc::pollfd; 2] = [
            libc::pollfd {
                fd: sigwinch_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];

        TuiInput {
            poll_fds,
            sigwinch_pipe,
            buffered_input: BufferedInput::new(input),
        }
    }

    fn get_event_from_buffered_input(&mut self) -> Option<io::Result<TuiEvent>> {
        match self.buffered_input.next() {
            Some(Ok(byte)) => {
                return match parse_event(byte, &mut self.buffered_input) {
                    Ok(Event::Key(k)) => Some(Ok(TuiEvent::KeyEvent(k))),
                    Ok(Event::Mouse(m)) => Some(Ok(TuiEvent::MouseEvent(m))),
                    Ok(Event::Unsupported(_)) => Some(Ok(TuiEvent::Unknown)),
                    Err(err) => Some(Err(err)),
                }
            }
            Some(Err(err)) => return Some(Err(err)),
            None => return None,
        }
    }
}

impl Iterator for TuiInput {
    type Item = io::Result<TuiEvent>;

    fn next(&mut self) -> Option<io::Result<TuiEvent>> {
        if self.buffered_input.might_have_buffered_data() {
            return self.get_event_from_buffered_input();
        }

        let poll_res: Option<io::Error>;

        loop {
            match unsafe { libc::poll(self.poll_fds.as_mut_ptr(), 2, POLL_INFINITE_TIMEOUT) } {
                -1 => {
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::Interrupted {
                        poll_res = Some(err);
                        break;
                    }
                    // Try poll again.
                }
                _ => {
                    poll_res = None;
                    break;
                }
            };
        }

        if poll_res.is_some() {
            return Some(Err(poll_res.unwrap()));
        }

        if self.poll_fds[SIGWINCH_PIPE_INDEX].revents & libc::POLLIN != 0 {
            // Just make this big enough to absorb a bunch of unacknowledged SIGWINCHes.
            let mut buf = [0; 32];
            let _ = self.sigwinch_pipe.read(&mut buf);
            return Some(Ok(TuiEvent::WinChEvent));
        }

        return self.get_event_from_buffered_input();
    }
}

#[derive(Debug)]
pub enum TuiEvent {
    WinChEvent,
    KeyEvent(Key),
    MouseEvent(MouseEvent),
    Unknown,
}

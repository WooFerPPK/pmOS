//! Standalone Principle VII conformance application.
//!
//! On `wasm32-wasip1` this binary calls the PMos `display_connect` extension,
//! uses only WASI `fd_read`/`fd_write` on the returned socket, and drives the
//! shipped display wire protocol through [`toolkit_free_client::SessionDriver`].
//! No toolkit code is linked. The app presents a distinctive 320x200 frame,
//! changes it after a keyboard event, and explicitly destroys its protocol
//! objects when the display server requests close.

#[cfg(target_arch = "wasm32")]
mod wasi_main {
    use toolkit_free_client::{SessionDriver, SessionSignal, WriteAttempt};

    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn fd_read(fd: i32, iovs_ptr: *const Iovec, iovs_len: i32, nread_ptr: *mut u32) -> i32;
        fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32)
            -> i32;
        fn poll_oneoff(
            subscriptions: *const u8,
            events: *mut u8,
            nsubscriptions: u32,
            nevents: *mut u32,
        ) -> i32;
    }

    #[link(wasm_import_module = "pmos_ext")]
    extern "C" {
        fn display_connect() -> i32;
    }

    const EAGAIN: i32 = 6;
    const ECONNREFUSED: i32 = 14;
    const CONNECT_RETRY_COUNT: u32 = 500;
    const CONNECT_RETRY_NS: u64 = 10_000_000;
    const SUBSCRIPTION_SIZE: usize = 48;
    const EVENT_SIZE: usize = 32;
    const EIO: i32 = 29;
    const EPIPE: i32 = 64;

    #[repr(C)]
    struct Ciovec {
        buf: *const u8,
        buf_len: u32,
    }

    #[repr(C)]
    struct Iovec {
        buf: *mut u8,
        buf_len: u32,
    }

    enum ReadResult {
        Data(Vec<u8>),
        Pending,
        Eof,
        Error(i32),
    }

    fn poll(subscriptions: &[[u8; SUBSCRIPTION_SIZE]]) -> Result<(), i32> {
        let mut events = [[0u8; EVENT_SIZE]; 2];
        let mut nevents = 0u32;
        let errno = unsafe {
            poll_oneoff(
                subscriptions.as_ptr().cast(),
                events.as_mut_ptr().cast(),
                subscriptions.len() as u32,
                &mut nevents,
            )
        };
        if errno != 0 {
            return Err(errno);
        }
        if nevents == 0 || nevents as usize > subscriptions.len() {
            return Err(EIO);
        }
        for event in &events[..nevents as usize] {
            let event_errno = u16::from_le_bytes([event[8], event[9]]) as i32;
            if event_errno != 0 {
                return Err(event_errno);
            }
            if u16::from_le_bytes([event[24], event[25]]) & 1 != 0 {
                return Err(EPIPE);
            }
        }
        Ok(())
    }

    fn fd_subscription(fd: i32, write: bool) -> [u8; SUBSCRIPTION_SIZE] {
        let mut subscription = [0u8; SUBSCRIPTION_SIZE];
        subscription[8] = if write { 2 } else { 1 };
        subscription[16..20].copy_from_slice(&(fd as u32).to_le_bytes());
        subscription
    }

    fn wait_display(fd: i32, write_pending: bool) -> Result<(), i32> {
        // Buffered inbound bytes must not short-circuit a send-side park while
        // the peer is full. Resume READ interest after the exact write suffix
        // has drained.
        poll(&[fd_subscription(fd, write_pending)])
    }

    fn wait_connect_retry() -> Result<(), i32> {
        let mut subscription = [0u8; SUBSCRIPTION_SIZE];
        subscription[8] = 0;
        subscription[16..20].copy_from_slice(&1u32.to_le_bytes());
        subscription[24..32].copy_from_slice(&CONNECT_RETRY_NS.to_le_bytes());
        poll(&[subscription])
    }

    fn connect() -> Result<i32, i32> {
        for _ in 0..CONNECT_RETRY_COUNT {
            let result = unsafe { display_connect() };
            if result >= 0 {
                return Ok(result);
            }
            if result == -ECONNREFUSED {
                wait_connect_retry()?;
                continue;
            }
            return Err(-result);
        }
        Err(ECONNREFUSED)
    }

    fn write_once(fd: i32, bytes: &[u8]) -> WriteAttempt {
        let iovec = Ciovec {
            buf: bytes.as_ptr(),
            buf_len: bytes.len() as u32,
        };
        let mut written = 0u32;
        let result = unsafe { fd_write(fd, &iovec, 1, &mut written) };
        if result == 0 {
            WriteAttempt::Written(written as usize)
        } else if result == EAGAIN {
            WriteAttempt::WouldBlock
        } else {
            WriteAttempt::Failed(result)
        }
    }

    fn read_once(fd: i32) -> ReadResult {
        let mut bytes = [0u8; 32 * 1024];
        let iovec = Iovec {
            buf: bytes.as_mut_ptr(),
            buf_len: bytes.len() as u32,
        };
        let mut read = 0u32;
        let result = unsafe { fd_read(fd, &iovec, 1, &mut read) };
        if result == 0 && read > 0 {
            return ReadResult::Data(bytes[..read as usize].to_vec());
        }
        if result == EAGAIN {
            return ReadResult::Pending;
        }
        if result == 0 {
            return ReadResult::Eof;
        }
        ReadResult::Error(result)
    }

    fn log_signals(signals: Vec<SessionSignal>) {
        for signal in signals {
            match signal {
                SessionSignal::GlobalsBound => {
                    println!("toolkit-free-client: discovered and bound compositor/shm/xdg/seat/keyboard");
                }
                SessionSignal::Configured {
                    serial,
                    width,
                    height,
                } => {
                    println!(
                        "toolkit-free-client: configured serial={serial} size={width}x{height}"
                    );
                }
                SessionSignal::FramePresented {
                    input_response: false,
                } => {
                    println!("toolkit-free-client: presented raw 320x200 frame rgba=19d3b3ff");
                }
                SessionSignal::FramePresented {
                    input_response: true,
                } => {
                    println!("toolkit-free-client: keyboard response frame rgba=ff8a1fff");
                }
                SessionSignal::Key { key, state } => {
                    println!("toolkit-free-client: keyboard event key={key} state={state}");
                }
                SessionSignal::BufferReleased(buffer) => {
                    println!("toolkit-free-client: buffer {} released", buffer.raw());
                }
                SessionSignal::CloseRequested => {
                    println!("toolkit-free-client: close requested");
                }
            }
        }
    }

    pub fn run() -> Result<(), String> {
        println!("toolkit-free-client: starting raw protocol client");
        let fd = connect().map_err(|errno| format!("display_connect errno={errno}"))?;
        println!("toolkit-free-client: connected fd={fd}");

        let mut driver =
            SessionDriver::new().map_err(|error| format!("session init: {error:?}"))?;

        loop {
            match read_once(fd) {
                ReadResult::Data(bytes) => {
                    let signals = driver
                        .push_server_bytes(&bytes)
                        .map_err(|error| format!("protocol: {error:?}"))?;
                    log_signals(signals);
                }
                ReadResult::Pending => {}
                ReadResult::Eof => return Err("display server closed the socket".to_string()),
                ReadResult::Error(errno) => return Err(format!("display read errno={errno}")),
            }

            let turn = driver
                .write_turn(|bytes| write_once(fd, bytes))
                .map_err(|error| format!("protocol write: {error:?}"))?;
            log_signals(turn.signals);
            if driver.shutdown_complete() {
                println!("toolkit-free-client: clean exit");
                return Ok(());
            }

            wait_display(fd, driver.wants_write())
                .map_err(|errno| format!("display wait errno={errno}"))?;
        }
    }
}

fn main() {
    #[cfg(target_arch = "wasm32")]
    {
        if let Err(error) = wasi_main::run() {
            eprintln!("toolkit-free-client: {error}");
            std::process::exit(1);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    println!(
        "toolkit-free-client: build for wasm32-wasip1 and run inside PMos to exercise /run/display"
    );
}

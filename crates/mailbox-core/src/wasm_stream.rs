//! A blocking browser byte stream over a SharedArrayBuffer mailbox.
//!
//! The core runs in a dedicated Worker, so `Atomics.wait` blocks only that
//! worker. The page's main thread owns the WebSocket and services the small
//! request/response protocol below. Bytes above this layer are ordinary
//! `Read`/`Write`, exactly like the native TCP transport.

use std::cell::{Cell, RefCell};
use std::io::{self, Read, Write};
use std::time::Duration;

use js_sys::{Atomics, Int32Array, Uint8Array};

/// Control-array offsets. Keep these in sync with `web/sab-bridge.js`.
const STATE: u32 = 0;
const LENGTH: u32 = 1;

const IDLE: i32 = 0;
const READ_REQUEST: i32 = 1;
const READ_READY: i32 = 2;
const WRITE_REQUEST: i32 = 3;
const WRITE_DONE: i32 = 4;
const CLOSED: i32 = -1;
const FAILED: i32 = -2;

#[derive(Clone)]
struct Bridge {
    control: Int32Array,
    data: Uint8Array,
}

thread_local! {
    static INSTALLED: RefCell<Option<Bridge>> = const { RefCell::new(None) };
}

/// Installs the buffers supplied to the Worker by the page.
pub fn install(control: Int32Array, data: Uint8Array) -> Result<(), String> {
    if control.length() < 2 {
        return Err("relay control buffer needs at least two i32 slots".into());
    }
    if data.length() == 0 {
        return Err("relay data buffer is empty".into());
    }
    // Loading through Atomics also proves that this really is shared memory.
    // The page owns lifecycle state, so do not overwrite a close/failure that
    // may have arrived while this Worker was loading its wasm module.
    Atomics::load(&control, STATE).map_err(js_error)?;
    INSTALLED.with(|slot| {
        *slot.borrow_mut() = Some(Bridge { control, data });
    });
    Ok(())
}

/// The browser counterpart of `TcpStream`.
pub struct BrowserStream {
    bridge: Bridge,
    read_timeout: Cell<Duration>,
    write_timeout: Cell<Duration>,
}

impl BrowserStream {
    /// Takes the bridge installed for this Worker. The destination is selected
    /// by the page/relay; `host` and `port` remain in the signature so the
    /// native and browser connection paths have the same policy inputs.
    pub fn connect(_host: &str, _port: u16) -> io::Result<BrowserStream> {
        let bridge = INSTALLED
            .with(|slot| slot.borrow().clone())
            .ok_or_else(|| io::Error::other("browser relay bridge is not installed"))?;
        Ok(BrowserStream {
            bridge,
            read_timeout: Cell::new(Duration::from_secs(30)),
            write_timeout: Cell::new(Duration::from_secs(30)),
        })
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.read_timeout
            .set(timeout.unwrap_or(Duration::from_secs(30)));
        Ok(())
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.write_timeout
            .set(timeout.unwrap_or(Duration::from_secs(30)));
        Ok(())
    }

    fn request(&self, request: i32, length: usize, timeout: Duration) -> io::Result<i32> {
        match Atomics::load(&self.bridge.control, STATE).map_err(io_js_error)? {
            CLOSED => return Ok(CLOSED),
            FAILED => return Ok(FAILED),
            IDLE => {}
            state => {
                return Err(io::Error::other(format!(
                    "browser relay was busy before request: state {state}"
                )))
            }
        }
        Atomics::store(&self.bridge.control, LENGTH, length as i32).map_err(io_js_error)?;
        Atomics::store(&self.bridge.control, STATE, request).map_err(io_js_error)?;
        Atomics::notify(&self.bridge.control, STATE).map_err(io_js_error)?;

        Atomics::wait_with_timeout(
            &self.bridge.control,
            STATE,
            request,
            timeout.as_secs_f64() * 1_000.0,
        )
        .map_err(io_js_error)?;

        let state = Atomics::load(&self.bridge.control, STATE).map_err(io_js_error)?;
        if state == request {
            let previous = Atomics::compare_exchange(&self.bridge.control, STATE, request, IDLE)
                .map_err(io_js_error)?;
            if previous == request {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "browser relay timed out",
                ));
            }
            return Atomics::load(&self.bridge.control, STATE).map_err(io_js_error);
        }
        Ok(state)
    }

    fn finish(&self) -> io::Result<()> {
        Atomics::store(&self.bridge.control, STATE, IDLE).map_err(io_js_error)?;
        Atomics::store(&self.bridge.control, LENGTH, 0).map_err(io_js_error)?;
        Ok(())
    }
}

impl Read for BrowserStream {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let requested = output.len().min(self.bridge.data.length() as usize);
        match self.request(READ_REQUEST, requested, self.read_timeout.get())? {
            READ_READY => {
                let length = Atomics::load(&self.bridge.control, LENGTH).map_err(io_js_error)?;
                if length < 0 || length as usize > requested {
                    return Err(io::Error::other(
                        "browser relay returned an invalid read length",
                    ));
                }
                let length = length as usize;
                self.bridge
                    .data
                    .subarray(0, length as u32)
                    .copy_to(&mut output[..length]);
                self.finish()?;
                Ok(length)
            }
            CLOSED => Ok(0),
            FAILED => Err(io::Error::other("browser relay failed")),
            state => Err(io::Error::other(format!(
                "browser relay returned unexpected read state {state}"
            ))),
        }
    }
}

impl Write for BrowserStream {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.is_empty() {
            return Ok(0);
        }
        let length = input.len().min(self.bridge.data.length() as usize);
        self.bridge
            .data
            .subarray(0, length as u32)
            .copy_from(&input[..length]);
        match self.request(WRITE_REQUEST, length, self.write_timeout.get())? {
            WRITE_DONE => {
                self.finish()?;
                Ok(length)
            }
            CLOSED => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "browser relay closed",
            )),
            FAILED => Err(io::Error::other("browser relay failed")),
            state => Err(io::Error::other(format!(
                "browser relay returned unexpected write state {state}"
            ))),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn js_error(error: wasm_bindgen::JsValue) -> String {
    error.as_string().unwrap_or_else(|| format!("{error:?}"))
}

fn io_js_error(error: wasm_bindgen::JsValue) -> io::Error {
    io::Error::other(js_error(error))
}

use std::io;
use std::ops::Deref;
use std::path::Path;
use std::time::Duration;
use std::sync::atomic::Ordering;

use libc;
use io::CoIo;
use yield_now::yield_with;
use scheduler::get_scheduler;
use os::unix::net::UnixStream;
use sync::delay_drop::DelayDrop;
use socket2::{Domain, SockAddr, Socket, Type};
use super::super::{add_socket, co_io_result, IoData};
use coroutine_impl::{co_cancel_data, CoroutineImpl, EventSource};

pub struct UnixStreamConnect {
    io_data: IoData,
    stream: Socket,
    path: SockAddr,
    can_drop: DelayDrop,
    is_connected: bool,
}

impl UnixStreamConnect {
    pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = SockAddr::unix(path)?;
        let socket = Socket::new(Domain::unix(), Type::stream(), None)?;
        // before yield we must set the socket to nonblocking mode and registe to selector
        socket.set_nonblocking(true)?;
        add_socket(&socket).map(|io| UnixStreamConnect {
            io_data: io,
            stream: socket,
            path: path,
            can_drop: DelayDrop::new(),
            is_connected: false,
        })
    }

    #[inline]
    // return ture if it's connected
    pub fn is_connected(&mut self) -> io::Result<bool> {
        // unix connect is some like completion mode
        // we must give the connect request first to the system
        match self.stream.connect(&self.path) {
            Ok(_) => {
                self.is_connected = true;
                Ok(true)
            }
            Err(ref e) if e.raw_os_error() == Some(libc::EINPROGRESS) => Ok(false),
            Err(e) => Err(e),
        }
    }

    #[inline]
    pub fn done(self) -> io::Result<UnixStream> {
        fn convert_to_stream(s: UnixStreamConnect) -> UnixStream {
            let stream = s.stream.into_unix_stream();
            UnixStream::from_coio(CoIo::from_raw(stream, s.io_data))
        }

        // first check if it's already connected
        if self.is_connected {
            return Ok(convert_to_stream(self));
        }

        loop {
            co_io_result()?;

            // clear the io_flag
            self.io_data.io_flag.store(false, Ordering::Relaxed);

            match self.stream.connect(&self.path) {
                Ok(_) => return Ok(convert_to_stream(self)),
                Err(ref e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {}
                Err(ref e) if e.raw_os_error() == Some(libc::EALREADY) => {}
                Err(ref e) if e.raw_os_error() == Some(libc::EISCONN) => {
                    return Ok(convert_to_stream(self));
                }
                Err(e) => return Err(e),
            }

            if self.io_data.io_flag.swap(false, Ordering::Relaxed) {
                continue;
            }

            // the result is still EINPROGRESS, need to try again
            self.can_drop.reset();
            yield_with(&self);
        }
    }
}

impl EventSource for UnixStreamConnect {
    fn subscribe(&mut self, co: CoroutineImpl) {
        let _g = self.can_drop.delay_drop();
        let cancel = co_cancel_data(&co);
        let io_data = &self.io_data;
        get_scheduler()
            .get_selector()
            .add_io_timer(io_data, Some(Duration::from_secs(10)));
        io_data.co.swap(co, Ordering::Release);

        // there is event, re-run the coroutine
        if io_data.io_flag.load(Ordering::Relaxed) {
            return io_data.schedule();
        }

        // register the cancel io data
        cancel.set_io(self.io_data.deref().clone());
        // re-check the cancel status
        if cancel.is_canceled() {
            unsafe { cancel.cancel() };
        }
    }
}

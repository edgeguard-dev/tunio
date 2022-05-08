use bytes::{BufMut, Bytes, BytesMut};
use crossbeam_channel::TryRecvError;
use log::warn;
use std::cmp::min;
use std::io::{Read, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::{io, thread};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
use windows::{
    Win32::Foundation::{ERROR_NO_MORE_ITEMS, HANDLE},
    Win32::System::Threading::{WaitForMultipleObjects, WAIT_OBJECT_0},
    Win32::System::WindowsProgramming::INFINITE,
};

use crate::platform::wintun::event::SafeEvent;
use crate::platform::wintun::handle::HandleWrapper;
use crate::traits::QueueT;
use wintun_sys::{DWORD, WINTUN_SESSION_HANDLE};

impl QueueT for Queue {}

pub struct Queue {
    session_handle: HandleWrapper<WINTUN_SESSION_HANDLE>,

    wintun: Arc<wintun_sys::wintun>,

    // Reader
    shutdown_event: Arc<SafeEvent>,

    reader_thread: Option<thread::JoinHandle<()>>,
    packet_rx: crossbeam_channel::Receiver<Bytes>,

    reader_wakers_tx: crossbeam_channel::Sender<Waker>,

    // Writer
    write_status_tx: crossbeam_channel::Sender<io::Result<usize>>,
    write_status_rx: crossbeam_channel::Receiver<io::Result<usize>>,
    packet_writer_thread: Option<tokio::task::JoinHandle<()>>,
}

const WAIT_OBJECT_1: u32 = WAIT_OBJECT_0 + 1;

impl Queue {
    pub fn new(
        handle: HandleWrapper<WINTUN_SESSION_HANDLE>,
        wintun: Arc<wintun_sys::wintun>,
    ) -> Self {
        let shutdown_event = Arc::new(SafeEvent::new());

        let inner_handle = handle.clone();
        let inner_wintun = wintun.clone();
        let inner_shutdown_event = shutdown_event.clone();

        let (packet_tx, packet_rx) = crossbeam_channel::bounded(16);
        let (reader_wakers_tx, reader_wakers_rx) = crossbeam_channel::unbounded();

        let reader_thread = Some(thread::spawn(move || {
            Self::reader_thread(
                inner_wintun,
                inner_handle,
                inner_shutdown_event,
                packet_tx,
                reader_wakers_rx,
            )
        }));

        let (write_status_tx, write_status_rx) = crossbeam_channel::bounded(1);

        Queue {
            session_handle: handle,
            wintun,
            shutdown_event,
            packet_rx,
            reader_thread,
            reader_wakers_tx,
            write_status_tx,
            write_status_rx,
            packet_writer_thread: None,
        }
    }

    fn reader_thread(
        wintun: Arc<wintun_sys::wintun>,
        handle: HandleWrapper<WINTUN_SESSION_HANDLE>,
        cmd_event: Arc<SafeEvent>,
        packet_tx: crossbeam_channel::Sender<Bytes>,
        wakers_rx: crossbeam_channel::Receiver<Waker>,
    ) {
        let read_event = HANDLE(unsafe { wintun.WintunGetReadWaitEvent(handle.0) as isize });
        let mut buffer = BytesMut::new(); // TODO: use with_capacity with full ring capacity

        'reader: loop {
            let mut packet_len: DWORD = 0;
            let packet = unsafe { wintun.WintunReceivePacket(handle.0, &mut packet_len) };

            if !packet.is_null() {
                unsafe {
                    let packet_slice = std::slice::from_raw_parts(packet, packet_len as usize);
                    buffer.put(packet_slice);
                    wintun.WintunReleaseReceivePacket(handle.0, packet)
                }
                packet_tx
                    .send(buffer.split().freeze())
                    .expect("Queue object is ok");

                // TODO: use single value channel or protected variable
                if let Some(waker) = wakers_rx.try_iter().last() {
                    waker.wake();
                }
            } else {
                let err = io::Error::last_os_error();
                if err.raw_os_error().unwrap() == ERROR_NO_MORE_ITEMS.0 as _ {
                    let result = unsafe {
                        WaitForMultipleObjects(&[cmd_event.0, read_event], false, INFINITE)
                    };
                    match result {
                        // Command
                        WAIT_OBJECT_0 => break 'reader,
                        // Ready for read
                        WAIT_OBJECT_1 => continue,

                        e => {
                            panic!("Unexpected event result: {e:?}");
                        }
                    }
                }
            }
        }
    }

    fn do_write(
        buf: &[u8],
        wintun: Arc<wintun_sys::wintun>,
        session_handle: HandleWrapper<WINTUN_SESSION_HANDLE>,
    ) -> io::Result<usize> {
        let packet = unsafe { wintun.WintunAllocateSendPacket(session_handle.0, buf.len() as _) };
        if !packet.is_null() {
            // Copy buffer to allocated packet
            unsafe {
                packet.copy_from_nonoverlapping(buf.as_ptr(), buf.len());
                wintun.WintunSendPacket(session_handle.0, packet); // Deallocates packet
            }
            Ok(buf.len())
        } else {
            let err = io::Error::last_os_error();
            if err.raw_os_error().unwrap() == ERROR_BUFFER_OVERFLOW.0 as _ {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                Err(err)
            }
        }
    }
}

impl Read for Queue {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.packet_rx.try_recv() {
            Err(TryRecvError::Empty) => Err(io::Error::from(io::ErrorKind::WouldBlock)),
            Err(TryRecvError::Disconnected) => Ok(0),
            Ok(message) => {
                let bytes_to_copy = min(buf.len(), message.len());
                if bytes_to_copy < buf.len() {
                    warn!("Data is truncated: {} > {}", buf.len(), bytes_to_copy);
                }
                buf.copy_from_slice(&message[..bytes_to_copy]);
                Ok(bytes_to_copy)
            }
        }
    }
}

impl Write for Queue {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Self::do_write(buf, self.wintun.clone(), self.session_handle.clone())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        // Set reader thread to stop eventually
        self.shutdown_event.set_event();
        // Join thread
        let _ = self.reader_thread.take().unwrap().join();

        unsafe {
            self.wintun.WintunEndSession(self.session_handle.0);
        }
    }
}

impl AsyncRead for Queue {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let self_mut = self.get_mut();
        let mut b = vec![0; buf.remaining()];

        match self_mut.read(b.as_mut_slice()) {
            Ok(n) => {
                buf.put_slice(&b[..n]);
                Poll::Ready(Ok(()))
            }
            Err(e) => {
                if e.kind() == io::ErrorKind::WouldBlock {
                    let _ = self_mut.reader_wakers_tx.send(cx.waker().clone());
                    Poll::Pending
                } else {
                    Poll::Ready(Err(e))
                }
            }
        }
    }
}

impl AsyncWrite for Queue {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let buffer = Bytes::copy_from_slice(buf);

        let inner_handle = HandleWrapper(self.session_handle.0);
        let inner_wintun = self.wintun.clone();
        let inner_write_status_tx = self.write_status_tx.clone();
        let waker = cx.waker().clone();

        if let Ok(result) = self.write_status_rx.try_recv() {
            Poll::Ready(result)
        } else {
            self.get_mut().packet_writer_thread = Some(tokio::task::spawn_blocking(move || {
                let inner_handle = inner_handle;

                let result = Self::do_write(&*buffer, inner_wintun.clone(), inner_handle.clone());

                let _ = inner_write_status_tx.send(result);
                waker.wake();
            }));
            Poll::Pending
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Not implemented by driver
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

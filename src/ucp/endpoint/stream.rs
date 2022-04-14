use futures::{AsyncRead, AsyncWrite};
use futures_lite::FutureExt;

use super::*;

impl Endpoint {
    pub async fn stream_send(&self, buf: &[u8]) -> Result<usize, Error> {
        trace!("stream_send: endpoint={:?} len={}", self.handle, buf.len());
        unsafe extern "C" fn callback(request: *mut c_void, status: ucs_status_t) {
            trace!(
                "stream_send: complete. req={:?}, status={:?}",
                request,
                status
            );
            let request = &mut *(request as *mut Request);
            request.waker.wake();
        }
        let status = unsafe {
            ucp_stream_send_nb(
                self.get_handle()?,
                buf.as_ptr() as _,
                buf.len() as _,
                ucp_dt_make_contig(1),
                Some(callback),
                0,
            )
        };
        if status.is_null() {
            trace!("stream_send: complete");
        } else if UCS_PTR_IS_PTR(status) {
            RequestHandle {
                ptr: status,
                poll_fn: poll_normal,
            }
            .await?;
        } else {
            return Err(Error::from_ptr(status).unwrap_err());
        }
        Ok(buf.len())
    }
    
    pub async fn stream_recv(&self, buf: &mut [MaybeUninit<u8>]) -> Result<usize, Error> {
        trace!("stream_recv: endpoint={:?} len={}", self.handle, buf.len());
        unsafe extern "C" fn callback(request: *mut c_void, status: ucs_status_t, length: u64) {
            trace!(
                "stream_recv: complete. req={:?}, status={:?}, len={}",
                request,
                status,
                length
            );
            let request = &mut *(request as *mut Request);
            request.waker.wake();
        }
        let mut length = MaybeUninit::uninit();
        let status = unsafe {
            ucp_stream_recv_nb(
                self.get_handle()?,
                buf.as_mut_ptr() as _,
                buf.len() as _,
                ucp_dt_make_contig(1),
                Some(callback),
                length.as_mut_ptr(),
                0,
            )
        };
        if status.is_null() {
            let length = unsafe { length.assume_init() } as usize;
            trace!("stream_recv: complete. len={}", length);
            Ok(length)
        } else if UCS_PTR_IS_PTR(status) {
            Ok(RequestHandle {
                ptr: status,
                poll_fn: poll_stream,
            }
            .await)
        } else {
            Err(Error::from_ptr(status).unwrap_err())
        }
    }
}

impl AsyncRead for AsyncStreamReaderEndpoint {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        trace!("poll_read: len={}", buf.len());
        let n = std::cmp::min(self.buffer.len(), buf.len());
        if n > 0 {
            buf[..n].copy_from_slice(&self.buffer[..n]);
            self.buffer.drain(..n);
            return Poll::Ready(Ok(n));
        }
        if let Some(ref mut handle) = self.handle {
            match handle.poll(cx) {
                Poll::Ready(n) => {
                    let n = std::cmp::min(n, buf.len());
                    buf[..n].copy_from_slice(&self.read_buffer[..n]);
                    self.read_buffer.drain(..n);
                    self.buffer = self.read_buffer.clone();
                    self.handle = None;
                    return Poll::Ready(Ok(n));
                }
                Poll::Pending => return Poll::Pending,
            }
        } else {
            self.read_buffer = vec![0; buf.len()];
            unsafe extern "C" fn callback(request: *mut c_void, status: ucs_status_t, length: u64) {
                trace!(
                    "stream_recv: complete. req={:?}, status={:?}, len={}",
                    request,
                    status,
                    length
                );
                let request = &mut *(request as *mut Request);
                request.waker.wake();
            }
            let handle = match self.endpoint.get_handle() {
                Ok(handle) => handle,
                Err(err) => {
                    return Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Other, err)))
                }
            };
            let mut length = MaybeUninit::uninit();
            let status = unsafe {
                ucp_stream_recv_nb(
                    handle,
                    self.read_buffer.as_mut_ptr() as _,
                    self.read_buffer.len() as _,
                    ucp_dt_make_contig(1),
                    Some(callback),
                    length.as_mut_ptr(),
                    0,
                )
            };

            if status.is_null() {
                let length = unsafe { length.assume_init() } as usize;
                trace!("stream_recv: complete. len={}", length);
                let n = std::cmp::min(length, buf.len());
                buf[..n].copy_from_slice(&self.read_buffer[..n]);
                self.read_buffer.drain(..n);
                self.buffer = self.read_buffer.clone();
                return Poll::Ready(Ok(n));
            } else if UCS_PTR_IS_PTR(status) {
                let mut handle = RequestHandle {
                    ptr: status,
                    poll_fn: poll_stream,
                };
                match handle.poll(cx) {
                    Poll::Ready(n) => {
                        let n = std::cmp::min(n, buf.len());
                        buf[..n].copy_from_slice(&self.read_buffer[..n]);
                        self.read_buffer.drain(..n);
                        self.buffer = self.read_buffer.clone();
                        return Poll::Ready(Ok(n));
                    }
                    Poll::Pending => {
                        self.handle = Some(handle);
                        return Poll::Pending;
                    }
                }
            } else {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    Error::from_ptr(status).unwrap_err(),
                )));
            }
        }
    }
}

impl AsyncWrite for AsyncStreamWriterEndpoint {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        trace!("poll_write: len={}", buf.len());
        if let Some(ref mut handle) = self.handle {
            match handle.poll(cx) {
                Poll::Ready(_) => {
                    self.handle = None;
                    return Poll::Ready(Ok(buf.len()));
                }
                Poll::Pending => return Poll::Pending,
            }
        } else {
            unsafe extern "C" fn callback(request: *mut c_void, status: ucs_status_t) {
                trace!(
                    "stream_send: complete. req={:?}, status={:?}",
                    request,
                    status
                );
                let request = &mut *(request as *mut Request);
                request.waker.wake();
            }
            let handle = match self.endpoint.get_handle() {
                Ok(handle) => handle,
                Err(err) => {
                    return Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Other, err)))
                }
            };
            let status = unsafe {
                ucp_stream_send_nb(
                    handle,
                    buf.as_ptr() as _,
                    buf.len() as _,
                    ucp_dt_make_contig(1),
                    Some(callback),
                    0,
                )
            };
            if status.is_null() {
                trace!("stream_send: complete");
                return Poll::Ready(Ok(buf.len()));
            } else if UCS_PTR_IS_PTR(status) {
                let mut handle = RequestHandle {
                    ptr: status,
                    poll_fn: poll_normal,
                };
                match handle.poll(cx) {
                    Poll::Ready(_) => {
                        self.handle = None;
                        return Poll::Ready(Ok(buf.len()));
                    }
                    Poll::Pending => {
                        self.handle = Some(handle);
                        return Poll::Pending;
                    }
                }
            } else {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    Error::from_ptr(status).unwrap_err(),
                )));
            }
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Some(ref mut handle) = self.flush_handle {
            match handle.poll(cx) {
                Poll::Ready(_) => {
                    self.flush_handle = None;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        } else {
            let handle = self.endpoint.get_handle().unwrap();
            unsafe extern "C" fn callback(request: *mut c_void, _status: ucs_status_t) {
                trace!("flush: complete");
                let request = &mut *(request as *mut Request);
                request.waker.wake();
            }
            let status = unsafe { ucp_ep_flush_nb(handle, 0, Some(callback)) };
            if status.is_null() {
                trace!("flush: complete");
                return Poll::Ready(Ok(()));
            } else if UCS_PTR_IS_PTR(status) {
                let mut handle = RequestHandle {
                    ptr: status,
                    poll_fn: poll_normal,
                };
                match handle.poll(cx) {
                    Poll::Ready(_) => {
                        self.flush_handle = None;
                        return Poll::Ready(Ok(()));
                    }
                    Poll::Pending => {
                        self.flush_handle = Some(handle);
                        return Poll::Pending;
                    }
                }
            } else {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    Error::from_ptr(status).unwrap_err(),
                )));
            }
        }
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        todo!()
    }
}

unsafe fn poll_stream(ptr: ucs_status_ptr_t) -> Poll<usize> {
    let mut len = MaybeUninit::<usize>::uninit();
    let status = ucp_stream_recv_request_test(ptr as _, len.as_mut_ptr() as _);
    if status == ucs_status_t::UCS_INPROGRESS {
        Poll::Pending
    } else {
        Poll::Ready(len.assume_init())
    }
}

use pyo3::{Py, PyAny, Python};
use songbird::input::core::io::MediaSource;
use std::io;
use std::io::{ErrorKind, SeekFrom};

// An io.BufferIOBase wrapper
pub struct PyBufferIO(pub Py<PyAny>);

impl io::Read for PyBufferIO {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let length = Python::with_gil::<_, io::Result<usize>>(|py| {
            let method = self
                .0
                .getattr(py, "read")
                .map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?;
            let bytes_read = method
                .call1(py, (buf.len(),))
                .map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?;
            let bytes: &[u8] = bytes_read
                .extract(py)
                .map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?;
            let len = if bytes.len() < buf.len() {
                bytes.len()
            } else {
                buf.len()
            };
            buf[..len].copy_from_slice(&bytes[..len]);
            Ok(len)
        })?;
        io::Result::Ok(length)
    }
}

impl io::Seek for PyBufferIO {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        Python::with_gil(|py| {
            let method = self.0.getattr(py, "seek").unwrap();
            let offset = match pos {
                SeekFrom::Start(n) => n as i64,
                SeekFrom::End(n) => n,
                SeekFrom::Current(n) => n,
            };
            let whence = match pos {
                SeekFrom::Start(_) => 0,
                SeekFrom::Current(_) => 1,
                SeekFrom::End(_) => 2,
            };
            let result: u64 = method
                .call1(py, (offset, whence))
                .unwrap()
                .extract(py)
                .unwrap();
            Ok(result)
        })
    }
}

impl MediaSource for PyBufferIO {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

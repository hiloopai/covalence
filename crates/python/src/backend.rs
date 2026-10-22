//! Python key-value backends at the native storage boundary.

use std::{
    fs::File,
    io::{self, Read, Write},
};

use covalence_kv_trait::{Error, Kv, KvFuture, ready};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use covalence_lib_python::pyo3;
use pyo3::{prelude::*, types::PyBytes};

use crate::PyValue;

fn backend(error: PyErr) -> Error {
    Error::Backend {
        source: Box::new(error),
    }
}

fn io_error(source: io::Error) -> Error {
    Error::Io { source }
}

pub(crate) struct PythonKv(pub(crate) Py<PyAny>);

impl Kv for PythonKv {
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>, Error> {
        Python::attach(|py| {
            let value = self
                .0
                .bind(py)
                .call_method1("get", (PyBytes::new(py, key),))?;
            if value.is_none() {
                Ok(None)
            } else {
                Ok(Some(Bytes::copy_from_slice(
                    value.cast::<PyBytes>()?.as_bytes(),
                )))
            }
        })
        .map_err(backend)
    }

    fn put(&self, key: &[u8], value: Bytes) -> Result<(), Error> {
        Python::attach(|py| {
            self.0
                .bind(py)
                .call_method1("put", (PyBytes::new(py, key), PyBytes::new(py, &value)))?;
            Ok(())
        })
        .map_err(backend)
    }

    fn exists(&self, key: &[u8]) -> Result<bool, Error> {
        let exists = Python::attach(|py| {
            let object = self.0.bind(py);
            if object.hasattr("exists")? {
                Ok(Some(
                    object
                        .call_method1("exists", (PyBytes::new(py, key),))?
                        .extract::<bool>()?,
                ))
            } else {
                Ok(None)
            }
        })
        .map_err(backend)?;
        match exists {
            Some(exists) => Ok(exists),
            None => Ok(self.get(key)?.is_some()),
        }
    }

    fn size(&self, key: &[u8]) -> Result<Option<u64>, Error> {
        let size = Python::attach(|py| {
            let object = self.0.bind(py);
            if object.hasattr("size")? {
                Ok(Some(
                    object
                        .call_method1("size", (PyBytes::new(py, key),))?
                        .extract::<Option<u64>>()?,
                ))
            } else {
                Ok(None)
            }
        })
        .map_err(backend)?;
        match size {
            Some(size) => Ok(size),
            None => Ok(self.get(key)?.map(|bytes| bytes.len() as u64)),
        }
    }

    fn put_from(&self, key: &[u8], reader: &mut dyn Read, len: u64) -> Result<(), Error> {
        let native = Python::attach(|py| self.0.bind(py).hasattr("put_file")).map_err(backend)?;
        if !native {
            let mut value = Vec::new();
            let read = reader.take(len).read_to_end(&mut value).map_err(io_error)?;
            if (read as u64) != len {
                return Err(io_error(io::ErrorKind::UnexpectedEof.into()));
            }
            return self.put(key, value.into());
        }
        let mut temp = tempfile::NamedTempFile::new().map_err(io_error)?;
        let read = io::copy(&mut reader.take(len), &mut temp).map_err(io_error)?;
        if read != len {
            return Err(io_error(io::ErrorKind::UnexpectedEof.into()));
        }
        temp.flush().map_err(io_error)?;
        Python::attach(|py| {
            self.0
                .bind(py)
                .call_method1("put_file", (PyBytes::new(py, key), temp.path()))?;
            Ok(())
        })
        .map_err(backend)
    }

    fn get_to(&self, key: &[u8], writer: &mut dyn Write) -> Result<Option<u64>, Error> {
        let native = Python::attach(|py| self.0.bind(py).hasattr("get_file")).map_err(backend)?;
        if !native {
            let Some(bytes) = self.get(key)? else {
                return Ok(None);
            };
            writer.write_all(&bytes).map_err(io_error)?;
            return Ok(Some(bytes.len() as u64));
        }
        let temp = tempfile::NamedTempFile::new().map_err(io_error)?;
        let len = Python::attach(|py| {
            self.0
                .bind(py)
                .call_method1("get_file", (PyBytes::new(py, key), temp.path()))?
                .extract::<Option<u64>>()
        })
        .map_err(backend)?;
        let Some(len) = len else {
            return Ok(None);
        };
        let mut file = File::open(temp.path()).map_err(io_error)?;
        let copied = io::copy(&mut file, writer).map_err(io_error)?;
        if copied != len {
            return Err(Error::Backend {
                source: "backend returned an incorrect file length".into(),
            });
        }
        Ok(Some(copied))
    }

    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }

    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        ready(self.put(key, value))
    }

    fn info(&self) -> Result<Value, Error> {
        Python::attach(|py| {
            let object = self.0.bind(py);
            if object.hasattr("info")? {
                Ok(PyValue::new(&object.call_method0("info")?)?.inner)
            } else {
                Ok(Value::Null)
            }
        })
        .map_err(backend)
    }
}

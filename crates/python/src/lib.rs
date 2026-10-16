mod errors;

use std::{
    collections::hash_map::DefaultHasher,
    fs::{File, Permissions},
    hash::{Hash, Hasher},
    io::{self, BufReader, Seek},
    path::{Path, PathBuf},
    sync::Arc,
};

use covalence_blob_trait::{Blob, BlobExt, Slice};
use covalence_cas_kv::KvCas;
use covalence_cas_memory::MemoryCas;
use covalence_cas_sqlite::SqliteCas;
use covalence_cas_trait::{Cas, GetExt};
use covalence_kv_memory::MemoryKv;
use covalence_kv_s3::{AwsOptions, S3Kv, S3Location};
use covalence_kv_trait::Kv;
use covalence_lib_cid::{Bdasl, Blake3, Cid, Cid64, Fact, Format, Multicodec};
use covalence_lib_dasl::{Builder, Error, Event, Value};
use covalence_lib_data::Bytes;
use covalence_lib_python::pyo3;
use pyo3::{
    exceptions::{PyTypeError, PyValueError},
    intern,
    prelude::*,
    pybacked::{PyBackedBytes, PyBackedStr},
    types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyIterator, PyList, PyString, PyTuple},
};

use crate::errors::{Class, raise, raise_as};

/// Bad input, as `ValueError`.
fn fail(error: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(error.to_string())
}

/// A code that fits in a CID.
fn code(code: u64) -> PyResult<Multicodec> {
    if code < 1 << 63 {
        Ok(Multicodec(code))
    } else {
        Err(fail(format!("code {code:#x} is too large")))
    }
}

/// Defines a Python class wrapping a [`Multicodec`], named by `$names`, the
/// first of each being the canonical name.
macro_rules! code_class {
    ($(#[$doc:meta])* $rust:ident, $py:literal, $arg:ident, $kind:literal, { $($code:expr => [$($name:literal),+]),+ $(,)? }) => {
        $(#[$doc])*
        #[pyclass(skip_from_py_object, frozen, eq, hash, name = $py, module = "covalence")]
        #[pyo3(crate = "covalence_lib_python::pyo3")]
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        struct $rust(Multicodec);

        impl $rust {
            fn named(name: &str) -> Option<Self> {
                match name {
                    $($($name)|+ => Some(Self($code)),)+
                    _ => None,
                }
            }

            fn name(&self) -> Option<&'static str> {
                match self.0 {
                    $(code if code == $code => Some([$($name),+][0]),)+
                    _ => None,
                }
            }
        }

        #[pymethods]
        #[pyo3(crate = "covalence_lib_python::pyo3")]
        impl $rust {
            #[new]
            fn new(name: &str) -> PyResult<Self> {
                Self::named(name).ok_or_else(|| fail(format!(concat!("unknown ", $kind, " {:?}"), name)))
            }

            #[doc = concat!("The ", $kind, " with multicodec code `code`, named or not.\n\n- A code from 2**63 to 2**64 - 1 raises ValueError.\n- A negative code, or one of 2**64 or more, raises OverflowError.")]
            #[staticmethod]
            fn from_code(code: u64) -> PyResult<Self> {
                Ok(Self(crate::code(code)?))
            }

            #[doc = concat!("The ", $kind, "'s name, or None if Covalence has none for it.")]
            #[getter(name)]
            fn py_name(&self) -> Option<&'static str> {
                self.name()
            }

            /// The multicodec code.
            #[getter]
            fn code(&self) -> u64 {
                self.0 .0
            }

            fn __str__(&self) -> String {
                self.name().map_or_else(|| format!("{:#x}", self.0 .0), str::to_owned)
            }

            fn __repr__(&self) -> String {
                match self.name() {
                    Some(name) => format!(concat!($py, "('{}')"), name),
                    None => format!(concat!($py, ".from_code({:#x})"), self.0 .0),
                }
            }
        }

        /// A `str` name or the class itself.
        #[derive(FromPyObject)]
        #[pyo3(crate = "covalence_lib_python::pyo3")]
        enum $arg<'py> {
            Object(PyRef<'py, $rust>),
            Name(PyBackedStr),
        }

        impl $arg<'_> {
            fn code(&self) -> PyResult<Multicodec> {
                match self {
                    $arg::Object(object) => Ok(object.0),
                    $arg::Name(name) => Ok($rust::new(name)?.0),
                }
            }
        }
    };
}

code_class!(
    /// A CID's codec, which says how its bytes are encoded.
    ///
    /// `Codec(name)` takes "raw", "dag-cbor" (or "drisl") or "git-raw"; other
    /// names raise ValueError. `from_code` takes any multicodec code. Codecs
    /// compare and hash by code.
    ///
    /// >>> from covalence import Codec
    /// >>> Codec('drisl')
    /// Codec('dag-cbor')
    /// >>> Codec('raw').code
    /// 85
    /// >>> Codec.from_code(0x99).name is None
    /// True
    PyCodec, "Codec", CodecArg, "codec", {
    Multicodec::RAW => ["raw"],
    Multicodec::DAG_CBOR => ["dag-cbor", "drisl"],
    Multicodec::GIT_RAW => ["git-raw"],
});

code_class!(
    /// A CID's hash function.
    ///
    /// `Hash(name)` takes a multicodec name ("sha1", "sha2-256", "sha2-512",
    /// "blake3") or a `hashlib` name ("sha256", "sha512"); other names raise
    /// ValueError. `from_code` takes any multicodec code. Hashes compare and
    /// hash by code.
    ///
    /// >>> from covalence import Hash
    /// >>> Hash('sha2-256')
    /// Hash('sha256')
    /// >>> Hash('blake3').code
    /// 30
    PyHash, "Hash", HashArg, "hash", {
    Multicodec::SHA1 => ["sha1"],
    Multicodec::SHA2_256 => ["sha256", "sha2-256"],
    Multicodec::SHA2_512 => ["sha512", "sha2-512"],
    Multicodec::BLAKE3 => ["blake3"],
});

/// A binary or text CID.
#[derive(FromPyObject)]
#[pyo3(crate = "covalence_lib_python::pyo3")]
enum CidArg {
    Text(PyBackedStr),
    Bytes(PyBackedBytes),
}

/// A content identifier: a CIDv1 with a digest of at most 64 bytes.
///
/// `Cid(cid)` parses text or binary, as `from_text` or `from_bytes` does.
/// `str` gives the text and `bytes` the binary form. CIDs compare and hash
/// by value, and match as `Cid(codec, hash, digest)`.
///
/// >>> from covalence import Cid
/// >>> cid = Cid.of(b'hello', 'sha256')
/// >>> Cid(str(cid)) == Cid(bytes(cid)) == cid
/// True
/// >>> cid.codec, cid.hash
/// (Codec('raw'), Hash('sha256'))
#[pyclass(
    skip_from_py_object,
    frozen,
    eq,
    hash,
    name = "Cid",
    module = "covalence"
)]
#[pyo3(crate = "covalence_lib_python::pyo3")]
#[derive(Clone, PartialEq, Eq, Hash)]
struct PyCid {
    inner: Cid64,
}

impl PyCid {
    fn new_cid(codec: Multicodec, hash: Multicodec, digest: &[u8]) -> PyResult<Self> {
        Ok(Self {
            inner: Cid64::new(Format { codec, hash }, digest).map_err(raise)?,
        })
    }
}

impl PyCid {
    /// The DASL CID, or `ValueError`.
    fn dasl(&self) -> PyResult<Cid<Bdasl>> {
        Cid::try_from(self.inner).map_err(|_| fail(format!("{} is not a DASL CID", self.inner)))
    }
}

impl PyCid {
    /// The CID as a store key, or `ValueError`.
    fn key(&self) -> PyResult<Cid> {
        Cid::try_from(self.inner)
            .map_err(|_| fail(format!("{} has a digest over 32 bytes", self.inner)))
    }
}

impl From<Cid<Bdasl>> for PyCid {
    fn from(cid: Cid<Bdasl>) -> Self {
        PyCid { inner: cid.into() }
    }
}

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PyCid {
    #[classattr]
    fn __match_args__() -> (&'static str, &'static str, &'static str) {
        ("codec", "hash", "digest")
    }

    #[new]
    fn new(cid: CidArg) -> PyResult<Self> {
        match cid {
            CidArg::Text(text) => Self::from_text(&text),
            CidArg::Bytes(bytes) => Self::from_bytes(bytes),
        }
    }

    /// Parse a CID's text: lowercase base32 with a `b` prefix.
    ///
    /// Invalid text raises ValueError.
    #[staticmethod]
    fn from_text(text: &str) -> PyResult<Self> {
        Ok(Self {
            inner: text.parse().map_err(raise)?,
        })
    }

    /// Parse a CID's binary form.
    ///
    /// Invalid bytes raise ValueError.
    #[staticmethod]
    fn from_bytes(data: PyBackedBytes) -> PyResult<Self> {
        Ok(Self {
            inner: Cid64::from_bytes(&data).map_err(raise)?,
        })
    }

    /// The CID made of `codec`, `hash` and `digest`.
    ///
    /// The codec and hash may be objects or names. The digest is not checked
    /// against the hash; one over 64 bytes raises ValueError.
    #[staticmethod]
    fn from_parts(codec: CodecArg<'_>, hash: HashArg<'_>, digest: PyBackedBytes) -> PyResult<Self> {
        Self::new_cid(codec.code()?, hash.code()?, &digest)
    }

    /// The CID of `data`, hashed with `hash` and labelled with `codec`.
    ///
    /// The defaults are BLAKE3 and raw. Covalence computes SHA-1, SHA-256,
    /// SHA-512 and BLAKE3; another hash raises UnsupportedError.
    #[staticmethod]
    #[pyo3(signature = (data, hash = None, codec = None))]
    fn of(
        py: Python<'_>,
        data: PyBackedBytes,
        hash: Option<HashArg<'_>>,
        codec: Option<CodecArg<'_>>,
    ) -> PyResult<Self> {
        let hash = hash.map_or(Ok(Multicodec::BLAKE3), |hash| hash.code())?;
        let codec = codec.map_or(Ok(Multicodec::RAW), |codec| codec.code())?;
        let digest = py
            .detach(|| hash.digest(&data))
            .ok_or_else(|| raise_as(Class::Unsupported, format!("cannot hash with {hash:?}")))?;
        Self::new_cid(codec, hash, &digest)
    }

    /// The raw BLAKE3 CID of the file at `path`, as `Cas.put_file` stores it.
    ///
    /// Reads the file without holding it in memory.
    #[staticmethod]
    fn of_file(py: Python<'_>, path: PathBuf) -> PyResult<Self> {
        let (_, cid, _) = py.detach(|| hash_file(&path))?;
        Ok(Self { inner: cid.into() })
    }

    /// The CID with the same hash and digest and the raw codec.
    fn to_raw(&self) -> Self {
        Self::new_cid(Multicodec::RAW, self.inner.hash(), self.inner.digest())
            .expect("only the codec changed")
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!("Cid('{}')", self.inner)
    }

    /// The binary form.
    fn __bytes__<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.to_bytes())
    }

    /// The codec.
    #[getter]
    fn codec(&self) -> PyCodec {
        PyCodec(self.inner.codec())
    }

    /// The hash function.
    #[getter]
    fn hash(&self) -> PyHash {
        PyHash(self.inner.hash())
    }

    /// The digest.
    #[getter]
    fn digest<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, self.inner.digest())
    }
}

// Nesting past `MAX_DEPTH` fails before recursing, which also stops cyclic input.
fn feed(builder: &mut Builder, obj: &Bound<'_, PyAny>) -> PyResult<()> {
    let mut push = |event| builder.push(event).map_err(raise);
    if let Ok(value) = obj.extract::<PyRef<'_, PyValue>>() {
        return value.inner.events().try_for_each(push);
    }
    if obj.is_none() {
        return push(Event::Null);
    }
    if obj.is_instance_of::<PyBool>() {
        return push(Event::Bool(obj.extract()?));
    }
    if obj.is_instance_of::<PyInt>() {
        let value = obj
            .extract()
            .map_err(|_| fail(format!("integer {obj} is outside i64")))?;
        return push(Event::Integer(value));
    }
    if obj.is_instance_of::<PyFloat>() {
        return Err(raise(Error::Float));
    }
    if let Ok(value) = obj.cast::<PyString>() {
        return push(Event::String(value.to_str()?));
    }
    if let Ok(cid) = obj.extract::<PyRef<'_, PyCid>>() {
        return push(Event::Link(cid.dasl()?));
    }
    let items: Option<Vec<_>> = match obj.cast::<PyList>() {
        Ok(list) => Some(list.iter().collect()),
        Err(_) => obj
            .cast::<PyTuple>()
            .ok()
            .map(|tuple| tuple.iter().collect()),
    };
    if let Some(items) = items {
        push(Event::List(items.len()))?;
        return items.iter().try_for_each(|item| feed(builder, item));
    }
    if let Ok(dict) = obj.cast::<PyDict>() {
        let mut entries = Vec::with_capacity(dict.len());
        for (key, value) in dict.iter() {
            let key = key
                .cast::<PyString>()
                .map_err(|_| PyTypeError::new_err("map keys must be strings"))?
                .to_str()?
                .to_owned();
            entries.push((key, value));
        }
        entries.sort_by(|(a, _), (b, _)| (a.len(), a).cmp(&(b.len(), b)));
        push(Event::Map(entries.len()))?;
        for (key, value) in &entries {
            builder.push(Event::String(key)).map_err(raise)?;
            feed(builder, value)?;
        }
        return Ok(());
    }
    let bytes = obj.extract::<PyBackedBytes>().map_err(|_| {
        let name = obj
            .get_type()
            .name()
            .map_or_else(|_| "?".into(), |name| name.to_string());
        PyTypeError::new_err(format!("{name} cannot be a DASL value"))
    })?;
    push(Event::Bytes(&bytes))
}

fn to_native(py: Python<'_>, value: &Value) -> PyResult<Py<PyAny>> {
    Ok(match value {
        Value::Null => py.None(),
        Value::Bool(value) => value.into_pyobject(py)?.to_owned().into_any().unbind(),
        Value::Integer(value) => value.into_pyobject(py)?.into_any().unbind(),
        Value::String(value) => value.into_pyobject(py)?.into_any().unbind(),
        Value::Bytes(value) => PyBytes::new(py, value).into_any().unbind(),
        Value::Link(cid) => Py::new(py, PyCid::from(*cid))?.into_any(),
        Value::List(items) => PyList::new(
            py,
            items
                .iter()
                .map(|item| to_native(py, item))
                .collect::<PyResult<Vec<_>>>()?,
        )?
        .into_any()
        .unbind(),
        Value::Map(map) => {
            let dict = PyDict::new(py);
            for (key, value) in map {
                dict.set_item(key, to_native(py, value)?)?;
            }
            dict.into_any().unbind()
        }
    })
}

/// A DASL value: null, bool, integer, bytes, string, list, map or link.
///
/// Map keys are strings. `Value(value)` takes None, bool, int, str, bytes,
/// `Cid`, `Value`, and lists, tuples and dicts with str keys of them. It
/// checks the same rules as `decode`:
///
/// - Links must be DASL CIDs: raw or DRISL, with a SHA-256 or BLAKE3 digest.
/// - Integers outside i64, floats and nesting deeper than 126 raise
///   ValueError.
///
/// Other types raise TypeError. Values compare and hash by value.
///
/// >>> from covalence import Value
/// >>> value = Value({'a': [1, b'x']})
/// >>> Value.decode(value.encode()) == value
/// True
/// >>> value.to_json()
/// '{"a":[1,{"$bytes":"eA"}]}'
#[pyclass(
    skip_from_py_object,
    frozen,
    eq,
    hash,
    name = "Value",
    module = "covalence"
)]
#[pyo3(crate = "covalence_lib_python::pyo3")]
#[derive(Clone, PartialEq, Eq, Hash)]
struct PyValue {
    inner: Value,
}

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PyValue {
    #[new]
    fn new(value: &Bound<'_, PyAny>) -> PyResult<Self> {
        let mut builder = Builder::default();
        feed(&mut builder, value)?;
        Ok(Self {
            inner: builder.finish().map_err(raise)?,
        })
    }

    /// The kind of value, as a string.
    ///
    /// One of "null", "bool", "integer", "bytes", "string", "list", "map" or
    /// "link".
    ///
    /// >>> from covalence import Value
    /// >>> Value([1, 'a']).kind
    /// 'list'
    #[getter]
    fn kind(&self) -> &'static str {
        match self.inner {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Integer(_) => "integer",
            Value::Bytes(_) => "bytes",
            Value::String(_) => "string",
            Value::List(_) => "list",
            Value::Map(_) => "map",
            Value::Link(_) => "link",
        }
    }

    /// The value as None, bool, int, str, bytes, `Cid`, lists and dicts.
    fn to_native(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        to_native(py, &self.inner)
    }

    /// The DRISL encoding.
    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let bytes = py.detach(|| self.inner.encode()).map_err(raise)?;
        Ok(PyBytes::new(py, &bytes))
    }

    /// Decode exactly one DRISL value.
    ///
    /// Invalid DRISL raises ValueError.
    #[staticmethod]
    fn decode(py: Python<'_>, data: PyBackedBytes) -> PyResult<Self> {
        Ok(Self {
            inner: py.detach(|| Value::decode(&data)).map_err(raise)?,
        })
    }

    /// The value as ATProto JSON.
    ///
    /// Links become `{"$link": "<CID>"}` and bytes `{"$bytes": "<base64>"}`.
    fn to_json(&self, py: Python<'_>) -> PyResult<String> {
        py.detach(|| self.inner.to_json()).map_err(raise)
    }

    /// Parse ATProto JSON, as `to_json` writes it.
    ///
    /// Invalid JSON raises ValueError.
    ///
    /// >>> from covalence import Value
    /// >>> Value.from_json('{"a": {"$bytes": "eA"}}') == Value({'a': b'x'})
    /// True
    #[staticmethod]
    fn from_json(py: Python<'_>, text: &str) -> PyResult<Self> {
        Ok(Self {
            inner: py.detach(|| Value::from_json(text)).map_err(raise)?,
        })
    }

    /// The CID of the DRISL encoding, hashed with BLAKE3.
    ///
    /// This is the CID `Cas.put_value` returns.
    fn cid(&self, py: Python<'_>) -> PyResult<PyCid> {
        Ok(py.detach(|| self.inner.cid()).map_err(raise)?.into())
    }

    /// The CIDs the value links to, in encoding order, with repeats.
    fn links(&self) -> Vec<PyCid> {
        self.inner.links().map(PyCid::from).collect()
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(format!("Value({})", self.to_native(py)?.bind(py).repr()?))
    }
}

/// A store of byte values under byte keys.
///
/// Build one with `memory` or `s3`. A failure of the storage underneath
/// raises StoreError.
///
/// >>> from covalence import Kv
/// >>> kv = Kv.memory()
/// >>> kv.put(b'key', b'value')
/// >>> kv.get(b'key'), kv.get(b'other'), kv.exists(b'key')
/// (b'value', None, True)
#[pyclass(frozen, name = "Kv", module = "covalence")]
#[pyo3(crate = "covalence_lib_python::pyo3")]
struct PyKv {
    inner: Arc<dyn Kv>,
}

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PyKv {
    /// A new, empty store in memory.
    #[staticmethod]
    fn memory() -> Self {
        Self {
            inner: MemoryKv::default().into_dyn(),
        }
    }

    /// A store in S3, configured as the AWS SDKs are.
    ///
    /// `location` is an `s3://bucket/prefix/` URL, or a bucket name with
    /// `prefix`. `region` and `profile` override the SDK configuration.
    /// `concurrency` is how many 8 MiB parts of one value move at once (16 by
    /// default; 0 counts as 1), which bounds the memory a streamed value
    /// holds.
    ///
    /// - An invalid location, a URL with `prefix` too, or no region raises
    ///   ValueError.
    /// - Keys must be UTF-8 with non-empty `/`-separated segments other than
    ///   `.` and `..`; other keys raise StoreError.
    /// - Credentials are fetched at the first request; missing or failed
    ///   credentials raise StoreError then.
    /// - Without `s3:ListBucket`, S3 answers 403 for a missing key, so reading
    ///   one raises StoreError instead of returning None or False.
    #[staticmethod]
    #[pyo3(signature = (location, prefix = None, *, region = None, profile = None, concurrency = None))]
    fn s3(
        py: Python<'_>,
        location: &str,
        prefix: Option<&str>,
        region: Option<String>,
        profile: Option<String>,
        concurrency: Option<usize>,
    ) -> PyResult<Self> {
        let location = match (location.strip_prefix("s3://"), prefix) {
            (None, prefix) => S3Location::new(location, prefix.unwrap_or_default()),
            (Some(_), None) => location.parse(),
            (Some(_), Some(_)) => {
                return Err(fail(
                    "pass the prefix in the s3:// URL or with a bucket name",
                ));
            }
        }
        .map_err(raise)?;
        let options = AwsOptions { profile, region };
        let kv = py
            .detach(|| S3Kv::from_aws(location, &options))
            .map_err(raise)?;
        let kv = match concurrency {
            Some(parts) => kv.concurrency(parts),
            None => kv,
        };
        Ok(Self {
            inner: kv.into_dyn(),
        })
    }

    /// The value under `key`, or None if there is none.
    fn get<'py>(
        &self,
        py: Python<'py>,
        key: PyBackedBytes,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        let value = py.detach(|| self.inner.get(&key)).map_err(raise)?;
        Ok(value.map(|value| PyBytes::new(py, &value)))
    }

    /// Store `value` under `key`, replacing any value there.
    fn put(&self, py: Python<'_>, key: PyBackedBytes, value: PyBackedBytes) -> PyResult<()> {
        let value = Bytes::from_owner(value);
        py.detach(|| self.inner.put(&key, value)).map_err(raise)
    }

    /// Whether there is a value under `key`, without reading it.
    fn exists(&self, py: Python<'_>, key: PyBackedBytes) -> PyResult<bool> {
        py.detach(|| self.inner.exists(&key)).map_err(raise)
    }
}

/// Bytes to snapshot, or a blob whose source to share.
#[derive(FromPyObject)]
#[pyo3(crate = "covalence_lib_python::pyo3")]
enum BlobArg<'py> {
    Blob(PyRef<'py, PyBlob>),
    Bytes(PyBackedBytes),
}

/// Bytes read whole or by range, not checked except by `fact`.
///
/// - `Blob(data)` holds a snapshot of `data` in memory.
/// - `Blob(blob)` shares `blob`'s source.
/// - Copies keep their class and share the source.
///
/// Its methods are not virtual: a Python subclass's overrides change what
/// Python callers see, but `fact` and `slice` read the Rust blob underneath.
///
/// >>> from covalence import Blob
/// >>> blob = Blob(b'hello world')
/// >>> blob.size(), blob.slice(6, 5).read_all()
/// (11, b'world')
#[pyclass(subclass, frozen, name = "Blob", module = "covalence")]
#[pyo3(crate = "covalence_lib_python::pyo3")]
struct PyBlob {
    inner: Arc<dyn Blob>,
}

impl PyBlob {
    /// A blob of `blob`'s class over the same source, with its attributes
    /// copied, or deep copied with `memo`.
    fn copy<'py>(
        blob: &Bound<'py, Self>,
        memo: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = blob.py();
        let class = blob.get_type();
        let copy = class.call_method1(intern!(py, "__new__"), (&class, blob))?;
        if let Ok(state) = blob.getattr(intern!(py, "__dict__")) {
            let state = match memo {
                Some(memo) => {
                    memo.set_item(blob.as_ptr() as usize, &copy)?;
                    py.import("copy")?.call_method1("deepcopy", (state, memo))?
                }
                None => state,
            };
            copy.getattr(intern!(py, "__dict__"))?
                .call_method1("update", (state,))?;
        }
        Ok(copy)
    }
}

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PyBlob {
    #[new]
    fn new(data: BlobArg<'_>) -> Self {
        let inner = match data {
            BlobArg::Blob(blob) => blob.inner.clone(),
            BlobArg::Bytes(data) => Arc::new(Bytes::from_owner(data)),
        };
        Self { inner }
    }

    /// The size in bytes, as the blob reports it.
    fn size(&self, py: Python<'_>) -> PyResult<u64> {
        py.detach(|| self.inner.size()).map_err(raise)
    }

    /// Up to `length` bytes from `start`; fewer at the end, and none past it.
    ///
    /// >>> from covalence import Blob
    /// >>> blob = Blob(b'hello')
    /// >>> blob.read_range(1, 3)
    /// b'ell'
    /// >>> blob.read_range(4, 10)
    /// b'o'
    /// >>> blob.read_range(5, 1)
    /// b''
    fn read_range<'py>(
        &self,
        py: Python<'py>,
        start: u64,
        length: u64,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let data = py
            .detach(|| self.inner.read_range(start, length))
            .map_err(raise)?;
        if data.len() as u64 > length {
            return Err(raise_as(
                Class::Store,
                "blob returned more bytes than asked",
            ));
        }
        Ok(PyBytes::new(py, &data))
    }

    /// All the bytes.
    fn read_all<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let data = py.detach(|| self.inner.read_all()).map_err(raise)?;
        Ok(PyBytes::new(py, &data))
    }

    /// Up to `length` bytes from `start`, as a blob over the same bytes.
    fn slice(&self, start: u64, length: u64) -> Self {
        Self {
            inner: Arc::new(Slice::new(self.inner.clone(), start, length)),
        }
    }

    /// A checked fact for exactly `cid`, or None if the bytes do not hash to it.
    ///
    /// - A CID with a digest over 32 bytes, such as SHA-512's, raises
    ///   ValueError.
    /// - Covalence checks SHA-1, SHA-256 and BLAKE3. Another hash, or a
    ///   digest length the hash does not give, raises UnsupportedError.
    ///
    /// >>> from covalence import Blob, Cid
    /// >>> blob = Blob(b'hello')
    /// >>> fact = blob.fact(Cid.of(b'hello', 'sha256'))
    /// >>> fact.cid == Cid.of(b'hello', 'sha256'), fact.data
    /// (True, b'hello')
    /// >>> fact.fact(Cid.of(b'hello', 'sha256', 'drisl')).cid.codec.name
    /// 'dag-cbor'
    /// >>> blob.fact(Cid.of(b'other')) is None
    /// True
    fn fact(&self, py: Python<'_>, cid: PyRef<'_, PyCid>) -> PyResult<Option<Py<PyBlobFact>>> {
        let cid = cid.key()?;
        let fact = py.detach(|| self.inner.fact(&cid)).map_err(raise)?;
        fact.map(|fact| PyBlobFact::new(py, fact)).transpose()
    }

    /// A blob of the same class over the same source.
    fn __copy__<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        Self::copy(slf, None)
    }

    /// A blob of the same class over the same source: blobs are read-only.
    fn __deepcopy__<'py>(
        slf: &Bound<'py, Self>,
        memo: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        Self::copy(slf, Some(memo))
    }
}

/// Bytes and a CID they hash to: a `Fact`, and a blob of those bytes.
///
/// Facts are equal when their CIDs and bytes are, and hash by CID. This
/// class cannot be subclassed.
///
/// >>> from covalence import BlobFact, Cid
/// >>> fact = BlobFact.of(b'hello')
/// >>> fact.cid == Cid.of(b'hello'), fact.data, fact.value()
/// (True, b'hello', Value(b'hello'))
#[pyclass(frozen, eq, extends = PyBlob, name = "BlobFact", module = "covalence")]
#[pyo3(crate = "covalence_lib_python::pyo3")]
#[derive(PartialEq)]
struct PyBlobFact {
    inner: Fact,
}

impl PyBlobFact {
    /// `inner` as a Python fact, which is a blob of its bytes.
    fn new(py: Python<'_>, inner: Fact) -> PyResult<Py<Self>> {
        Self::over(py, Arc::new(inner.clone()), inner)
    }

    /// `inner` as a Python fact, read through `source`.
    fn over(py: Python<'_>, source: Arc<dyn Blob>, inner: Fact) -> PyResult<Py<Self>> {
        let blob = PyBlob { inner: source };
        Py::new(
            py,
            PyClassInitializer::from(blob).add_subclass(Self { inner }),
        )
    }
}

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PyBlobFact {
    /// The fact of `data` and its raw BLAKE3 CID.
    #[staticmethod]
    fn of(py: Python<'_>, data: PyBackedBytes) -> PyResult<Py<Self>> {
        let data = Bytes::from_owner(data);
        let fact = py.detach(|| Fact::<Blake3>::of(data));
        Self::new(py, fact.widen())
    }

    /// The CID the bytes hash to.
    #[getter]
    fn cid(&self) -> PyCid {
        PyCid {
            inner: (*self.inner.claim()).into(),
        }
    }

    /// The bytes.
    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, self.inner.bytes())
    }

    /// The bytes decoded as the CID's codec says.
    ///
    /// - DRISL bytes decode to their value; invalid DRISL raises ValueError.
    /// - Raw bytes become a bytes `Value`.
    /// - Another codec raises UnsupportedError.
    fn value(&self, py: Python<'_>) -> PyResult<PyValue> {
        let inner = py.detach(|| Value::from_fact(&self.inner)).map_err(raise)?;
        Ok(PyValue { inner })
    }

    /// Equal facts have equal CIDs, so facts hash by CID.
    fn __hash__(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        Hash::hash(self.inner.claim(), &mut hasher);
        hasher.finish()
    }

    /// A fact over the same source.
    fn __copy__(slf: &Bound<'_, Self>) -> PyResult<Py<Self>> {
        let source = slf.as_super().get().inner.clone();
        Self::over(slf.py(), source, slf.get().inner.clone())
    }

    /// A fact over the same source: facts are read-only.
    fn __deepcopy__(slf: &Bound<'_, Self>, _memo: &Bound<'_, PyAny>) -> PyResult<Py<Self>> {
        Self::__copy__(slf)
    }

    fn __repr__(&self) -> String {
        let len = self.inner.bytes().len();
        format!("BlobFact(cid={}, len={len})", self.cid().__repr__())
    }
}

/// A content-addressed store, which finds bytes by their CIDs.
///
/// Build one with `memory`, `sqlite` or `kv`. `put` returns the raw BLAKE3
/// CID of the bytes, and a BLAKE3 CID with another codec finds the same
/// bytes.
///
/// - A CID with a digest over 32 bytes raises ValueError.
/// - A failed check raises IntegrityError.
/// - A failure of the storage underneath raises StoreError.
///
/// >>> from covalence import Cas, Cid
/// >>> cas = Cas.memory()
/// >>> cid = cas.put(b'hello')
/// >>> cid == Cid.of(b'hello'), cas.get(cid)
/// (True, b'hello')
/// >>> cas.missing([cid, Cid.of(b'other')]) == [Cid.of(b'other')]
/// True
#[pyclass(frozen, name = "Cas", module = "covalence")]
#[pyo3(crate = "covalence_lib_python::pyo3")]
struct PyCas {
    inner: Arc<dyn Cas>,
}

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PyCas {
    /// A new, empty store in memory.
    #[staticmethod]
    fn memory() -> Self {
        Self {
            inner: MemoryCas::default().into_dyn(),
        }
    }

    /// A store in the SQLite database at `path`, or in memory without one.
    #[staticmethod]
    #[pyo3(signature = (path = None))]
    fn sqlite(py: Python<'_>, path: Option<PathBuf>) -> PyResult<Self> {
        let store = py
            .detach(|| match path {
                Some(path) => SqliteCas::open(path),
                None => SqliteCas::open_in_memory(),
            })
            .map_err(raise)?;
        Ok(Self {
            inner: store.into_dyn(),
        })
    }

    /// A store of blocks in `kv`, each under `prefix/blake3/<digest>/bytes`.
    ///
    /// - Without a prefix, the key is `blake3/<digest>/bytes`.
    /// - The digest is the BLAKE3 digest in lowercase unpadded base32.
    /// - A trailing `/` on `prefix` is optional; an invalid prefix raises
    ///   ValueError.
    #[staticmethod]
    #[pyo3(signature = (kv, prefix = None))]
    fn kv(kv: PyRef<'_, PyKv>, prefix: Option<&str>) -> PyResult<Self> {
        let kv = kv.inner.clone();
        let store = match prefix {
            Some(prefix) => KvCas::with_prefix(kv, prefix).map_err(fail)?,
            None => KvCas::new(kv),
        };
        Ok(Self {
            inner: store.into_dyn(),
        })
    }

    /// Store `data` and return its raw BLAKE3 CID.
    ///
    /// Checks that the store keeps the data under that CID.
    fn put(&self, py: Python<'_>, data: PyBackedBytes) -> PyResult<PyCid> {
        let key = self.store(py, Bytes::from_owner(data))?;
        Ok(PyCid { inner: key.into() })
    }

    /// Store the DRISL encoding of `value` and return its DRISL CID.
    ///
    /// The CID is hashed with BLAKE3, so it is `Value(value).cid()`.
    fn put_value(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<PyCid> {
        let value = PyValue::new(value)?;
        let data = py.detach(|| value.inner.encode()).map_err(raise)?;
        let key = self.store(py, data.into())?;
        PyCid::new_cid(Multicodec::DAG_CBOR, key.hash(), key.digest())
    }

    /// Store the file at `path` and return its raw BLAKE3 CID.
    ///
    /// Checks that the store keeps the file under that CID.
    ///
    /// - Reads the file twice, to hash it and to store it, and stores only
    ///   the bytes it hashed. If they changed in between it raises
    ///   IntegrityError, and if the file got shorter OSError, storing nothing.
    /// - `Cas.kv` over `Kv.s3` streams the file; other stores read it into
    ///   memory.
    fn put_file(&self, py: Python<'_>, path: PathBuf) -> PyResult<PyCid> {
        let (mut file, claim, len) = py.detach(|| hash_file(&path))?;
        let key = py
            .detach(|| self.inner.put_from(claim, &mut file, len))
            .map_err(raise)?;
        check_key(key, claim)?;
        Ok(PyCid { inner: key.into() })
    }

    /// Store `length` bytes from the iterable `chunks`, which must hash to `cid`, and return its raw BLAKE3 CID.
    ///
    /// For bytes that arrive in pieces, such as a request body, without
    /// holding them all at once.
    ///
    /// - `cid` must be a BLAKE3 CID of any codec; another hash raises
    ///   ValueError.
    /// - `chunks` yields bytes or bytearray. It is read in order, and once
    ///   more after `length` bytes, to check that it ends there.
    /// - Bytes that do not hash to `cid` raise IntegrityError; `chunks`
    ///   holding fewer or more than `length` bytes raises ValueError; an
    ///   exception from `chunks` propagates as it is. Each stores nothing.
    ///   If cleanup also fails, raises StoreError with the source exception
    ///   as its `__cause__` and the cleanup failure in its message.
    /// - `Cas.kv` over `Kv.s3` streams the bytes up in parts, holding about as
    ///   many parts as upload at once; other stores hold them in memory.
    fn put_from(
        &self,
        py: Python<'_>,
        cid: PyRef<'_, PyCid>,
        chunks: &Bound<'_, PyAny>,
        length: u64,
    ) -> PyResult<PyCid> {
        let claim = cid
            .key()?
            .raw_blake3()
            .ok_or_else(|| fail(format!("{} is not a BLAKE3 CID", cid.inner)))?;
        let mut reader = Chunks {
            iter: chunks.try_iter()?.unbind(),
            chunk: None,
            offset: 0,
            left: length,
            error: None,
        };
        // No read asks past the last byte, so check that nothing follows it here.
        if length == 0 {
            reader.end()?;
        }
        let result = py.detach(|| self.inner.put_from(claim, &mut reader, length));
        if let Some(source) = reader.error.take() {
            return match result {
                Err(error @ covalence_cas_trait::Error::Backend { .. }) => {
                    let error = raise(error);
                    error.set_cause(py, Some(source));
                    Err(error)
                }
                _ => Err(source),
            };
        }
        let key = result.map_err(raise)?;
        check_key(key, claim)?;
        Ok(PyCid { inner: key.into() })
    }

    /// Write the bytes under `cid` to the file at `path` and return their length.
    ///
    /// Returns None if missing.
    ///
    /// - Checks the bytes against `cid` in a temporary file beside `path`,
    ///   which replaces `path` only once the check passes. A missing CID or a
    ///   failure leaves `path` as it was.
    /// - `Cas.kv` over `Kv.s3` streams the bytes; other stores hold them in
    ///   memory.
    fn get_file(
        &self,
        py: Python<'_>,
        cid: PyRef<'_, PyCid>,
        path: PathBuf,
    ) -> PyResult<Option<u64>> {
        let cid = cid.key()?;
        let (mut temp, permissions) = py.detach(|| temp_beside(&path))?;
        let len = py
            .detach(|| self.inner.get_to(&cid, temp.as_file_mut()))
            .map_err(raise)?;
        if len.is_some() {
            py.detach(|| {
                temp.as_file().set_permissions(permissions)?;
                temp.persist(&path).map_err(|error| error.error)
            })?;
        }
        Ok(len)
    }

    /// The bytes under `cid`, checked against it, or None if missing.
    fn get<'py>(
        &self,
        py: Python<'py>,
        cid: PyRef<'_, PyCid>,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        let cid = cid.key()?;
        let fact = py.detach(|| self.inner.get_fact(&cid)).map_err(raise)?;
        Ok(fact.map(|fact| PyBytes::new(py, fact.bytes())))
    }

    /// Whether the store holds bytes for `cid`, without checking their integrity.
    ///
    /// - `Cas.sqlite` checks its index, without reading the bytes.
    /// - `Cas.kv` asks its `Kv` whether the key exists (a HEAD on S3).
    /// - `Cas.memory` looks the bytes up in memory.
    fn has(&self, py: Python<'_>, cid: PyRef<'_, PyCid>) -> PyResult<bool> {
        let cid = cid.key()?;
        py.detach(|| self.inner.has(&cid)).map_err(raise)
    }

    /// Those of `cids` the store lacks, in order, as `has` finds them.
    fn missing(&self, py: Python<'_>, cids: Vec<PyRef<'_, PyCid>>) -> PyResult<Vec<PyCid>> {
        let keys = cids
            .iter()
            .map(|cid| cid.key())
            .collect::<PyResult<Vec<_>>>()?;
        let missing = py.detach(|| self.inner.missing(&keys)).map_err(raise)?;
        Ok(missing
            .into_iter()
            .map(|key| PyCid { inner: key.into() })
            .collect())
    }

    /// The size in bytes of the value under `cid`, as the store vouches for it.
    ///
    /// None if missing. No store checks sizes yet.
    fn size(&self, py: Python<'_>, cid: PyRef<'_, PyCid>) -> PyResult<Option<u64>> {
        let cid = cid.key()?;
        py.detach(|| self.inner.size(&cid)).map_err(raise)
    }

    /// The unchecked size in bytes of the value under `cid`, or None if missing.
    fn size_unchecked(&self, py: Python<'_>, cid: PyRef<'_, PyCid>) -> PyResult<Option<u64>> {
        let cid = cid.key()?;
        py.detach(|| self.inner.size_unchecked(&cid)).map_err(raise)
    }

    /// Up to `length` bytes from `start`, as the store vouches for them.
    ///
    /// None if missing. No store checks ranges yet.
    ///
    /// >>> from covalence import Cas
    /// >>> cas = Cas.memory()
    /// >>> cid = cas.put(b'hello')
    /// >>> cas.size(cid)
    /// 5
    /// >>> cas.get_range(cid, 1, 3)
    /// b'ell'
    /// >>> cas.get_range(cid, 4, 10)
    /// b'o'
    /// >>> cas.get_range(cid, 5, 1)
    /// b''
    fn get_range<'py>(
        &self,
        py: Python<'py>,
        cid: PyRef<'_, PyCid>,
        start: u64,
        length: u64,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        let cid = cid.key()?;
        let data = py
            .detach(|| self.inner.get_range(&cid, start, length))
            .map_err(raise)?;
        Ok(data.map(|data| PyBytes::new(py, &data)))
    }

    /// Up to `length` bytes from `start`, unchecked, or None if missing.
    fn get_range_unchecked<'py>(
        &self,
        py: Python<'py>,
        cid: PyRef<'_, PyCid>,
        start: u64,
        length: u64,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        let cid = cid.key()?;
        let data = py
            .detach(|| self.inner.get_range_unchecked(&cid, start, length))
            .map_err(raise)?;
        Ok(data.map(|data| PyBytes::new(py, &data)))
    }

    /// The bytes under `cid` as a checked `BlobFact` for `cid`, or None if missing.
    fn get_fact(&self, py: Python<'_>, cid: PyRef<'_, PyCid>) -> PyResult<Option<Py<PyBlobFact>>> {
        let cid = cid.key()?;
        let fact = py.detach(|| self.inner.get_fact(&cid)).map_err(raise)?;
        fact.map(|fact| PyBlobFact::new(py, fact)).transpose()
    }

    /// What the store says about itself, for debugging: None or a dict.
    fn info(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        to_native(py, &py.detach(|| self.inner.info()).map_err(raise)?)
    }

    /// Whatever the store counts, for debugging: None or a dict.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        to_native(py, &py.detach(|| self.inner.stats()).map_err(raise)?)
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(format!("Cas({})", self.info(py)?.bind(py).repr()?))
    }
}

impl PyCas {
    /// Store `data`, checking that the store keeps it under its BLAKE3 hash.
    fn store(&self, py: Python<'_>, data: Bytes) -> PyResult<Cid> {
        let claim = py.detach(|| *Fact::<Blake3>::of(data.clone()).claim());
        let key = py.detach(|| self.inner.put(data)).map_err(raise)?;
        check_key(key, claim)?;
        Ok(key)
    }
}

/// The bytes of a Python iterable of chunks, read as exactly `left` more
/// bytes. The read handing over the last of them first checks that the
/// iterable ends there, so a store reading it never sees a last byte that
/// more bytes follow. On failure it keeps the Python exception in `error`.
struct Chunks {
    iter: Py<PyIterator>,
    chunk: Option<PyBackedBytes>,
    offset: usize,
    left: u64,
    error: Option<PyErr>,
}

impl Chunks {
    /// The next chunk, skipping empty ones, or None at the end.
    fn next(&self) -> PyResult<Option<PyBackedBytes>> {
        Python::attach(|py| {
            let iter = self.iter.bind(py);
            for item in iter.clone() {
                let chunk: PyBackedBytes = item?.extract()?;
                if !chunk.is_empty() {
                    return Ok(Some(chunk));
                }
            }
            Ok(None)
        })
    }

    /// ValueError unless the iterable has no more bytes.
    fn end(&self) -> PyResult<()> {
        match self.next()? {
            None => Ok(()),
            Some(_) => Err(PyValueError::new_err("chunks hold more bytes than length")),
        }
    }

    /// The next chunk, counted against `left`: ValueError if the iterable
    /// ends early or holds too many bytes.
    fn take(&mut self) -> PyResult<PyBackedBytes> {
        let Some(chunk) = self.next()? else {
            return Err(PyValueError::new_err("chunks hold fewer bytes than length"));
        };
        self.left = self
            .left
            .checked_sub(chunk.len() as u64)
            .ok_or_else(|| PyValueError::new_err("chunks hold more bytes than length"))?;
        if self.left == 0 {
            self.end()?;
        }
        Ok(chunk)
    }
}

impl io::Read for Chunks {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let chunk = match self.chunk.take() {
            Some(chunk) if self.offset < chunk.len() => chunk,
            _ if self.left == 0 => return Ok(0),
            _ => {
                self.offset = 0;
                self.take().map_err(|error| {
                    let message = error.to_string();
                    self.error = Some(error);
                    io::Error::other(message)
                })?
            }
        };
        let read = buf.len().min(chunk.len() - self.offset);
        buf[..read].copy_from_slice(&chunk[self.offset..self.offset + read]);
        self.offset += read;
        self.chunk = Some(chunk);
        Ok(read)
    }
}

/// `IntegrityError` unless the store kept the data under `claim`.
fn check_key(key: Cid, claim: Blake3) -> PyResult<()> {
    if key.same_multihash(&claim.into()) {
        return Ok(());
    }
    let message = format!("the store kept the data under {key}, not {claim}");
    Err(raise_as(Class::Integrity, message))
}

/// The file at `path`, rewound after hashing it, its raw BLAKE3 CID and its
/// length.
fn hash_file(path: &Path) -> io::Result<(File, Blake3, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let len = io::copy(&mut BufReader::with_capacity(1 << 20, &file), &mut hasher)?;
    file.rewind()?;
    let digest = hasher.finalize();
    let cid = Blake3::new(Format::BLAKE3, digest.as_bytes()).expect("a BLAKE3 digest");
    Ok((file, cid, len))
}

/// A temporary file in the directory of `path` that only its owner can read,
/// and the permissions to give it before it replaces `path`: those of the
/// file at `path` if there is one, else those the umask gives a new file.
fn temp_beside(path: &Path) -> io::Result<(tempfile::NamedTempFile, Permissions)> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let permissions = match std::fs::metadata(path) {
        Ok(existing) => existing.permissions(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => new_file_permissions(dir)?,
        Err(error) => return Err(error),
    };
    let temp = tempfile::Builder::new()
        .prefix(".covalence-")
        .tempfile_in(dir)?;
    Ok((temp, permissions))
}

/// The permissions the umask gives a new file in `dir`, read from an empty
/// probe file that is removed at once.
fn new_file_permissions(dir: &Path) -> io::Result<Permissions> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(".covalence-");
    #[cfg(unix)]
    builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o666));
    Ok(builder
        .tempfile_in(dir)?
        .as_file()
        .metadata()?
        .permissions())
}

#[pymodule]
#[pyo3(crate = "covalence_lib_python::pyo3", gil_used = true)]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // maturin writes the Cargo version in its PEP 440 form, parsed with
    // pep440_rs (0.1.0-dev.2 is 0.1.0.dev2), so match the installed metadata.
    let version: pep440_rs::Version = env!("CARGO_PKG_VERSION").parse().map_err(fail)?;
    m.add("__version__", version.to_string())?;
    m.add_class::<PyCodec>()?;
    m.add_class::<PyHash>()?;
    m.add_class::<PyCid>()?;
    m.add_class::<PyValue>()?;
    m.add_class::<PyKv>()?;
    m.add_class::<PyCas>()?;
    m.add_class::<PyBlob>()?;
    m.add_class::<PyBlobFact>()?;
    errors::add(m)
}

#[cfg(test)]
mod tests {
    use covalence_cas_trait::{AsyncGet, AsyncPut, CasFuture, Error as CasError, Get, Put, ready};

    use super::*;

    /// A store that keeps everything under the CID of no bytes.
    struct Rekeying;

    impl Get for Rekeying {
        fn get_unchecked(&self, _: &Cid) -> Result<Option<Bytes>, CasError> {
            Ok(None)
        }
    }

    impl AsyncGet for Rekeying {
        fn aget_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
            ready(self.get_unchecked(key))
        }
    }

    impl Put for Rekeying {
        fn put(&self, _: Bytes) -> Result<Cid, CasError> {
            Ok((*Fact::<Blake3>::of(Bytes::from_static(b"")).claim()).into())
        }
    }

    impl AsyncPut for Rekeying {
        fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
            ready(self.put(bytes))
        }
    }

    impl Cas for Rekeying {}

    /// Downloads are private from the start, whatever the file will become.
    #[cfg(unix)]
    #[test]
    fn temporary_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let (temp, new) = temp_beside(&target).unwrap();
        assert_eq!(
            temp.as_file().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(new.mode() & 0o600, 0o600);
        drop(temp);
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, PermissionsExt::from_mode(0o640)).unwrap();
        let (temp, existing) = temp_beside(&target).unwrap();
        assert_eq!(
            temp.as_file().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(existing.mode() & 0o777, 0o640);
        drop(temp);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// A blob whose `read_all` answers differently from its ranges.
    struct Overriding;

    impl Blob for Overriding {
        fn size(&self) -> Result<u64, covalence_blob_trait::Error> {
            Ok(5)
        }
        fn read_range(&self, _: u64, _: u64) -> Result<Bytes, covalence_blob_trait::Error> {
            Ok(Bytes::from_static(b"range"))
        }
        fn read_all(&self) -> Result<Bytes, covalence_blob_trait::Error> {
            Ok(Bytes::from_static(b"all"))
        }
    }

    #[test]
    fn copies_share_the_source() {
        Python::initialize();
        Python::attach(|py| {
            let source: Arc<dyn Blob> = Arc::new(Overriding);
            let blob = Bound::new(
                py,
                PyBlob {
                    inner: source.clone(),
                },
            )
            .unwrap();
            let copies = [
                PyBlob::__copy__(&blob).unwrap(),
                PyBlob::__deepcopy__(&blob, PyDict::new(py).as_any()).unwrap(),
            ];
            for copy in copies {
                let copy = copy.cast_into::<PyBlob>().unwrap();
                assert!(Arc::ptr_eq(&copy.get().inner, &source));
                assert_eq!(copy.get().read_all(py).unwrap().as_bytes(), b"all");
            }
            let fact = PyBlobFact::of(py, PyBytes::new(py, b"x").extract().unwrap()).unwrap();
            let fact = fact.bind(py);
            let source = &fact.as_super().get().inner;
            let copies = [
                PyBlobFact::__copy__(fact).unwrap(),
                PyBlobFact::__deepcopy__(fact, PyDict::new(py).as_any()).unwrap(),
            ];
            for copy in copies {
                assert!(Arc::ptr_eq(&copy.bind(py).as_super().get().inner, source));
            }
        });
    }

    #[test]
    fn read_range_rejects_more_bytes_than_asked() {
        Python::initialize();
        Python::attach(|py| {
            let blob = PyBlob {
                inner: Arc::new(Overriding),
            };
            assert_eq!(blob.read_range(py, 0, 5).unwrap().as_bytes(), b"range");
            let error = blob.read_range(py, 0, 4).unwrap_err();
            let store = errors::store_error(py).unwrap();
            assert!(error.value(py).is_instance(&store).unwrap());
        });
    }

    #[test]
    fn put_checks_the_key_the_store_returns() {
        Python::initialize();
        Python::attach(|py| {
            let data = |data: &[u8]| PyBytes::new(py, data).extract::<PyBackedBytes>().unwrap();
            let cas = PyCas {
                inner: Rekeying.into_dyn(),
            };
            assert!(cas.put(py, data(b"")).is_ok());
            let error = cas.put(py, data(b"data")).err().unwrap();
            let integrity = errors::integrity_error(py).unwrap();
            assert!(error.value(py).is_instance(&integrity).unwrap());
        });
    }
}

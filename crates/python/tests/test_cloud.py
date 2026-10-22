"""Cloud SDK contracts, with stand-ins that require no network or credentials."""

import io
from pathlib import Path
import subprocess
import sys
import tempfile
from types import ModuleType
import unittest
from unittest import mock

from covalence import Kv, StoreError
from covalence._cloud import GcsBackend, S3Backend


class ClientError(Exception):
    def __init__(self, code):
        self.response = {"Error": {"Code": code}}
        super().__init__(code)


class S3Client:
    def __init__(self):
        self.objects = {}
        self.transfers = []
        self.error = None

    def _object(self, bucket, key):
        if self.error is not None:
            raise ClientError(self.error)
        try:
            return self.objects[bucket, key]
        except KeyError:
            raise ClientError("NoSuchKey") from None

    def put_object(self, *, Bucket, Key, Body, StorageClass):
        self.objects[Bucket, Key] = (Body, StorageClass)

    def get_object(self, *, Bucket, Key):
        return {"Body": io.BytesIO(self._object(Bucket, Key)[0])}

    def head_object(self, *, Bucket, Key):
        data, storage_class = self._object(Bucket, Key)
        return {"ContentLength": len(data), "StorageClass": storage_class}

    def upload_file(self, filename, bucket, key, *, ExtraArgs):
        self.transfers.append(("upload", filename, bucket, key))
        with open(filename, "rb") as stream:
            self.objects[bucket, key] = (stream.read(), ExtraArgs["StorageClass"])

    def download_file(self, bucket, key, filename):
        self.transfers.append(("download", filename, bucket, key))
        data, _ = self._object(bucket, key)
        # boto3 downloads beside the destination and then renames onto it.
        partial = Path(filename).with_name(Path(filename).name + ".partial")
        with partial.open("wb") as stream:
            stream.write(data)
        partial.replace(filename)


class NotFound(Exception):
    pass


class GcsBlob:
    def __init__(self, bucket, name):
        self.bucket, self.name = bucket, name
        self.storage_class = None
        self.size = None

    def _data(self):
        if self.bucket.error is not None:
            raise self.bucket.error
        if self.name not in self.bucket.objects:
            raise NotFound(self.name)
        return self.bucket.objects[self.name][0]

    def upload_from_string(self, data):
        self.bucket.objects[self.name] = (data, self.storage_class)

    def download_as_bytes(self):
        return self._data()

    def upload_from_filename(self, filename):
        self.bucket.transfers.append(("upload", filename, self.name))
        with open(filename, "rb") as stream:
            self.upload_from_string(stream.read())

    def download_to_filename(self, filename):
        self.bucket.transfers.append(("download", filename, self.name))
        data = self._data()
        with open(filename, "wb") as stream:
            stream.write(data)

    def exists(self):
        if self.bucket.error is not None:
            raise self.bucket.error
        return self.name in self.bucket.objects

    def reload(self):
        self.size = len(self._data())


class GcsBucket:
    def __init__(self, name):
        self.name = name
        self.objects = {}
        self.transfers = []
        self.error = None

    def blob(self, name):
        return GcsBlob(self, name)


def module(name, **attributes):
    result = ModuleType(name)
    vars(result).update(attributes)
    return result


class CloudTests(unittest.TestCase):
    def setUp(self):
        self.s3 = S3Client()
        self.gcs_buckets = {}
        self.gcs_clients = []
        self.boto = mock.Mock()
        self.boto.client.return_value = self.s3
        self.boto.Session.return_value.client.return_value = self.s3
        buckets, clients = self.gcs_buckets, self.gcs_clients

        class GcsClient:
            def __init__(self, project=None):
                clients.append(("adc", None, project))

            @classmethod
            def from_service_account_json(cls, path, project=None):
                instance = object.__new__(cls)
                clients.append(("file", path, project))
                return instance

            def bucket(self, name):
                return buckets.setdefault(name, GcsBucket(name))

        storage = module("google.cloud.storage", Client=GcsClient)
        self.modules = mock.patch.dict(sys.modules, {
            "boto3": self.boto,
            "botocore": module("botocore"),
            "botocore.exceptions": module("botocore.exceptions", ClientError=ClientError),
            "google": module("google"),
            "google.api_core": module("google.api_core"),
            "google.api_core.exceptions": module("google.api_core.exceptions", NotFound=NotFound),
            "google.cloud": module("google.cloud", storage=storage),
            "google.cloud.storage": storage,
        })
        self.modules.start()
        self.addCleanup(self.modules.stop)

    def test_s3_storage_class_threshold_includes_the_boundary_and_file_transfers(self):
        backend = S3Backend("s3://bucket/prefix/", storage_class="GLACIER_IR", standard_below=4, client=self.s3)
        self.assertEqual((backend.bucket, backend.prefix, backend.url), ("bucket", "prefix/", "s3://bucket/prefix"))
        backend.put(b"small", b"123")
        backend.put(b"boundary", b"1234")
        self.assertEqual(self.s3.objects["bucket", "prefix/small"], (b"123", "STANDARD"))
        self.assertEqual(self.s3.objects["bucket", "prefix/boundary"], (b"1234", "GLACIER_IR"))
        self.assertEqual(backend.get(b"boundary"), b"1234")
        with tempfile.TemporaryDirectory() as directory:
            source, target = Path(directory) / "source", Path(directory) / "target"
            source.write_bytes(b"file")
            backend.put_file(b"file", source)
            self.assertEqual(backend.get_file(b"file", target), 4)
            self.assertEqual(target.read_bytes(), b"file")
        self.assertEqual([call[0] for call in self.s3.transfers], ["upload", "download"])
        self.assertEqual(self.s3.objects["bucket", "prefix/file"][1], "GLACIER_IR")

    def test_sdk_factories_preserve_opaque_prefixes_instead_of_normalizing_object_names(self):
        for prefix in ("legacy//prefix", "legacy/./prefix", "legacy/../prefix"):
            for location, argument in ((f"s3://bucket/{prefix}/", None), ("bucket", f"/{prefix}/")):
                with self.subTest(provider="s3", location=location, prefix=argument):
                    kv = Kv.s3(location, argument, storage_class="STANDARD")
                    kv.put(b"key", b"payload")
                    self.assertEqual(kv.get(b"key"), b"payload")
                    self.assertEqual(self.s3.objects["bucket", f"{prefix}/key"], (b"payload", "STANDARD"))
            with self.subTest(provider="gcs", prefix=prefix):
                kv = Kv.gcs("bucket", f"/{prefix}/")
                kv.put(b"key", b"payload")
                self.assertEqual(kv.get(b"key"), b"payload")
                self.assertEqual(self.gcs_buckets["bucket"].objects[f"{prefix}/key"], (b"payload", "STANDARD"))

    def test_s3_missing_and_denied_are_distinct_and_empty_is_present(self):
        backend = S3Backend("bucket", client=self.s3)
        backend.put(b"empty", b"")
        self.assertTrue(backend.exists(b"empty"))
        self.assertEqual(backend.size(b"empty"), 0)
        self.assertEqual(backend.get(b"empty"), b"")
        self.assertIsNone(backend.get(b"missing"))
        self.assertIsNone(backend.size(b"missing"))
        self.assertFalse(backend.exists(b"missing"))
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "target"
            self.assertIsNone(backend.get_file(b"missing", target))
            self.assertFalse(target.exists())
            self.s3.error = "AccessDenied"
            for operation, args in ((backend.get, (b"key",)), (backend.exists, (b"key",)),
                                    (backend.get_file, (b"key", target))):
                with self.subTest(operation=operation.__name__), self.assertRaises(ClientError):
                    operation(*args)

    def test_s3_defaults_to_boto_credentials_and_honors_region_and_profile(self):
        S3Backend("bucket")
        self.boto.client.assert_called_once_with("s3")
        S3Backend("bucket", region="us-west-2", profile="work")
        self.boto.Session.assert_called_once_with(profile_name="work")
        self.boto.Session.return_value.client.assert_called_once_with("s3", region_name="us-west-2")

    def test_gcs_storage_class_prefix_adc_and_file_transfers(self):
        backend = GcsBackend("bucket", "/prefix/", storage_class="COLDLINE", project="billing")
        self.assertEqual((backend.prefix, backend.url), ("prefix/", "gs://bucket/prefix"))
        self.assertEqual(self.gcs_clients, [("adc", None, "billing")])
        backend.put(b"bytes", b"payload")
        bucket = self.gcs_buckets["bucket"]
        self.assertEqual(bucket.objects["prefix/bytes"], (b"payload", "COLDLINE"))
        self.assertEqual(backend.get(b"bytes"), b"payload")
        self.assertTrue(backend.exists(b"bytes"))
        self.assertEqual(backend.size(b"bytes"), 7)
        with tempfile.TemporaryDirectory() as directory:
            source, target = Path(directory) / "source", Path(directory) / "target"
            source.write_bytes(b"file")
            backend.put_file(b"file", source)
            self.assertEqual(backend.get_file(b"file", target), 4)
            self.assertEqual(target.read_bytes(), b"file")
        self.assertEqual([call[0] for call in bucket.transfers], ["upload", "download"])
        self.assertEqual(bucket.objects["prefix/file"][1], "COLDLINE")

    def test_gcs_key_file_replaces_adc_and_missing_is_distinct_from_denied(self):
        backend = GcsBackend("bucket", project="billing", credentials_file=Path("service-account.json"))
        self.assertEqual(self.gcs_clients, [("file", "service-account.json", "billing")])
        self.assertEqual(backend.url, "gs://bucket")
        backend.put(b"empty", b"")
        self.assertTrue(backend.exists(b"empty"))
        self.assertEqual(backend.size(b"empty"), 0)
        self.assertIsNone(backend.get(b"missing"))
        self.assertIsNone(backend.size(b"missing"))
        self.assertFalse(backend.exists(b"missing"))
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "target"
            self.assertIsNone(backend.get_file(b"missing", target))
            self.gcs_buckets["bucket"].error = PermissionError("denied")
            for operation, args in ((backend.get, (b"key",)), (backend.exists, (b"key",)),
                                    (backend.get_file, (b"key", target))):
                with self.subTest(operation=operation.__name__), self.assertRaises(PermissionError):
                    operation(*args)

    def test_factory_file_transfers_stream_through_the_sdk(self):
        for kv in (Kv.s3("s3://bucket/prefix", storage_class="GLACIER_IR", standard_below=4, client=self.s3),
                   Kv.gcs("bucket", "prefix", storage_class="ARCHIVE")):
            with self.subTest(kv=kv), tempfile.TemporaryDirectory() as directory:
                source, target = Path(directory) / "source", Path(directory) / "target"
                source.write_bytes(b"file payload")
                kv.put_file(b"key", source)
                self.assertTrue(kv.exists(b"key"))
                self.assertEqual(kv.get_file(b"key", target), 12)
                self.assertEqual(target.read_bytes(), b"file payload")
                self.assertIsNone(kv.get_file(b"missing", target))
                self.assertEqual(target.read_bytes(), b"file payload")
        self.assertEqual([call[0] for call in self.s3.transfers], ["upload", "download", "download"])
        self.assertEqual([call[0] for call in self.gcs_buckets["bucket"].transfers], ["upload", "download", "download"])

    def test_native_tuning_cannot_be_silently_ignored_by_sdk_backend(self):
        for concurrency in (0, 1, 16):
            with self.subTest(concurrency=concurrency), self.assertRaises(ValueError):
                Kv.s3("bucket", client=self.s3, concurrency=concurrency)
        for limit in (0, 1, 1024):
            with self.subTest(upload_buffer_bytes=limit), self.assertRaises(ValueError):
                Kv.s3("bucket", client=self.s3, upload_buffer_bytes=limit)
        self.assertFalse(self.s3.transfers)

    def test_cloud_errors_cross_the_native_boundary_as_store_errors(self):
        kv = Kv.s3("bucket", client=self.s3)
        self.s3.error = "AccessDenied"
        with self.assertRaises(StoreError):
            kv.get(b"key")
        kv = Kv.gcs("bucket")
        self.gcs_buckets["bucket"].error = PermissionError("denied")
        with self.assertRaises(StoreError):
            kv.exists(b"key")

    def test_invalid_locations_keys_classes_and_thresholds(self):
        for operation in (
            lambda: S3Backend("gs://bucket"),
            lambda: S3Backend("s3://bucket/prefix", "another"),
            lambda: S3Backend("bucket", standard_below=-1),
            lambda: GcsBackend("gs://bucket"),
            lambda: GcsBackend("bucket", storage_class="GLACIER_IR"),
        ):
            with self.assertRaises(ValueError):
                operation()
        for backend in (S3Backend("bucket", client=self.s3), GcsBackend("bucket")):
            for key in (b"", b"a//b", b"a/../b", b"/a", b"a/", b"\xff"):
                with self.subTest(backend=type(backend).__name__, key=key), self.assertRaises(ValueError):
                    backend.get(key)

    def test_missing_extras_explain_how_to_install_them(self):
        with mock.patch.dict(sys.modules, {"botocore.exceptions": None}):
            with self.assertRaisesRegex(ImportError, r"covalence\[s3-python\]"):
                S3Backend("bucket")
        with mock.patch.dict(sys.modules, {"google.cloud.storage": None}):
            with self.assertRaisesRegex(ImportError, r"covalence\[gcs\]"):
                GcsBackend("bucket")

    def test_importing_covalence_does_not_import_cloud_sdks(self):
        result = subprocess.run(
            [sys.executable, "-I", "-c", "import covalence, sys; "
             "assert not any(name in sys.modules for name in ('boto3', 'botocore', 'google.cloud.storage'))"],
            capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()

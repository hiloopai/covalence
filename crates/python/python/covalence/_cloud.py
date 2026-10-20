"""Optional Python SDK backends for Covalence key-value stores.

SDK imports happen when a backend is made, so importing Covalence does not
require either cloud extra or load either provider's credentials.
"""

from contextlib import closing
from importlib import import_module
from pathlib import Path
from typing import Any


def _object_key(prefix: str, key: bytes) -> str:
    """An object name, rejecting keys that native S3 stores cannot hold."""
    name = key.decode("utf-8")
    if any(part in ("", ".", "..") for part in name.split("/")):
        raise ValueError("object keys must have non-empty segments other than '.' and '..'")
    return prefix + name


def _prefix(prefix: str) -> str:
    """A prefix with one trailing slash, or an empty string."""
    prefix = prefix.strip("/")
    if prefix and any(part in ("", ".", "..") for part in prefix.split("/")):
        raise ValueError("object prefixes must have non-empty segments other than '.' and '..'")
    return f"{prefix}/" if prefix else ""


def _bucket(bucket: str) -> str:
    """A bucket name without a scheme or path."""
    if not bucket or "/" in bucket or bucket in (".", "..") or ":" in bucket:
        raise ValueError(f"invalid bucket name: {bucket!r}")
    return bucket


class S3Backend:
    """Objects in S3, with streaming transfers and a size-based storage class."""

    def __init__(
        self,
        bucket_or_url: str,
        prefix: str | None = None,
        *,
        region: str | None = None,
        profile: str | None = None,
        storage_class: str = "STANDARD",
        standard_below: int = 0,
        client: Any = None,
    ) -> None:
        """Configure a bucket, importing boto3 only when no client is supplied."""
        if bucket_or_url.startswith("s3://"):
            if prefix is not None:
                raise ValueError("an S3 URL cannot also have a prefix argument")
            bucket, _, prefix = bucket_or_url[5:].partition("/")
        else:
            bucket = bucket_or_url
        self.bucket = _bucket(bucket)
        self.prefix = _prefix(prefix or "")
        self.url = f"s3://{self.bucket}/{self.prefix}".rstrip("/")
        if standard_below < 0:
            raise ValueError("standard_below must be non-negative")
        self.storage_class = storage_class
        self.standard_below = standard_below
        try:
            ClientError = import_module("botocore.exceptions").ClientError

            if client is None:
                boto3 = import_module("boto3")

                session = boto3.Session(profile_name=profile) if profile is not None else boto3
                client = session.client("s3", region_name=region) if region is not None else session.client("s3")
        except ImportError as error:
            raise ImportError("S3 Python transfers need the s3-python extra: pip install 'covalence[s3-python]'") from error
        self.client = client
        self._client_error = ClientError

    def _missing(self, error: Any) -> bool:
        """Whether an S3 error reports a missing object rather than access denial."""
        return error.response.get("Error", {}).get("Code") in ("404", "NoSuchKey", "NotFound")

    def storage_class_of(self, size: int) -> str:
        """The class used for an object of this many stored bytes."""
        return "STANDARD" if size < self.standard_below else self.storage_class

    def get(self, key: bytes) -> bytes | None:
        """Read an object, returning None when S3 reports it missing."""
        try:
            response = self.client.get_object(Bucket=self.bucket, Key=_object_key(self.prefix, key))
        except self._client_error as error:
            if self._missing(error):
                return None
            raise
        with closing(response["Body"]) as body:
            return body.read()

    def put(self, key: bytes, value: bytes) -> None:
        """Replace an object, choosing its class from its stored size."""
        self.client.put_object(
            Bucket=self.bucket,
            Key=_object_key(self.prefix, key),
            Body=value,
            StorageClass=self.storage_class_of(len(value)),
        )

    def exists(self, key: bytes) -> bool:
        """Check for an object with HEAD, without downloading its bytes."""
        return self.size(key) is not None

    def size(self, key: bytes) -> int | None:
        """Read an object's stored size with HEAD, or None if missing."""
        try:
            response = self.client.head_object(Bucket=self.bucket, Key=_object_key(self.prefix, key))
        except self._client_error as error:
            if self._missing(error):
                return None
            raise
        size = response["ContentLength"]
        if not isinstance(size, int) or size < 0:
            raise ValueError("S3 response has no valid ContentLength")
        return size

    def put_file(self, key: bytes, path: Path) -> None:
        """Stream a file through boto3's transfer manager."""
        self.client.upload_file(
            str(path),
            self.bucket,
            _object_key(self.prefix, key),
            ExtraArgs={"StorageClass": self.storage_class_of(Path(path).stat().st_size)},
        )

    def get_file(self, key: bytes, path: Path) -> int | None:
        """Stream an object to a file, returning its size or None if missing."""
        try:
            self.client.download_file(self.bucket, _object_key(self.prefix, key), str(path))
        except self._client_error as error:
            if self._missing(error):
                return None
            raise
        return Path(path).stat().st_size

    def info(self) -> dict[str, str]:
        """The backend name and normalized object location."""
        return {"name": "s3", "url": self.url}


class GcsBackend:
    """Objects in Google Cloud Storage, transferred through the Python SDK."""

    def __init__(
        self,
        bucket: str,
        prefix: str = "",
        *,
        storage_class: str = "STANDARD",
        project: str | None = None,
        credentials_file: str | Path | None = None,
    ) -> None:
        """Use Application Default Credentials, or a service-account key file."""
        bucket = _bucket(bucket)
        self.prefix = _prefix(prefix)
        self.url = f"gs://{bucket}/{self.prefix}".rstrip("/")
        if storage_class not in ("STANDARD", "NEARLINE", "COLDLINE", "ARCHIVE"):
            raise ValueError("GCS storage_class must be STANDARD, NEARLINE, COLDLINE or ARCHIVE")
        self.storage_class = storage_class
        try:
            NotFound = import_module("google.api_core.exceptions").NotFound
            storage = import_module("google.cloud.storage")
        except ImportError as error:
            raise ImportError("GCS needs the gcs extra: pip install 'covalence[gcs]'") from error
        client = (
            storage.Client(project=project)
            if credentials_file is None
            else storage.Client.from_service_account_json(str(credentials_file), project=project)
        )
        self.bucket = client.bucket(bucket)
        self._not_found = NotFound

    def get(self, key: bytes) -> bytes | None:
        """Read an object, returning None when GCS reports it missing."""
        try:
            return self.bucket.blob(_object_key(self.prefix, key)).download_as_bytes()
        except self._not_found:
            return None

    def put(self, key: bytes, value: bytes) -> None:
        """Replace an object in the configured storage class."""
        blob = self.bucket.blob(_object_key(self.prefix, key))
        blob.storage_class = self.storage_class
        blob.upload_from_string(value)

    def exists(self, key: bytes) -> bool:
        """Check for an object without downloading its bytes."""
        return self.bucket.blob(_object_key(self.prefix, key)).exists()

    def size(self, key: bytes) -> int | None:
        """Read an object's stored size from its metadata, or None if missing."""
        blob = self.bucket.blob(_object_key(self.prefix, key))
        try:
            blob.reload()
        except self._not_found:
            return None
        if not isinstance(blob.size, int) or blob.size < 0:
            raise ValueError("GCS response has no valid size")
        return blob.size

    def put_file(self, key: bytes, path: Path) -> None:
        """Stream a file with a resumable upload when the SDK requires one."""
        blob = self.bucket.blob(_object_key(self.prefix, key))
        blob.storage_class = self.storage_class
        blob.upload_from_filename(str(path))

    def get_file(self, key: bytes, path: Path) -> int | None:
        """Stream an object to a file, returning its size or None if missing."""
        try:
            self.bucket.blob(_object_key(self.prefix, key)).download_to_filename(str(path))
        except self._not_found:
            return None
        return Path(path).stat().st_size

    def info(self) -> dict[str, str]:
        """The backend name and normalized object location."""
        return {"name": "gcs", "url": self.url}

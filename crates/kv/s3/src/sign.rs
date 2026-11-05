//! Payload-bound signatures for multipart part requests.

use std::time::SystemTime;

use aws_sigv4::{
    http_request::{
        self, PayloadChecksumKind, PercentEncodingMode, SignableBody, SignableRequest,
        SigningInstructions, SigningSettings, UriPathNormalizationMode,
    },
    sign::v4,
};
use covalence_lib_error::snafu::ResultExt;

use crate::{Credentials, Error, S3Kv, SigningParametersSnafu, SigningSnafu};

impl S3Kv {
    pub(crate) fn payload_signature(
        &self,
        url: &url::Url,
        hash: &str,
        credentials: &Credentials,
    ) -> Result<SigningInstructions, Error> {
        let identity = aws_credential_types::Credentials::new(
            credentials.key(),
            credentials.secret(),
            credentials.token().map(str::to_owned),
            None,
            "S3Kv",
        )
        .into();
        // S3 signs the escaped object path exactly once and preserves its segments.
        let mut settings = SigningSettings::default();
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        settings.percent_encoding_mode = PercentEncodingMode::Single;
        settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
        let parameters = v4::SigningParams::builder()
            .identity(&identity)
            .region(self.bucket.region())
            .name("s3")
            .time(SystemTime::now())
            .settings(settings)
            .build()
            .context(SigningParametersSnafu)?
            .into();
        let request = SignableRequest::new(
            "PUT",
            url.as_str(),
            std::iter::empty(),
            SignableBody::Precomputed(hash.to_owned()),
        )
        .context(SigningSnafu)?;
        let (instructions, _) = http_request::sign(request, &parameters)
            .context(SigningSnafu)?
            .into_parts();
        Ok(instructions)
    }
}

// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Time-Stamp Protocol (TSP) / RFC 3161 client.

use {
    crate::asn1::{
        rfc3161::{
            MessageImprint, PkiStatus, TimeStampReq, TimeStampResp, TstInfo,
            OID_CONTENT_TYPE_TST_INFO,
        },
        rfc5652::{SignedData, OID_ID_SIGNED_DATA},
    },
    bcder::{
        decode::{Constructed, DecodeError, IntoSource, Source},
        encode::Values,
        Integer, OctetString,
    },
    ring::rand::SecureRandom,
    std::{convert::Infallible, ops::Deref},
    x509_certificate::DigestAlgorithm,
};

#[cfg(feature = "isahc")]
use isahc::{http, ReadResponseExt, RequestExt};

#[cfg(feature = "reqwest")]
use reqwest::IntoUrl;

#[cfg(feature = "ureq")]
use ureq::http;

pub const HTTP_CONTENT_TYPE_REQUEST: &str = "application/timestamp-query";

pub const HTTP_CONTENT_TYPE_RESPONSE: &str = "application/timestamp-reply";

#[derive(Debug)]
pub enum TimeStampError {
    Io(std::io::Error),
    #[cfg(feature = "isahc")]
    Isahc(isahc::Error),
    #[cfg(feature = "reqwest")]
    Reqwest(reqwest::Error),
    #[cfg(feature = "ureq")]
    Ureq(ureq::Error),
    Asn1Decode(DecodeError<Infallible>),
    Http(String),
    Random,
    NonceMismatch,
    Unsuccessful(TimeStampResp),
    BadResponse,
}

impl std::fmt::Display for TimeStampError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => f.write_fmt(format_args!("I/O error: {}", e)),
            #[cfg(feature = "isahc")]
            Self::Isahc(e) => f.write_fmt(format_args!("HTTP client error: {}", e)),
            #[cfg(feature = "reqwest")]
            Self::Reqwest(e) => f.write_fmt(format_args!("HTTP error: {}", e)),
            #[cfg(feature = "ureq")]
            Self::Ureq(e) => f.write_fmt(format_args!("HTTP client error: {}", e)),
            Self::Asn1Decode(e) => f.write_fmt(format_args!("ASN.1 decode error: {}", e)),
            Self::Http(msg) => f.write_str(msg),
            Self::Random => f.write_str("error generating random nonce"),
            Self::NonceMismatch => f.write_str("nonce mismatch"),
            Self::Unsuccessful(r) => f.write_fmt(format_args!(
                "unsuccessful Time-Stamp Protocol response: {:?}: {:?}",
                r.status.status, r.status.status_string
            )),
            Self::BadResponse => f.write_str("bad server response"),
        }
    }
}

impl std::error::Error for TimeStampError {}

impl From<std::io::Error> for TimeStampError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(feature = "isahc")]
impl From<isahc::Error> for TimeStampError {
    fn from(e: isahc::Error) -> Self {
        Self::Isahc(e)
    }
}

#[cfg(feature = "isahc")]
impl From<http::Error> for TimeStampError {
    fn from(e: http::Error) -> Self {
        Self::Http(e.to_string())
    }
}

#[cfg(feature = "reqwest")]
impl From<reqwest::Error> for TimeStampError {
    fn from(e: reqwest::Error) -> Self {
        Self::Reqwest(e)
    }
}

#[cfg(feature = "ureq")]
impl From<ureq::Error> for TimeStampError {
    fn from(e: ureq::Error) -> Self {
        Self::Ureq(e)
    }
}

#[cfg(feature = "ureq")]
impl From<http::Error> for TimeStampError {
    fn from(e: http::Error) -> Self {
        Self::Http(e.to_string())
    }
}

impl From<DecodeError<Infallible>> for TimeStampError {
    fn from(e: DecodeError<Infallible>) -> Self {
        Self::Asn1Decode(e)
    }
}

/// High-level interface to [TimeStampResp].
///
/// This type provides a high-level interface to the low-level ASN.1 response
/// type from a Time-Stamp Protocol request.
pub struct TimeStampResponse(TimeStampResp);

impl Deref for TimeStampResponse {
    type Target = TimeStampResp;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TimeStampResponse {
    /// Whether the time stamp request was successful.
    pub fn is_success(&self) -> bool {
        matches!(
            self.0.status.status,
            PkiStatus::Granted | PkiStatus::GrantedWithMods
        )
    }

    /// Obtain the size of the time-stamp token data.
    pub fn token_content_size(&self) -> Option<usize> {
        self.0
            .time_stamp_token
            .as_ref()
            .map(|token| token.content.len())
    }

    /// Decode the `SignedData` value in the response.
    pub fn signed_data(&self) -> Result<Option<SignedData>, DecodeError<Infallible>> {
        if let Some(token) = &self.0.time_stamp_token {
            let source = token.content.clone();

            if token.content_type == OID_ID_SIGNED_DATA {
                Ok(Some(source.decode(SignedData::take_from)?))
            } else {
                Err(source
                    .into_source()
                    .content_err("invalid OID on signed data"))
            }
        } else {
            Ok(None)
        }
    }

    pub fn tst_info(&self) -> Result<Option<TstInfo>, DecodeError<Infallible>> {
        if let Some(signed_data) = self.signed_data()? {
            if signed_data.content_info.content_type == OID_CONTENT_TYPE_TST_INFO {
                if let Some(content) = signed_data.content_info.content {
                    Ok(Some(Constructed::decode(
                        content.to_bytes(),
                        bcder::Mode::Der,
                        TstInfo::take_from,
                    )?))
                } else {
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }
}

impl From<TimeStampResp> for TimeStampResponse {
    fn from(resp: TimeStampResp) -> Self {
        Self(resp)
    }
}

/// Send a Time-Stamp request for a given message to an HTTP URL.
///
/// This is a wrapper around [time_stamp_request_http] that constructs the low-level
/// ASN.1 request object with reasonable defaults.
#[cfg(feature = "isahc")]
pub fn time_stamp_message_http<T>(
    url: T,
    message: &[u8],
    digest_algorithm: DigestAlgorithm,
) -> Result<TimeStampResponse, TimeStampError>
where
    http::Uri: TryFrom<T>,
    <http::Uri as TryFrom<T>>::Error: Into<http::Error>,
{
    let request = create_request(message, digest_algorithm)?;
    time_stamp_request_http(url, &request)
}

/// Send a Time-Stamp request for a given message to an HTTP URL.
///
/// This is a wrapper around [time_stamp_request_http] that constructs the low-level
/// ASN.1 request object with reasonable defaults.
#[cfg(feature = "reqwest")]
pub fn time_stamp_message_http(
    url: impl IntoUrl,
    message: &[u8],
    digest_algorithm: DigestAlgorithm,
) -> Result<TimeStampResponse, TimeStampError> {
    let request = create_request(message, digest_algorithm)?;
    time_stamp_request_http(url, &request)
}

/// Send a Time-Stamp request for a given message to an HTTP URL.
///
/// This is a wrapper around [time_stamp_request_http] that constructs the low-level
/// ASN.1 request object with reasonable defaults.
#[cfg(feature = "ureq")]
pub fn time_stamp_message_http<T>(
    url: T,
    message: &[u8],
    digest_algorithm: DigestAlgorithm,
) -> Result<TimeStampResponse, TimeStampError>
where
    http::Uri: TryFrom<T>,
    <http::Uri as TryFrom<T>>::Error: Into<http::Error>,
{
    let request = create_request(message, digest_algorithm)?;
    time_stamp_request_http(url, &request)
}

/// Send a [TimeStampReq] to a server via HTTP.
#[cfg(feature = "isahc")]
pub fn time_stamp_request_http<T>(
    url: T,
    request: &TimeStampReq,
) -> Result<TimeStampResponse, TimeStampError>
where
    http::Uri: TryFrom<T>,
    <http::Uri as TryFrom<T>>::Error: Into<http::Error>,
{
    let mut body = Vec::<u8>::new();
    request
        .encode_ref()
        .write_encoded(bcder::Mode::Der, &mut body)?;

    let mut response = isahc::Request::post(url)
        .header("Content-Type", HTTP_CONTENT_TYPE_REQUEST)
        .body(body)?
        .send()?;

    if response.status().is_success()
        && response.headers().get("Content-Type")
            == Some(&http::header::HeaderValue::from_static(
                HTTP_CONTENT_TYPE_RESPONSE,
            ))
    {
        let response_bytes = response.bytes()?;

        let res = TimeStampResponse(Constructed::decode(
            response_bytes.as_ref(),
            bcder::Mode::Der,
            TimeStampResp::take_from,
        )?);

        // Verify nonce was reflected, if present.
        if res.is_success() {
            if let Some(tst_info) = res.tst_info()? {
                if tst_info.nonce != request.nonce {
                    return Err(TimeStampError::NonceMismatch);
                }
            }
        }

        Ok(res)
    } else {
        Err(TimeStampError::Http(String::from("bad HTTP response")))
    }
}

/// Send a [TimeStampReq] to a server via HTTP.
#[cfg(feature = "reqwest")]
pub fn time_stamp_request_http(
    url: impl IntoUrl,
    request: &TimeStampReq,
) -> Result<TimeStampResponse, TimeStampError> {
    let client = reqwest::blocking::Client::new();

    let mut body = Vec::<u8>::new();
    request
        .encode_ref()
        .write_encoded(bcder::Mode::Der, &mut body)?;

    let response = client
        .post(url)
        .header("Content-Type", HTTP_CONTENT_TYPE_REQUEST)
        .body(body)
        .send()?;

    if response.status().is_success()
        && response.headers().get("Content-Type")
            == Some(&reqwest::header::HeaderValue::from_static(
                HTTP_CONTENT_TYPE_RESPONSE,
            ))
    {
        let response_bytes = response.bytes()?;

        let res = TimeStampResponse(Constructed::decode(
            response_bytes.as_ref(),
            bcder::Mode::Der,
            TimeStampResp::take_from,
        )?);

        // Verify nonce was reflected, if present.
        if res.is_success() {
            if let Some(tst_info) = res.tst_info()? {
                if tst_info.nonce != request.nonce {
                    return Err(TimeStampError::NonceMismatch);
                }
            }
        }

        Ok(res)
    } else {
        Err(TimeStampError::Http(String::from("bad HTTP response")))
    }
}

/// Send a [TimeStampReq] to a server via HTTP.
#[cfg(feature = "ureq")]
pub fn time_stamp_request_http<T>(
    url: T,
    request: &TimeStampReq,
) -> Result<TimeStampResponse, TimeStampError>
where
    http::Uri: TryFrom<T>,
    <http::Uri as TryFrom<T>>::Error: Into<http::Error>,
{
    let mut body = Vec::<u8>::new();
    request
        .encode_ref()
        .write_encoded(bcder::Mode::Der, &mut body)?;

    let req = http::Request::post(url)
        .header("Content-Type", HTTP_CONTENT_TYPE_REQUEST)
        .body(body)?;
    let mut response = ureq::run(req)?;

    if response.status().is_success()
        && response.headers().get("Content-Type")
            == Some(&http::header::HeaderValue::from_static(
                HTTP_CONTENT_TYPE_RESPONSE,
            ))
    {
        let response_bytes = response.body_mut().read_to_vec()?;

        let res = TimeStampResponse(Constructed::decode(
            response_bytes.as_ref(),
            bcder::Mode::Der,
            TimeStampResp::take_from,
        )?);

        // Verify nonce was reflected, if present.
        if res.is_success() {
            if let Some(tst_info) = res.tst_info()? {
                if tst_info.nonce != request.nonce {
                    return Err(TimeStampError::NonceMismatch);
                }
            }
        }

        Ok(res)
    } else {
        Err(TimeStampError::Http(String::from("bad HTTP response")))
    }
}

fn create_request(
    message: &[u8],
    digest_algorithm: DigestAlgorithm,
) -> Result<TimeStampReq, TimeStampError> {
    let mut h = digest_algorithm.digester();
    h.update(message);
    let digest = h.finish();

    let mut random = [0u8; 8];
    ring::rand::SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| TimeStampError::Random)?;

    Ok(TimeStampReq {
        version: Integer::from(1),
        message_imprint: MessageImprint {
            hash_algorithm: digest_algorithm.into(),
            hashed_message: OctetString::new(bytes::Bytes::copy_from_slice(digest.as_ref())),
        },
        req_policy: None,
        nonce: Some(Integer::from(u64::from_le_bytes(random))),
        cert_req: Some(true),
        extensions: None,
    })
}

#[cfg(test)]
mod test {
    use super::*;

    const DIGICERT_TIMESTAMP_URL: &str = "http://timestamp.digicert.com";

    #[test]
    fn verify_static() {
        let signed_data =
            crate::SignedData::parse_ber(include_bytes!("testdata/tsp-signed-data.der")).unwrap();

        for signer in signed_data.signers() {
            signer
                .verify_message_digest_with_signed_data(&signed_data)
                .unwrap();
            signer
                .verify_signature_with_signed_data(&signed_data)
                .unwrap();
        }
    }

    #[test]
    fn simple_request() {
        let message = b"hello, world";

        let res = time_stamp_message_http(DIGICERT_TIMESTAMP_URL, message, DigestAlgorithm::Sha256)
            .unwrap();

        let signed_data = res.signed_data().unwrap().unwrap();
        assert_eq!(
            signed_data.content_info.content_type,
            OID_CONTENT_TYPE_TST_INFO
        );
        let tst_info = res.tst_info().unwrap().unwrap();
        assert_eq!(tst_info.version, Integer::from(1));

        let parsed = crate::SignedData::try_from(&signed_data).unwrap();
        for signer in parsed.signers() {
            signer
                .verify_message_digest_with_signed_data(&parsed)
                .unwrap();
            signer.verify_signature_with_signed_data(&parsed).unwrap();
        }
    }
}

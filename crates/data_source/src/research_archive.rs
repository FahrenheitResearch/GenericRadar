//! Public research-radar archives that are safe to browse in the application.
//!
//! There is no universal Level-I/time-series repository. Research radar data
//! is split among instrument owners and field-project archives, and several of
//! those require an account, a data order, or direct contact. This module starts
//! with the one source that is both public and machine-readable: NOAA/NSSL's
//! THREDDS catalog of KOUN RVP8/RVP900 time series.
//!
//! The catalog and file URL shapes are documented by the server itself:
//!
//! - catalog root: <https://data.nssl.noaa.gov/thredds/catalog/RRDD/KOUN/catalog.xml>
//! - file service: <https://data.nssl.noaa.gov/thredds/fileServer/RRDD/KOUN/...>
//!
//! Only paths returned below can be downloaded. Callers cannot supply a host
//! or URL, and every catalog reference and file path is revalidated before it
//! is joined to the fixed HTTPS origin. That is both a security boundary and a
//! provenance boundary: an "NSSL" button must not quietly fetch from somewhere
//! else after a malformed catalog entry or redirect.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::blocking::Client;
use serde::Deserialize;

use crate::{DataSourceError, Result, http_user_agent};

const NSSL_HOST: &str = "data.nssl.noaa.gov";
const NSSL_CATALOG_PREFIX: &str = "https://data.nssl.noaa.gov/thredds/catalog/RRDD/KOUN/";
const NSSL_FILE_PREFIX: &str = "https://data.nssl.noaa.gov/thredds/fileServer/";
const NSSL_FILE_PATH_PREFIX: &str = "RRDD/KOUN/";
const NSSL_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const NSSL_CATALOG_TIMEOUT: Duration = Duration::from_secs(12);
const NSSL_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DOWNLOAD_PROGRESS_STEP_BYTES: u64 = 512 * 1024;

/// Intermediate named by the NSSL leaf certificate's Authority Information
/// Access field, valid through 21 March 2036.
///
/// `data.nssl.noaa.gov` currently serves only its leaf certificate. Desktop
/// browsers recover the omitted Sectigo intermediate through AIA; rustls does
/// not perform network AIA fetching, so the otherwise-valid official endpoint
/// fails with `UnknownIssuer`. Supplying that public intermediate here repairs
/// the chain without disabling certificate or hostname verification. Source:
/// <http://crt.sectigo.com/SectigoPublicServerAuthenticationCADVR36.crt>.
const SECTIGO_PUBLIC_SERVER_DV_R36_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIGTDCCBDSgAwIBAgIQOXpmzCdWNi4NqofKbqvjsTANBgkqhkiG9w0BAQwFADBf
MQswCQYDVQQGEwJHQjEYMBYGA1UEChMPU2VjdGlnbyBMaW1pdGVkMTYwNAYDVQQD
Ey1TZWN0aWdvIFB1YmxpYyBTZXJ2ZXIgQXV0aGVudGljYXRpb24gUm9vdCBSNDYw
HhcNMjEwMzIyMDAwMDAwWhcNMzYwMzIxMjM1OTU5WjBgMQswCQYDVQQGEwJHQjEY
MBYGA1UEChMPU2VjdGlnbyBMaW1pdGVkMTcwNQYDVQQDEy5TZWN0aWdvIFB1Ymxp
YyBTZXJ2ZXIgQXV0aGVudGljYXRpb24gQ0EgRFYgUjM2MIIBojANBgkqhkiG9w0B
AQEFAAOCAY8AMIIBigKCAYEAljZf2HIz7+SPUPQCQObZYcrxLTHYdf1ZtMRe7Yeq
RPSwygz16qJ9cAWtWNTcuICc++p8Dct7zNGxCpqmEtqifO7NvuB5dEVexXn9RFF
H12Hm+NtPRQgXIFjx6MSJcNWuVO3XGE57L1mHlcQYj+g4hny90aFh2SCZCDEVkAj
aEMMfYPKuCjHuuF+bzHFb/9gV8P9+ekcHENF2nR1efGWSKwnfG5RawlkaQDpRtZT
mM64TIsv/r7cyFO4nSjs1jLdXYdz5q3a4L0NoabZfbdxVb+CUEHfB0bpulZQtH1
Rv38e/lIdP7OTTIlZh6OYL6NhxP8So0/sht/4J9mqIGxRFc0/pC8suja+wcIUna0
HBpXKfXTKpzgis+zmXDL06ASJf5E4A2/m+Hp6b84sfPAwQ766rI65mh50S0Di9E3
Pn2WcaJc+PILsBmYpgtmgWTR9eV9otfKRUBfzHUHcVgarub/XluEpRlTtZudU5xb
FNxx/DgMrXLUAPaI60fZ6wA+PTAgMBAAGjggGBMIIBfTAfBgNVHSMEGDAWgBRWc1
hklfmSGrASKgRieaFAFYghSTAdBgNVHQ4EFgQUaMASFhgOr872h6YyV6NGUV3LBy
cwDgYDVR0PAQH/BAQDAgGGMBIGA1UdEwEB/wQIMAYBAf8CAQAwHQYDVR0lBBYwFA
YIKwYBBQUHAwEGCCsGAQUFBwMCMBsGA1UdIAQUMBIwBgYEVR0gADAIBgZngQwBAg
EwVAYDVR0fBE0wSzBJoEegRYZDaHR0cDovL2NybC5zZWN0aWdvLmNvbS9TZWN0aW
dvUHVibGljU2VydmVyQXV0aGVudGljYXRpb25Sb290UjQ2LmNybDCBhAYIKwYBBQUH
AQEEeDB2ME8GCCsGAQUFBzAChkNodHRwOi8vY3J0LnNlY3RpZ28uY29tL1NlY3Rp
Z29QdWJsaWNTZXJ2ZXJBdXRoZW50aWNhdGlvblJvb3RSNDYucDdjMCMGCCsGAQUF
BzABhhdodHRwOi8vb2NzcC5zZWN0aWdvLmNvbTANBgkqhkiG9w0BAQwFAAOCAgEA
YtOC9Fy+TqECFw40IospI92kLGgoSZGPOSQXMBqmsGWZUQ7rux7cj1du6d9rD6C8
ze1B2eQjkrGkIL/OF1s7vSmgYVafsRoZd/IHUrkoQvX8FZwUsmPu7amgBfaY3g+d
q1x0jNGKb6I6Bzdl6LgMD9qxp+3i7GQOnd9J8LFSietY6Z4jUBzVoOoz8iAU84OF
h2HhAuiPw1ai0VnY38RTI+8kepGWVfGxfBWzwH9uIjeooIeaosVFvE8cmYUB4TSH
5dUyD0jHct2+8ceKEtIoFU/FfHq/mDaVnvcDCZXtIgitdMFQdMZaVehmObyhRdDD
4NQCs0gaI9AAgFj4L9QtkARzhQLNyRf87Kln+YU0lgCGr9HLg3rGO8q+Y4ppLsOd
unQZ6ZxPNGIfOApbPVf5hCe58EZwiWdHIMn9lPP6+F404y8NNugbQixBber+x536W
rZhFZLjEkhp7fFXf9r32rNPfb74X/U90Bdy4lzp3+X1ukh1BuMxA/EEhDoTOS3l7
ABvc7BYSQubQ2490OcdkIzUh3ZwDrakMVrbaTxUM2p24N6dB+ns2zptWCva6jzWr
8IWKIMxzxLPv5Kt3ePKcUdvkBU/smqujSczTzzSjIoR5QqQA6lN1ZRSnuHIWCvhJ
EltkYnTAH41QJ6SAWO66GrrUESwN/cgZzL4JLEqz1Y=
-----END CERTIFICATE-----"#;

/// A hard stop against an erroneous catalog entry or response filling a disk.
/// KOUN Level-I records observed in the public catalog are under 500 MB; 2 GiB
/// leaves ample room for unusually long records without making "no bound" the
/// policy.
pub const MAX_NSSL_LEVEL1_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The official browser page to show when somebody needs archive context,
/// license wording, or a citation beyond what fits in the in-app browser.
pub const NSSL_KOUN_CATALOG_PAGE: &str =
    "https://data.nssl.noaa.gov/thredds/catalog/customConfig/RRDD/KOUN.html";

/// The case used to pin this application's Level-I reader against real rain.
pub const KOUN_2013_05_20_IQ_CATALOG: &str = "2013/KOUN_20130520/IQ/catalog.xml";

/// An opaque, validated location beneath NOAA/NSSL's KOUN THREDDS root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NsslCatalogPath(String);

impl NsslCatalogPath {
    /// The whole KOUN archive: years first, then collection/date directories.
    #[must_use]
    pub fn root() -> Self {
        Self("catalog.xml".to_owned())
    }

    /// The 20 May 2013 Level-I catalog used by the real-data reader tests.
    #[must_use]
    pub fn verified_case() -> Self {
        Self(KOUN_2013_05_20_IQ_CATALOG.to_owned())
    }

    #[must_use]
    pub fn display(&self) -> &str {
        &self.0
    }

    fn url(&self) -> String {
        format!("{NSSL_CATALOG_PREFIX}{}", self.0)
    }
}

/// A directory in the official catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NsslCatalogDirectory {
    pub name: String,
    pub path: NsslCatalogPath,
}

/// One NSSL KOUN Level-I RVP time-series record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NsslLevel1File {
    pub name: String,
    /// Rounded size reported by THREDDS. The HTTP Content-Length is used for
    /// exact transfer verification; this value is for selection and display.
    pub estimated_size_bytes: Option<u64>,
    pub modified: Option<DateTime<Utc>>,
    url_path: String,
}

impl NsslLevel1File {
    /// A record opened end-to-end by this project's real-data tests.
    #[must_use]
    pub fn is_verified_sample(&self) -> bool {
        self.name == "KOUN_RVP.20130520.194555.936.Ascope_DEFAULT.0.H+V.300"
    }

    /// The 60.9 MB object is truncated in the public archive. It remains in
    /// the listing so the browser agrees with the source, but the UI can keep
    /// it from being mistaken for a usable sample.
    #[must_use]
    pub fn is_known_truncated(&self) -> bool {
        self.name == "KOUN_RVP.20130520.194113.663.Ascope_DEFAULT.0.H+V.300"
    }

    fn download_url(&self) -> Result<String> {
        validate_file_path(&self.url_path)?;
        Ok(format!("{NSSL_FILE_PREFIX}{}", self.url_path))
    }
}

/// One level of the KOUN THREDDS hierarchy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NsslCatalog {
    pub path: NsslCatalogPath,
    pub directories: Vec<NsslCatalogDirectory>,
    /// Only files whose official path identifies them as KOUN RVP I/Q are
    /// exposed. Scripts and processed products elsewhere in the tree are not
    /// Level-I data and must not leak into this picker merely because THREDDS
    /// can download them.
    pub files: Vec<NsslLevel1File>,
}

/// Download result for a user-owned Downloads directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadedResearchFile {
    pub path: PathBuf,
    pub bytes: u64,
    pub url: String,
}

/// Browse one official NOAA/NSSL catalog level.
pub fn list_nssl_koun_catalog(path: &NsslCatalogPath) -> Result<NsslCatalog> {
    validate_catalog_path(path.display())?;
    let text = catalog_client()
        .get(path.url())
        .send()?
        .error_for_status()?
        .text()?;
    parse_catalog(path.clone(), &text)
}

/// Download one selected Level-I record without overwriting an existing file.
///
/// The response streams into a sibling `.download` file and is renamed only
/// after the body reaches EOF and matches HTTP Content-Length. `progress` is
/// called at most twice per MiB plus once on completion; `cancelled` is checked
/// on every 64 KiB read.
pub fn download_nssl_level1_file(
    file: &NsslLevel1File,
    downloads_dir: &Path,
    progress: &dyn Fn(u64, Option<u64>),
    cancelled: &dyn Fn() -> bool,
) -> Result<DownloadedResearchFile> {
    let url = file.download_url()?;
    fs::create_dir_all(downloads_dir)?;
    let (destination, partial) = unused_destination(downloads_dir, &file.name)?;

    let outcome = (|| {
        let mut response = download_client().get(&url).send()?.error_for_status()?;
        let expected = response.content_length();
        if expected.is_some_and(|bytes| bytes > MAX_NSSL_LEVEL1_DOWNLOAD_BYTES) {
            return Err(DataSourceError::ResearchDownloadTooLarge {
                name: file.name.clone(),
                maximum_bytes: MAX_NSSL_LEVEL1_DOWNLOAD_BYTES,
            });
        }

        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?;
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut copied = 0_u64;
        let mut next_progress = 0_u64;
        progress(0, expected.or(file.estimated_size_bytes));

        loop {
            if cancelled() {
                return Err(DataSourceError::ResearchDownloadCancelled {
                    name: file.name.clone(),
                });
            }
            let read = match response.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read])?;
            copied = copied.saturating_add(read as u64);
            if copied > MAX_NSSL_LEVEL1_DOWNLOAD_BYTES {
                return Err(DataSourceError::ResearchDownloadTooLarge {
                    name: file.name.clone(),
                    maximum_bytes: MAX_NSSL_LEVEL1_DOWNLOAD_BYTES,
                });
            }
            if copied >= next_progress {
                progress(copied, expected.or(file.estimated_size_bytes));
                next_progress = copied.saturating_add(DOWNLOAD_PROGRESS_STEP_BYTES);
            }
        }
        output.flush()?;
        drop(output);

        if let Some(expected) = expected
            && copied != expected
        {
            return Err(DataSourceError::DownloadSizeMismatch {
                url: url.clone(),
                expected,
                actual: copied,
            });
        }
        fs::rename(&partial, &destination)?;
        progress(copied, Some(copied));
        Ok(DownloadedResearchFile {
            path: destination,
            bytes: copied,
            url,
        })
    })();

    if outcome.is_err() {
        let _ = fs::remove_file(&partial);
    }
    outcome
}

/// The ordinary per-user Downloads directory on desktop platforms.
///
/// A shell without a home/profile must choose explicitly rather than silently
/// writing a research file into the application's disposable cache.
#[must_use]
pub fn default_downloads_directory() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from).map(|path| path.join("Downloads"))
}

fn catalog_client() -> Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| build_client(NSSL_CATALOG_TIMEOUT))
        .clone()
}

fn download_client() -> Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| build_client(NSSL_DOWNLOAD_TIMEOUT))
        .clone()
}

fn build_client(timeout: Duration) -> Client {
    let nssl_intermediate =
        reqwest::Certificate::from_pem(SECTIGO_PUBLIC_SERVER_DV_R36_PEM.as_bytes())
            .expect("bundled NSSL intermediate should be a PEM certificate");
    Client::builder()
        .user_agent(http_user_agent())
        .connect_timeout(NSSL_CONNECT_TIMEOUT)
        .timeout(timeout)
        .add_root_certificate(nssl_intermediate)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let url = attempt.url();
            if attempt.previous().len() < 5
                && url.scheme() == "https"
                && url.host_str() == Some(NSSL_HOST)
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .expect("NSSL HTTP client should be constructible")
}

fn unused_destination(directory: &Path, name: &str) -> Result<(PathBuf, PathBuf)> {
    validate_file_name(name)?;
    for suffix in 0..10_000_u32 {
        let candidate_name = if suffix == 0 {
            name.to_owned()
        } else {
            format!("{name}.{suffix}")
        };
        let destination = directory.join(&candidate_name);
        let partial = directory.join(format!("{candidate_name}.download"));
        if !destination.exists() && !partial.exists() {
            return Ok((destination, partial));
        }
    }
    Err(DataSourceError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no unused Downloads filename remains for {name}"),
    )))
}

fn validate_file_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
    {
        return Err(DataSourceError::UnsafeResearchArchivePath(name.to_owned()));
    }
    Ok(())
}

fn validate_file_path(path: &str) -> Result<()> {
    let Some(name) = path.rsplit('/').next() else {
        return Err(DataSourceError::UnsafeResearchArchivePath(path.to_owned()));
    };
    if !path.starts_with(NSSL_FILE_PATH_PREFIX)
        || path.contains("//")
        || path.contains("\\")
        || path.contains('%')
        || path.contains('?')
        || path.contains('#')
        || path.split('/').any(|segment| {
            segment.is_empty()
                || segment == "."
                || segment == ".."
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
        })
    {
        return Err(DataSourceError::UnsafeResearchArchivePath(path.to_owned()));
    }
    validate_file_name(name)
}

fn validate_catalog_path(path: &str) -> Result<()> {
    if !path.ends_with("catalog.xml")
        || path.starts_with('/')
        || path.contains("//")
        || path.contains("\\")
        || path.contains('?')
        || path.contains('#')
        || path.split('/').any(|segment| {
            segment.is_empty()
                || segment == "."
                || segment == ".."
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
        })
    {
        return Err(DataSourceError::UnsafeResearchArchivePath(path.to_owned()));
    }
    Ok(())
}

fn resolve_catalog_ref(current: &NsslCatalogPath, href: &str) -> Result<NsslCatalogPath> {
    if href.contains("://") || href.starts_with('/') {
        return Err(DataSourceError::UnsafeResearchArchivePath(href.to_owned()));
    }
    let parent = current.0.rsplit_once('/').map_or("", |(parent, _)| parent);
    let joined = if parent.is_empty() {
        href.to_owned()
    } else {
        format!("{parent}/{href}")
    };
    validate_catalog_path(&joined)?;
    Ok(NsslCatalogPath(joined))
}

fn parse_catalog(path: NsslCatalogPath, xml: &str) -> Result<NsslCatalog> {
    let parsed: ThreddsCatalogXml = quick_xml::de::from_str(xml)?;
    let mut directories = Vec::new();
    for reference in parsed.dataset.catalog_refs {
        let Some(href) = reference.href.or(reference.prefixed_href) else {
            continue;
        };
        let target = resolve_catalog_ref(&path, &href)?;
        directories.push(NsslCatalogDirectory {
            name: reference.name,
            path: target,
        });
    }
    directories.sort_by(|left, right| left.name.cmp(&right.name));

    let mut files = parsed
        .dataset
        .datasets
        .into_iter()
        .filter_map(|dataset| {
            let url_path = dataset.url_path?;
            if !url_path.contains("/IQ/") || !dataset.name.starts_with("KOUN_RVP.") {
                return None;
            }
            if validate_file_path(&url_path).is_err() {
                return None;
            }
            let estimated_size_bytes = dataset.data_size.and_then(|size| size.bytes());
            let modified = dataset.date.and_then(|date| {
                DateTime::parse_from_rfc3339(date.value.trim())
                    .ok()
                    .map(|time| time.with_timezone(&Utc))
            });
            Some(NsslLevel1File {
                name: dataset.name,
                estimated_size_bytes,
                modified,
                url_path,
            })
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(NsslCatalog {
        path,
        directories,
        files,
    })
}

#[derive(Debug, Deserialize)]
struct ThreddsCatalogXml {
    dataset: ThreddsDatasetRootXml,
}

#[derive(Debug, Deserialize)]
struct ThreddsDatasetRootXml {
    #[serde(rename = "catalogRef", default)]
    catalog_refs: Vec<ThreddsCatalogRefXml>,
    #[serde(rename = "dataset", default)]
    datasets: Vec<ThreddsDatasetXml>,
}

#[derive(Debug, Deserialize)]
struct ThreddsCatalogRefXml {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@href", default)]
    href: Option<String>,
    #[serde(rename = "@xlink:href", default)]
    prefixed_href: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ThreddsDatasetXml {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@urlPath", default)]
    url_path: Option<String>,
    #[serde(rename = "dataSize", default)]
    data_size: Option<ThreddsDataSizeXml>,
    #[serde(rename = "date", default)]
    date: Option<ThreddsDateXml>,
}

#[derive(Debug, Deserialize)]
struct ThreddsDataSizeXml {
    #[serde(rename = "@units")]
    units: String,
    #[serde(rename = "$text")]
    value: String,
}

impl ThreddsDataSizeXml {
    fn bytes(self) -> Option<u64> {
        let value = self.value.trim().parse::<f64>().ok()?;
        let multiplier = match self.units.as_str() {
            "bytes" => 1.0,
            "Kbytes" => 1_000.0,
            "Mbytes" => 1_000_000.0,
            "Gbytes" => 1_000_000_000.0,
            _ => return None,
        };
        let bytes = value * multiplier;
        (bytes.is_finite() && bytes >= 0.0 && bytes <= u64::MAX as f64)
            .then_some(bytes.round() as u64)
    }
}

#[derive(Debug, Deserialize)]
struct ThreddsDateXml {
    #[serde(rename = "$text")]
    value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<catalog xmlns="http://www.unidata.ucar.edu/namespaces/thredds/InvCatalog/v1.0"
         xmlns:xlink="http://www.w3.org/1999/xlink" version="1.2">
  <dataset name="2013/">
    <catalogRef xlink:href="KOUN_20130520/catalog.xml" xlink:title="KOUN_20130520"
                name="KOUN_20130520" />
    <dataset name="KOUN_RVP.20130520.194555.936.Ascope_DEFAULT.0.H+V.300"
             urlPath="RRDD/KOUN/2013/KOUN_20130520/IQ/KOUN_RVP.20130520.194555.936.Ascope_DEFAULT.0.H+V.300">
      <dataSize units="Mbytes">16.37</dataSize>
      <date type="modified">2013-06-11T22:47:15Z</date>
    </dataset>
    <dataset name="processed.nc" urlPath="RRDD/KOUN/2013/processed.nc" />
  </dataset>
</catalog>"#;

    #[test]
    fn thredds_catalog_exposes_directories_and_only_level1_files() {
        let catalog = parse_catalog(NsslCatalogPath("2013/catalog.xml".to_owned()), CATALOG)
            .expect("catalog parses");
        assert_eq!(catalog.directories.len(), 1);
        assert_eq!(catalog.directories[0].name, "KOUN_20130520");
        assert_eq!(
            catalog.directories[0].path.display(),
            "2013/KOUN_20130520/catalog.xml"
        );
        assert_eq!(catalog.files.len(), 1);
        assert_eq!(catalog.files[0].estimated_size_bytes, Some(16_370_000));
        assert!(catalog.files[0].is_verified_sample());
    }

    #[test]
    fn paths_cannot_leave_the_fixed_nssl_koun_archive() {
        let current = NsslCatalogPath::root();
        for hostile in [
            "https://example.invalid/catalog.xml",
            "../secret/catalog.xml",
            "/RRDD/KOUN/catalog.xml",
            "safe/catalog.xml?elsewhere=1",
        ] {
            assert!(resolve_catalog_ref(&current, hostile).is_err(), "{hostile}");
        }
        for hostile in [
            "https://example.invalid/file",
            "RRDD/KOUN/../../secret",
            "RRDD/KOUN/IQ/file?redirect=1",
            "RRDD/OTHER/IQ/KOUN_RVP.file",
        ] {
            assert!(validate_file_path(hostile).is_err(), "{hostile}");
        }
    }

    #[test]
    fn downloads_never_overwrite_an_existing_file_or_partial() {
        let directory = std::env::temp_dir().join(format!(
            "genericradar-research-download-name-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("test directory");
        fs::write(directory.join("KOUN_RVP.sample"), b"existing").expect("seed destination");
        fs::write(directory.join("KOUN_RVP.sample.1.download"), b"partial").expect("seed partial");

        let (destination, partial) =
            unused_destination(&directory, "KOUN_RVP.sample").expect("unused name");
        assert_eq!(
            destination.file_name().and_then(|name| name.to_str()),
            Some("KOUN_RVP.sample.2")
        );
        assert_eq!(
            partial.file_name().and_then(|name| name.to_str()),
            Some("KOUN_RVP.sample.2.download")
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    #[ignore = "queries the live NOAA/NSSL THREDDS catalog"]
    fn official_verified_case_catalog_is_machine_readable() {
        let catalog = list_nssl_koun_catalog(&NsslCatalogPath::verified_case())
            .expect("official catalog responds");
        assert!(catalog.files.len() > 100, "{}", catalog.files.len());
        assert!(catalog.files.iter().any(NsslLevel1File::is_verified_sample));
    }

    #[test]
    #[ignore = "downloads a 4.4 MB real KOUN Level-I record from NOAA/NSSL"]
    fn official_level1_record_streams_to_a_complete_local_file() {
        let catalog = list_nssl_koun_catalog(&NsslCatalogPath::verified_case())
            .expect("official catalog responds");
        let file = catalog
            .files
            .into_iter()
            .find(|file| file.name == "KOUN_RVP.20130520.194601.730.Ascope_DEFAULT.0.H+V.250")
            .expect("known small record is listed");
        let directory = std::env::temp_dir().join(format!(
            "genericradar-research-download-real-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        let downloaded = download_nssl_level1_file(&file, &directory, &|_, _| {}, &|| false)
            .expect("record downloads");
        assert!(downloaded.bytes > 4_000_000, "{}", downloaded.bytes);
        assert_eq!(
            fs::metadata(&downloaded.path)
                .expect("download exists")
                .len(),
            downloaded.bytes
        );
        fs::remove_dir_all(directory).expect("remove real-download test directory");
    }
}

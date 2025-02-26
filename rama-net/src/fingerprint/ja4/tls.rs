use itertools::Itertools as _;
use std::{borrow::Cow, fmt};

use rama_core::context::Extensions;

use crate::tls::{
    ApplicationProtocol, CipherSuite, ExtensionId, ProtocolVersion, SecureTransport,
    SignatureScheme, client::NegotiatedTlsParameters,
};

#[derive(Clone)]
/// Input data for a "ja4" hash.
///
/// Computed using [`Ja4::compute`].
pub struct Ja4 {
    protocol: TransportProtocol,
    version: TlsVersion,
    has_sni: bool,
    alpn: Option<ApplicationProtocol>,
    cipher_suites: Vec<CipherSuite>,
    extensions: Option<Vec<ExtensionId>>,
    signature_algorithms: Option<Vec<SignatureScheme>>,
}

impl Ja4 {
    /// Compute the [`Ja4`] (hash).
    ///
    /// As specified by <https://blog.foxio.io/ja4%2B-network-fingerprinting>
    /// and reference implementations found at <https://github.com/FoxIO-LLC/ja4>.
    pub fn compute(ext: &Extensions) -> Result<Self, Ja4ComputeError> {
        let client_hello = ext
            .get::<SecureTransport>()
            .and_then(|st| st.client_hello())
            .ok_or(Ja4ComputeError::MissingClientHello)?;

        let version: TlsVersion = match ext.get::<NegotiatedTlsParameters>() {
            Some(params) => params.protocol_version,
            None => {
                tracing::trace!("NegotiatedTlsParameters missing: fallback to client hello tls.. (backward compat)");
                client_hello.protocol_version()
            }
        }.try_into()?;

        let mut cipher_suites: Vec<_> = client_hello
            .cipher_suites()
            .iter()
            .filter(|c| !c.is_grease())
            .copied()
            .collect();
        if cipher_suites.is_empty() {
            return Err(Ja4ComputeError::EmptyCipherSuites);
        }
        cipher_suites.sort_unstable_by_key(|k| format!("{k:04x}"));

        let mut extensions = None;
        let mut alpn = None;
        let mut signature_algorithms = None;
        let mut protocol = TransportProtocol::Tcp;
        let mut has_sni = false;

        let ce_extensions = client_hello.extensions();
        for ext in ce_extensions {
            let id = ext.id();

            match id {
                ExtensionId::QUIC_TRANSPORT_PARAMETERS => {
                    protocol = TransportProtocol::Quic;
                }
                ExtensionId::SERVER_NAME => {
                    has_sni = true;
                }
                _ => {
                    if id.is_grease() {
                        continue;
                    }
                }
            }

            extensions
                .get_or_insert_with(|| Vec::with_capacity(ce_extensions.len()))
                .push(id);

            match ext {
                crate::tls::client::ClientHelloExtension::ApplicationLayerProtocolNegotiation(
                    alpns,
                ) => {
                    alpn = alpns.iter().next().cloned();
                }
                crate::tls::client::ClientHelloExtension::SignatureAlgorithms(vec) => {
                    // this one is the only one not sorted
                    let vec: Vec<_> = vec.iter().filter(|g| !g.is_grease()).copied().collect();
                    if !vec.is_empty() {
                        signature_algorithms = Some(vec)
                    }
                }
                _ => (),
            }
        }

        if let Some(extensions) = extensions.as_mut() {
            extensions.sort_unstable_by_key(|k| format!("{k:04x}"));
        }

        Ok(Self {
            protocol,
            version,
            has_sni,
            alpn,
            cipher_suites,
            extensions,
            signature_algorithms,
        })
    }

    #[inline]
    pub fn to_human_string(&self) -> String {
        format!("{self:?}")
    }

    fn fmt_as(&self, f: &mut fmt::Formatter<'_>, hash_chunks: bool) -> fmt::Result {
        let protocol = self.protocol;
        let version = self.version;
        let sni_marker = if self.has_sni { 'd' } else { 'i' };
        let nr_ciphers = 99.min(self.cipher_suites.len());
        let nr_exts = 99.min(
            self.extensions
                .as_ref()
                .map(|ext| ext.len())
                .unwrap_or_default(),
        );
        let mut alpn_it = self
            .alpn
            .as_ref()
            .and_then(|alpn| std::str::from_utf8(alpn.as_bytes()).ok())
            .map(|s| s.chars())
            .into_iter()
            .flatten();
        let alpn_0 = alpn_it.next().unwrap_or('0');
        let alpn_1 = alpn_it.last().unwrap_or('0');

        // JA4_a (AKA first chunk)
        write!(
            f,
            "{protocol}{version}{sni_marker}{nr_ciphers:02}{nr_exts:02}{alpn_0}{alpn_1}"
        )?;

        // JA4_b (AKA Cipher Suites, sorted)
        let cipher_suites = self
            .cipher_suites
            .iter()
            .map(|c| format!("{c:04x}"))
            .join(",");

        // JA4_c (AKA Exts + Sigs)
        let extensions =
            self.extensions
                .as_ref()
                .map(|e| e.iter())
                .into_iter()
                .flatten()
                .filter_map(|e| match e {
                    ExtensionId::SERVER_NAME
                    | ExtensionId::APPLICATION_LAYER_PROTOCOL_NEGOTIATION => None,
                    _ => Some(format!("{e:04x}")),
                })
                .join(",");
        let signature_algorithms = self
            .signature_algorithms
            .as_ref()
            .map(|s| s.iter())
            .into_iter()
            .flatten()
            .map(|s| format!("{s:04x}"))
            .join(",");
        let ext_sig_sep = if signature_algorithms.is_empty() {
            ""
        } else {
            "_"
        };

        if hash_chunks {
            write!(
                f,
                "_{}_{}",
                hash12(cipher_suites),
                hash12(format!(
                    "{}{}{}",
                    extensions, ext_sig_sep, signature_algorithms,
                )),
            )
        } else {
            write!(
                f,
                "_{}_{}{}{}",
                cipher_suites, extensions, ext_sig_sep, signature_algorithms,
            )
        }
    }
}

impl fmt::Display for Ja4 {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_as(f, true)
    }
}

impl fmt::Debug for Ja4 {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_as(f, false)
    }
}

fn hash12(s: impl AsRef<str>) -> Cow<'static, str> {
    use sha2::{Digest as _, Sha256};

    let s = s.as_ref();
    if s.is_empty() {
        "000000000000".into()
    } else {
        let sha256 = Sha256::digest(s);
        hex::encode(&sha256.as_slice()[..6]).into()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
enum TransportProtocol {
    Tcp,
    Quic,
}

impl fmt::Display for TransportProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            TransportProtocol::Tcp => "t",
            TransportProtocol::Quic => "q",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
enum TlsVersion {
    Tls1_0,
    Tls1_1,
    Tls1_2,
    Tls1_3,
}

impl fmt::Display for TlsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            TlsVersion::Tls1_0 => "10",
            TlsVersion::Tls1_1 => "11",
            TlsVersion::Tls1_2 => "12",
            TlsVersion::Tls1_3 => "13",
        };
        f.write_str(s)
    }
}

impl TryFrom<ProtocolVersion> for TlsVersion {
    type Error = Ja4ComputeError;

    fn try_from(value: ProtocolVersion) -> Result<Self, Self::Error> {
        match value {
            ProtocolVersion::SSLv2
            | ProtocolVersion::SSLv3
            | ProtocolVersion::TLSv1_0
            | ProtocolVersion::DTLSv1_0 => Ok(Self::Tls1_0),
            ProtocolVersion::TLSv1_1 => Ok(Self::Tls1_1),
            ProtocolVersion::TLSv1_2 | ProtocolVersion::DTLSv1_2 => Ok(Self::Tls1_2),
            ProtocolVersion::TLSv1_3 | ProtocolVersion::DTLSv1_3 => Ok(Self::Tls1_3),
            _ => Err(Ja4ComputeError::InvalidTlsVersion),
        }
    }
}

#[derive(Debug, Clone)]
/// error identifying a failure in [`Ja4::compute`]
pub enum Ja4ComputeError {
    /// missing [`ClientHello`]
    MissingClientHello,
    /// cipher suites was empty
    EmptyCipherSuites,
    /// invalid tls version
    InvalidTlsVersion,
}

impl fmt::Display for Ja4ComputeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ja4ComputeError::MissingClientHello => {
                write!(f, "Ja4 Compute Error: missing client hello")
            }
            Ja4ComputeError::EmptyCipherSuites => {
                write!(f, "Ja4 Compute Error: empty cipher suites")
            }
            Ja4ComputeError::InvalidTlsVersion => {
                write!(f, "Ja4 Compute Error: invalid tls version")
            }
        }
    }
}

impl std::error::Error for Ja4ComputeError {}

#[cfg(test)]
mod tests {
    use crate::tls::client::parse_client_hello;

    use super::*;

    #[derive(Debug)]
    struct TestCase {
        client_hello: Vec<u8>,
        negotiated_protocol_version: Option<ProtocolVersion>,
        pcap: &'static str,
        expected_ja4_str: &'static str,
        expected_ja4_hash: &'static str,
    }

    #[test]
    fn test_ja4_compute() {
        // src: <https://github.com/jabedude/ja3-rs/blob/a30d1bea03d2230b1239d437c3f6af7fb7699338/src/lib.rs#L380>
        // + random wireshark
        // + random curl to echo.ramaproxy.org over http/1.1
        let test_cases = [
            TestCase {
                client_hello: vec![
                    0x3, 0x3, 0x86, 0xad, 0xa4, 0xcc, 0x19, 0xe7, 0x14, 0x54, 0x54, 0xfd, 0xe7,
                    0x37, 0x33, 0xdf, 0x66, 0xcb, 0xf6, 0xef, 0x3e, 0xc0, 0xa1, 0x54, 0xc6, 0xdd,
                    0x14, 0x5e, 0xc0, 0x83, 0xac, 0xb9, 0xb4, 0xe7, 0x20, 0x1c, 0x64, 0xae, 0xa7,
                    0xa2, 0xc3, 0xe1, 0x8c, 0xd1, 0x25, 0x2, 0x4d, 0xf7, 0x86, 0x4a, 0xc7, 0x19,
                    0xd0, 0xc4, 0xbd, 0xfb, 0x40, 0xc2, 0xef, 0x7f, 0x6d, 0xd3, 0x9a, 0xa7, 0x53,
                    0xdf, 0xdd, 0x0, 0x22, 0x1a, 0x1a, 0x13, 0x1, 0x13, 0x2, 0x13, 0x3, 0xc0, 0x2b,
                    0xc0, 0x2f, 0xc0, 0x2c, 0xc0, 0x30, 0xcc, 0xa9, 0xcc, 0xa8, 0xc0, 0x13, 0xc0,
                    0x14, 0x0, 0x9c, 0x0, 0x9d, 0x0, 0x2f, 0x0, 0x35, 0x0, 0xa, 0x1, 0x0, 0x1,
                    0x91, 0xa, 0xa, 0x0, 0x0, 0x0, 0x0, 0x0, 0x20, 0x0, 0x1e, 0x0, 0x0, 0x1b, 0x67,
                    0x6f, 0x6f, 0x67, 0x6c, 0x65, 0x61, 0x64, 0x73, 0x2e, 0x67, 0x2e, 0x64, 0x6f,
                    0x75, 0x62, 0x6c, 0x65, 0x63, 0x6c, 0x69, 0x63, 0x6b, 0x2e, 0x6e, 0x65, 0x74,
                    0x0, 0x17, 0x0, 0x0, 0xff, 0x1, 0x0, 0x1, 0x0, 0x0, 0xa, 0x0, 0xa, 0x0, 0x8,
                    0x9a, 0x9a, 0x0, 0x1d, 0x0, 0x17, 0x0, 0x18, 0x0, 0xb, 0x0, 0x2, 0x1, 0x0, 0x0,
                    0x23, 0x0, 0x0, 0x0, 0x10, 0x0, 0xe, 0x0, 0xc, 0x2, 0x68, 0x32, 0x8, 0x68,
                    0x74, 0x74, 0x70, 0x2f, 0x31, 0x2e, 0x31, 0x0, 0x5, 0x0, 0x5, 0x1, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0xd, 0x0, 0x14, 0x0, 0x12, 0x4, 0x3, 0x8, 0x4, 0x4, 0x1, 0x5,
                    0x3, 0x8, 0x5, 0x5, 0x1, 0x8, 0x6, 0x6, 0x1, 0x2, 0x1, 0x0, 0x12, 0x0, 0x0,
                    0x0, 0x33, 0x0, 0x2b, 0x0, 0x29, 0x9a, 0x9a, 0x0, 0x1, 0x0, 0x0, 0x1d, 0x0,
                    0x20, 0x59, 0x8, 0x6f, 0x41, 0x9a, 0xa5, 0xaa, 0x1d, 0x81, 0xe3, 0x47, 0xf0,
                    0x25, 0x5f, 0x92, 0x7, 0xfc, 0x4b, 0x13, 0x74, 0x51, 0x46, 0x98, 0x8, 0x74,
                    0x3b, 0xde, 0x57, 0x86, 0xe8, 0x2c, 0x74, 0x0, 0x2d, 0x0, 0x2, 0x1, 0x1, 0x0,
                    0x2b, 0x0, 0xb, 0xa, 0xfa, 0xfa, 0x3, 0x4, 0x3, 0x3, 0x3, 0x2, 0x3, 0x1, 0x0,
                    0x1b, 0x0, 0x3, 0x2, 0x0, 0x2, 0xba, 0xba, 0x0, 0x1, 0x0, 0x0, 0x15, 0x0, 0xbd,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                ],
                negotiated_protocol_version: Some(ProtocolVersion::TLSv1_3),
                pcap: "chrome-grease-single.pcap",
                expected_ja4_str: "t13d1615h2_000a,002f,0035,009c,009d,1301,1302,1303,c013,c014,c02b,c02c,c02f,c030,cca8,cca9_0005,000a,000b,000d,0012,0015,0017,001b,0023,002b,002d,0033,ff01_0403,0804,0401,0503,0805,0501,0806,0601,0201",
                expected_ja4_hash: "t13d1615h2_46e7e9700bed_45f260be83e2",
            },
            TestCase {
                client_hello: vec![
                    0x03, 0x03, 0x95, 0xb9, 0xc5, 0xa1, 0x35, 0x0d, 0xc2, 0x47, 0x9d, 0x37, 0x77,
                    0x94, 0x51, 0x39, 0x08, 0xc1, 0x67, 0x43, 0x08, 0xa4, 0x53, 0xb3, 0x18, 0x7e,
                    0x0c, 0xde, 0x18, 0xd6, 0x77, 0x1d, 0xd7, 0x0c, 0x20, 0x5b, 0x41, 0xe2, 0xb4,
                    0xe3, 0x28, 0x26, 0xfd, 0x1a, 0x14, 0xab, 0x14, 0x04, 0x0b, 0xe2, 0xe1, 0x66,
                    0x12, 0xbd, 0x44, 0x41, 0x38, 0xcd, 0xb3, 0xcf, 0xa1, 0x44, 0xe0, 0xa4, 0xf7,
                    0x5d, 0x90, 0x00, 0x3e, 0x13, 0x02, 0x13, 0x03, 0x13, 0x01, 0xc0, 0x2c, 0xc0,
                    0x30, 0x00, 0x9f, 0xcc, 0xa9, 0xcc, 0xa8, 0xcc, 0xaa, 0xc0, 0x2b, 0xc0, 0x2f,
                    0x00, 0x9e, 0xc0, 0x24, 0xc0, 0x28, 0x00, 0x6b, 0xc0, 0x23, 0xc0, 0x27, 0x00,
                    0x67, 0xc0, 0x0a, 0xc0, 0x14, 0x00, 0x39, 0xc0, 0x09, 0xc0, 0x13, 0x00, 0x33,
                    0x00, 0x9d, 0x00, 0x9c, 0x00, 0x3d, 0x00, 0x3c, 0x00, 0x35, 0x00, 0x2f, 0x00,
                    0xff, 0x01, 0x00, 0x01, 0x75, 0x00, 0x00, 0x00, 0x17, 0x00, 0x15, 0x00, 0x00,
                    0x12, 0x65, 0x63, 0x68, 0x6f, 0x2e, 0x72, 0x61, 0x6d, 0x61, 0x70, 0x72, 0x6f,
                    0x78, 0x79, 0x2e, 0x6f, 0x72, 0x67, 0x00, 0x0b, 0x00, 0x04, 0x03, 0x00, 0x01,
                    0x02, 0x00, 0x0a, 0x00, 0x16, 0x00, 0x14, 0x00, 0x1d, 0x00, 0x17, 0x00, 0x1e,
                    0x00, 0x19, 0x00, 0x18, 0x01, 0x00, 0x01, 0x01, 0x01, 0x02, 0x01, 0x03, 0x01,
                    0x04, 0x33, 0x74, 0x00, 0x00, 0x00, 0x10, 0x00, 0x0b, 0x00, 0x09, 0x08, 0x68,
                    0x74, 0x74, 0x70, 0x2f, 0x31, 0x2e, 0x31, 0x00, 0x16, 0x00, 0x00, 0x00, 0x17,
                    0x00, 0x00, 0x00, 0x31, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x2a, 0x00, 0x28, 0x04,
                    0x03, 0x05, 0x03, 0x06, 0x03, 0x08, 0x07, 0x08, 0x08, 0x08, 0x09, 0x08, 0x0a,
                    0x08, 0x0b, 0x08, 0x04, 0x08, 0x05, 0x08, 0x06, 0x04, 0x01, 0x05, 0x01, 0x06,
                    0x01, 0x03, 0x03, 0x03, 0x01, 0x03, 0x02, 0x04, 0x02, 0x05, 0x02, 0x06, 0x02,
                    0x00, 0x2b, 0x00, 0x05, 0x04, 0x03, 0x04, 0x03, 0x03, 0x00, 0x2d, 0x00, 0x02,
                    0x01, 0x01, 0x00, 0x33, 0x00, 0x26, 0x00, 0x24, 0x00, 0x1d, 0x00, 0x20, 0xe3,
                    0x86, 0xb6, 0x7d, 0x52, 0x0e, 0xd1, 0x7f, 0xbe, 0xed, 0xc0, 0xe8, 0xd9, 0x94,
                    0x4a, 0x7b, 0xff, 0xb8, 0xa0, 0x13, 0xa8, 0x5f, 0xbd, 0x2b, 0x10, 0x51, 0xa1,
                    0x3f, 0xb2, 0xe3, 0x37, 0x5d, 0x00, 0x15, 0x00, 0xae, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00,
                ],
                negotiated_protocol_version: Some(ProtocolVersion::TLSv1_3),
                pcap: "curl_http1.1.pcap",
                expected_ja4_str: "t13d3113h1_002f,0033,0035,0039,003c,003d,0067,006b,009c,009d,009e,009f,00ff,1301,1302,1303,c009,c00a,c013,c014,c023,c024,c027,c028,c02b,c02c,c02f,c030,cca8,cca9,ccaa_000a,000b,000d,0015,0016,0017,002b,002d,0031,0033,3374_0403,0503,0603,0807,0808,0809,080a,080b,0804,0805,0806,0401,0501,0601,0303,0301,0302,0402,0502,0602",
                expected_ja4_hash: "t13d3113h1_e8f1e7e78f70_ce5650b735ce",
            },
            TestCase {
                client_hello: vec![
                    0x3, 0x3, 0xf6, 0x65, 0xb, 0x22, 0x13, 0xf1, 0xc3, 0xe9, 0xe7, 0xb3, 0xdc, 0x9,
                    0xe4, 0x4b, 0xcb, 0x6e, 0x5, 0xaf, 0x8f, 0x2f, 0x41, 0x8d, 0x15, 0xa8, 0x88,
                    0x46, 0x24, 0x83, 0xca, 0x9, 0x7c, 0x95, 0x20, 0x12, 0xc4, 0x5e, 0x71, 0x8b,
                    0xb9, 0xc9, 0xa9, 0x37, 0x93, 0x4c, 0x41, 0xa6, 0xe8, 0x9e, 0x8f, 0x15, 0x78,
                    0x52, 0xe, 0x3c, 0x28, 0xba, 0xab, 0xa3, 0x34, 0x8b, 0x53, 0x82, 0x83, 0x75,
                    0x24, 0x0, 0x3e, 0x13, 0x2, 0x13, 0x3, 0x13, 0x1, 0xc0, 0x2c, 0xc0, 0x30, 0x0,
                    0x9f, 0xcc, 0xa9, 0xcc, 0xa8, 0xcc, 0xaa, 0xc0, 0x2b, 0xc0, 0x2f, 0x0, 0x9e,
                    0xc0, 0x24, 0xc0, 0x28, 0x0, 0x6b, 0xc0, 0x23, 0xc0, 0x27, 0x0, 0x67, 0xc0,
                    0xa, 0xc0, 0x14, 0x0, 0x39, 0xc0, 0x9, 0xc0, 0x13, 0x0, 0x33, 0x0, 0x9d, 0x0,
                    0x9c, 0x0, 0x3d, 0x0, 0x3c, 0x0, 0x35, 0x0, 0x2f, 0x0, 0xff, 0x1, 0x0, 0x1,
                    0x75, 0x0, 0x0, 0x0, 0x10, 0x0, 0xe, 0x0, 0x0, 0xb, 0x65, 0x78, 0x61, 0x6d,
                    0x70, 0x6c, 0x65, 0x2e, 0x63, 0x6f, 0x6d, 0x0, 0xb, 0x0, 0x4, 0x3, 0x0, 0x1,
                    0x2, 0x0, 0xa, 0x0, 0xc, 0x0, 0xa, 0x0, 0x1d, 0x0, 0x17, 0x0, 0x1e, 0x0, 0x19,
                    0x0, 0x18, 0x33, 0x74, 0x0, 0x0, 0x0, 0x10, 0x0, 0xe, 0x0, 0xc, 0x2, 0x68,
                    0x32, 0x8, 0x68, 0x74, 0x74, 0x70, 0x2f, 0x31, 0x2e, 0x31, 0x0, 0x16, 0x0, 0x0,
                    0x0, 0x17, 0x0, 0x0, 0x0, 0xd, 0x0, 0x30, 0x0, 0x2e, 0x4, 0x3, 0x5, 0x3, 0x6,
                    0x3, 0x8, 0x7, 0x8, 0x8, 0x8, 0x9, 0x8, 0xa, 0x8, 0xb, 0x8, 0x4, 0x8, 0x5, 0x8,
                    0x6, 0x4, 0x1, 0x5, 0x1, 0x6, 0x1, 0x3, 0x3, 0x2, 0x3, 0x3, 0x1, 0x2, 0x1, 0x3,
                    0x2, 0x2, 0x2, 0x4, 0x2, 0x5, 0x2, 0x6, 0x2, 0x0, 0x2b, 0x0, 0x9, 0x8, 0x3,
                    0x4, 0x3, 0x3, 0x3, 0x2, 0x3, 0x1, 0x0, 0x2d, 0x0, 0x2, 0x1, 0x1, 0x0, 0x33,
                    0x0, 0x26, 0x0, 0x24, 0x0, 0x1d, 0x0, 0x20, 0x37, 0x98, 0x48, 0x7f, 0x2f, 0xbc,
                    0x86, 0xf9, 0xb8, 0x2, 0xcd, 0x31, 0xf0, 0x4, 0x30, 0xa9, 0x2f, 0x29, 0x61,
                    0xac, 0xec, 0xc9, 0x2f, 0xf7, 0x45, 0xad, 0xd9, 0x67, 0x7, 0x14, 0x62, 0x1,
                    0x0, 0x15, 0x0, 0xb6, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                ],
                negotiated_protocol_version: Some(ProtocolVersion::TLSv1_3),
                pcap: "curl.pcap",
                expected_ja4_str: "t13d3112h2_002f,0033,0035,0039,003c,003d,0067,006b,009c,009d,009e,009f,00ff,1301,1302,1303,c009,c00a,c013,c014,c023,c024,c027,c028,c02b,c02c,c02f,c030,cca8,cca9,ccaa_000a,000b,000d,0015,0016,0017,002b,002d,0033,3374_0403,0503,0603,0807,0808,0809,080a,080b,0804,0805,0806,0401,0501,0601,0303,0203,0301,0201,0302,0202,0402,0502,0602",
                expected_ja4_hash: "t13d3112h2_e8f1e7e78f70_f4b9272caa35",
            },
            TestCase {
                client_hello: vec![
                    0x3, 0x3, 0x14, 0x67, 0xca, 0x9a, 0xe4, 0x41, 0xc2, 0x31, 0xe7, 0xa4, 0x87,
                    0xfa, 0x83, 0xdf, 0x5c, 0xe4, 0xa1, 0x9d, 0xa1, 0x42, 0x39, 0xda, 0xd, 0xf0,
                    0x3e, 0xc3, 0xfb, 0xb3, 0xaf, 0xec, 0x5b, 0x14, 0x20, 0x6e, 0xd5, 0x9f, 0x39,
                    0x1d, 0x5e, 0x20, 0x51, 0x38, 0xdc, 0x63, 0x5d, 0xe0, 0xbf, 0x1b, 0xff, 0xa0,
                    0x3d, 0xde, 0x20, 0x59, 0x33, 0x40, 0x30, 0x6e, 0x31, 0x2c, 0xdf, 0x8e, 0x7a,
                    0xd5, 0xe9, 0x0, 0x22, 0x13, 0x1, 0x13, 0x3, 0x13, 0x2, 0xc0, 0x2b, 0xc0, 0x2f,
                    0xcc, 0xa9, 0xcc, 0xa8, 0xc0, 0x2c, 0xc0, 0x30, 0xc0, 0xa, 0xc0, 0x9, 0xc0,
                    0x13, 0xc0, 0x14, 0x0, 0x9c, 0x0, 0x9d, 0x0, 0x2f, 0x0, 0x35, 0x1, 0x0, 0x6,
                    0xf2, 0x0, 0x0, 0x0, 0x12, 0x0, 0x10, 0x0, 0x0, 0xd, 0x72, 0x61, 0x6d, 0x61,
                    0x70, 0x72, 0x6f, 0x78, 0x79, 0x2e, 0x6f, 0x72, 0x67, 0x0, 0x17, 0x0, 0x0,
                    0xff, 0x1, 0x0, 0x1, 0x0, 0x0, 0xa, 0x0, 0x10, 0x0, 0xe, 0x11, 0xec, 0x0, 0x1d,
                    0x0, 0x17, 0x0, 0x18, 0x0, 0x19, 0x1, 0x0, 0x1, 0x1, 0x0, 0xb, 0x0, 0x2, 0x1,
                    0x0, 0x0, 0x23, 0x0, 0x0, 0x0, 0x10, 0x0, 0xe, 0x0, 0xc, 0x2, 0x68, 0x32, 0x8,
                    0x68, 0x74, 0x74, 0x70, 0x2f, 0x31, 0x2e, 0x31, 0x0, 0x5, 0x0, 0x5, 0x1, 0x0,
                    0x0, 0x0, 0x0, 0x0, 0x22, 0x0, 0xa, 0x0, 0x8, 0x4, 0x3, 0x5, 0x3, 0x6, 0x3,
                    0x2, 0x3, 0x0, 0x33, 0x5, 0x2f, 0x5, 0x2d, 0x11, 0xec, 0x4, 0xc0, 0x75, 0xe5,
                    0x3, 0xee, 0x1c, 0xb6, 0x50, 0xc2, 0x40, 0x22, 0xfc, 0xa1, 0x70, 0x8, 0xcd,
                    0xda, 0x74, 0xbc, 0x49, 0xd0, 0xb, 0xad, 0x34, 0xb4, 0xdf, 0x78, 0xb, 0x90,
                    0x61, 0x29, 0xd0, 0xd6, 0x67, 0x98, 0xa0, 0x2a, 0x50, 0x95, 0x10, 0x65, 0x94,
                    0x8d, 0xe3, 0x9, 0x38, 0xe7, 0xf5, 0xc5, 0xae, 0xfb, 0x43, 0xf9, 0x86, 0xa8,
                    0xf2, 0xdc, 0x78, 0xfd, 0xd3, 0x31, 0x87, 0x16, 0xbf, 0xa8, 0x90, 0x58, 0xd1,
                    0xa7, 0x6b, 0x56, 0x2a, 0xb1, 0xd5, 0x92, 0x6f, 0x9a, 0x89, 0x25, 0x20, 0xa,
                    0x7b, 0x87, 0xcc, 0x6d, 0x61, 0xf8, 0x9f, 0x70, 0xb3, 0x97, 0x84, 0x10, 0xbd,
                    0x58, 0x46, 0xb, 0x88, 0xbc, 0x39, 0x53, 0xfa, 0x6c, 0x48, 0x5a, 0xbd, 0x67,
                    0x3, 0x3a, 0x7, 0x2, 0x58, 0xb9, 0x25, 0x2e, 0xb0, 0xe5, 0xa, 0x52, 0xa, 0xba,
                    0x11, 0xcb, 0x1e, 0xdf, 0x63, 0xa0, 0x3, 0x98, 0x1e, 0x14, 0x3a, 0x6b, 0x8a,
                    0x94, 0x9d, 0x48, 0xd7, 0xc, 0xa5, 0xd3, 0x71, 0x6a, 0x16, 0x97, 0xf1, 0xba,
                    0x8b, 0x15, 0xbc, 0xa1, 0x51, 0x67, 0x2, 0xfd, 0xfc, 0x5d, 0xc0, 0x72, 0x2a,
                    0x95, 0x9c, 0x1d, 0x15, 0xe6, 0xb7, 0xab, 0x12, 0x9a, 0xd3, 0x49, 0x83, 0x19,
                    0xfc, 0x10, 0x6e, 0x6a, 0x3d, 0x89, 0xf2, 0xa1, 0x64, 0x3, 0x6a, 0x4d, 0xc,
                    0xcd, 0x46, 0x53, 0x75, 0xb3, 0x77, 0x69, 0xd4, 0x61, 0x81, 0x8d, 0x3a, 0x94,
                    0x64, 0xac, 0xa2, 0xa7, 0x7c, 0xc, 0x2a, 0x5c, 0xe, 0xf, 0x45, 0x9e, 0x92,
                    0xf4, 0x1, 0x42, 0x3b, 0x85, 0x15, 0xd9, 0x9a, 0xa5, 0xb6, 0x5b, 0xd0, 0x26,
                    0x7e, 0x49, 0xcc, 0x3e, 0x2f, 0x82, 0x7, 0xc1, 0x81, 0xaa, 0xaf, 0xa4, 0x13,
                    0x32, 0xb0, 0x96, 0x82, 0xc2, 0xcb, 0x1, 0xf2, 0x54, 0x49, 0x93, 0x44, 0x1,
                    0x15, 0x90, 0x3a, 0xd1, 0x52, 0x2a, 0x78, 0x23, 0x2d, 0x78, 0x61, 0xa2, 0xa7,
                    0xaa, 0x83, 0xd3, 0xbb, 0x8e, 0x2a, 0x6e, 0xd, 0xc8, 0x95, 0x73, 0x6, 0x2f,
                    0xf0, 0xd2, 0x7a, 0x80, 0xda, 0xb, 0xdf, 0x4, 0x85, 0xcb, 0x19, 0x81, 0x16,
                    0x99, 0x47, 0xd3, 0xbc, 0x3c, 0x9d, 0xb4, 0x19, 0x1c, 0x40, 0x9c, 0x6e, 0x95,
                    0x1, 0xe, 0x94, 0x82, 0x26, 0xd1, 0x10, 0x55, 0x97, 0x76, 0xe, 0x2a, 0x53,
                    0x2a, 0x75, 0x7b, 0xdc, 0xf7, 0x16, 0x2d, 0x84, 0x69, 0x3e, 0xfa, 0x3f, 0xed,
                    0x4, 0x20, 0x58, 0x7c, 0x9, 0xee, 0x41, 0x9c, 0x4a, 0x25, 0x6, 0x2f, 0x29,
                    0x3d, 0x6, 0xac, 0x48, 0x2e, 0xd1, 0x65, 0xd9, 0x85, 0x74, 0xf0, 0xf8, 0x35,
                    0xcd, 0x14, 0x5f, 0x9c, 0x89, 0x4b, 0x39, 0xc0, 0xa4, 0x6f, 0x36, 0x39, 0x8,
                    0x70, 0xb4, 0xa4, 0x8, 0x4e, 0x6e, 0xd4, 0x27, 0x93, 0xb0, 0x22, 0x34, 0xfc,
                    0x52, 0xd8, 0x4a, 0x48, 0xd4, 0xf9, 0x9a, 0x89, 0xdc, 0xbf, 0xc8, 0x73, 0x77,
                    0xca, 0x64, 0x7, 0x8c, 0x2c, 0x95, 0x23, 0x43, 0x4a, 0x8a, 0xa6, 0xa5, 0xcc,
                    0xc, 0xc3, 0xc9, 0x6, 0x7e, 0xcd, 0xbc, 0x7, 0xbd, 0x55, 0x1f, 0x32, 0x64,
                    0x1b, 0x9b, 0xc9, 0x7e, 0xc7, 0xa, 0x79, 0x96, 0x48, 0xb9, 0xfa, 0x26, 0xa9,
                    0x9c, 0xf7, 0x3d, 0x8f, 0xb4, 0xa9, 0x90, 0x36, 0x23, 0xe4, 0x93, 0x9b, 0x9b,
                    0xda, 0x5a, 0x44, 0x10, 0xcf, 0xcd, 0xb5, 0x1d, 0x55, 0xe4, 0xaa, 0x11, 0x6a,
                    0x89, 0xca, 0x53, 0x94, 0xc8, 0xa1, 0x0, 0x11, 0x96, 0xca, 0xb4, 0x5a, 0xb4,
                    0x1d, 0x50, 0x1e, 0x3a, 0xd0, 0x5f, 0xa1, 0x41, 0x58, 0x11, 0xf6, 0x62, 0x61,
                    0x65, 0xc4, 0x4a, 0x28, 0x9a, 0x81, 0x6b, 0x9f, 0x8a, 0x67, 0x7e, 0x1a, 0x55,
                    0x10, 0xa4, 0xe7, 0x54, 0x25, 0xc6, 0x83, 0xf9, 0xe8, 0x54, 0x75, 0x39, 0x76,
                    0x69, 0x27, 0x1e, 0x72, 0xc5, 0x3c, 0xdf, 0x43, 0x9b, 0xbc, 0x9c, 0x4a, 0x1a,
                    0x91, 0x63, 0xd, 0x94, 0x58, 0x22, 0xf2, 0xa7, 0x99, 0x27, 0x5, 0x51, 0x13,
                    0x1f, 0xfa, 0xf8, 0x5c, 0x46, 0xf6, 0x83, 0xab, 0x82, 0xa5, 0xe, 0xc2, 0xaf,
                    0x96, 0x48, 0xa8, 0xf8, 0x1a, 0x32, 0x3d, 0xc1, 0xb0, 0x2d, 0x41, 0x71, 0x85,
                    0xf2, 0xc6, 0x27, 0x9b, 0xbc, 0x23, 0xa9, 0x57, 0x8, 0xf5, 0xf, 0xa9, 0x4c,
                    0x92, 0xbd, 0xd1, 0xa4, 0x13, 0x9a, 0xad, 0x3, 0x16, 0x34, 0xbe, 0xf1, 0xa3,
                    0xe0, 0x50, 0x56, 0x46, 0xfc, 0x49, 0x4, 0xc3, 0x2c, 0xdb, 0x55, 0x6, 0xcb,
                    0x78, 0x4e, 0xa4, 0xc7, 0x3f, 0xb3, 0xf2, 0x44, 0x56, 0x30, 0xb9, 0x76, 0x32,
                    0x36, 0x2, 0x4b, 0xaa, 0x9, 0x63, 0xd, 0xd4, 0x40, 0x98, 0xfd, 0x13, 0x99,
                    0x3b, 0x1b, 0x6b, 0x87, 0xdb, 0xa8, 0xc, 0xe2, 0xe, 0x38, 0x6b, 0x6d, 0x41,
                    0xf1, 0x1c, 0x56, 0x25, 0x1b, 0x8b, 0x1b, 0x67, 0x8c, 0xe7, 0x2b, 0xea, 0x42,
                    0x61, 0xbe, 0x5b, 0xa7, 0x64, 0x8a, 0xa4, 0xb1, 0x57, 0x19, 0x2e, 0xf2, 0x71,
                    0xe3, 0xa8, 0x27, 0xd1, 0xa9, 0x1, 0x2, 0x87, 0xf, 0x23, 0x88, 0x1a, 0x10,
                    0x54, 0x7f, 0x0, 0xaa, 0x56, 0x1d, 0x28, 0x6f, 0xff, 0xb9, 0x87, 0x8d, 0xc0,
                    0x54, 0x67, 0xd8, 0x3e, 0x52, 0x6a, 0x3d, 0x25, 0xab, 0x62, 0x8a, 0x78, 0x94,
                    0xf0, 0x4, 0xbb, 0x8c, 0x1a, 0x4b, 0x13, 0xf4, 0x95, 0x16, 0xe7, 0x55, 0xdf,
                    0x21, 0x1d, 0xfb, 0x86, 0xc8, 0x70, 0xb9, 0xcd, 0xef, 0x7b, 0x8c, 0xbd, 0x13,
                    0x1f, 0x6b, 0xbc, 0x5f, 0xff, 0xa5, 0x14, 0x7a, 0x81, 0x31, 0x28, 0x41, 0xc0,
                    0xbf, 0x87, 0x84, 0xa8, 0xdb, 0x39, 0x5e, 0xf5, 0x51, 0x4f, 0x5a, 0x3f, 0xa4,
                    0x4c, 0x4f, 0x6b, 0xca, 0x64, 0xe1, 0x46, 0x10, 0x6b, 0xe8, 0xa7, 0x12, 0x9a,
                    0x4d, 0xe0, 0xe1, 0x45, 0x4a, 0xf8, 0xf, 0xfe, 0x36, 0x76, 0x1a, 0x7a, 0x17,
                    0xe5, 0x4b, 0x5c, 0x8f, 0x98, 0x76, 0x41, 0x74, 0x8e, 0xfc, 0x47, 0x4f, 0x22,
                    0xe2, 0x4, 0x23, 0x63, 0xa3, 0x56, 0xac, 0x6, 0x47, 0xa3, 0x47, 0x80, 0x2a,
                    0x49, 0xbc, 0x76, 0x84, 0x70, 0x54, 0x52, 0xd1, 0xf5, 0x74, 0x2f, 0xe1, 0xba,
                    0x26, 0xa1, 0x72, 0xf0, 0x8b, 0x4a, 0xee, 0xa4, 0x12, 0x3, 0x78, 0x17, 0x1f,
                    0x20, 0xbf, 0xa5, 0x52, 0x93, 0x70, 0xe1, 0x73, 0x6d, 0x99, 0x93, 0x7e, 0xe5,
                    0x59, 0x11, 0x23, 0x9a, 0xb1, 0x47, 0xa2, 0xd6, 0xc1, 0x48, 0x3a, 0x71, 0x84,
                    0x7a, 0x27, 0x6f, 0x6, 0xc6, 0x45, 0x24, 0xd5, 0x48, 0xe5, 0x88, 0x22, 0x4f,
                    0xdb, 0xb4, 0x97, 0x94, 0x93, 0x1b, 0x8a, 0x61, 0xca, 0x94, 0xcc, 0x7b, 0x89,
                    0x58, 0x55, 0xd9, 0x3a, 0x4b, 0x9c, 0x4b, 0xd2, 0xfc, 0xc4, 0x5f, 0x7c, 0x9d,
                    0x53, 0xf8, 0x70, 0xcb, 0xf8, 0x40, 0x52, 0x1b, 0x7e, 0x60, 0xf9, 0x64, 0xa,
                    0x20, 0x5d, 0xe2, 0x62, 0xa3, 0x6b, 0x83, 0xc4, 0x8b, 0x25, 0x54, 0xde, 0xc3,
                    0x40, 0x77, 0x65, 0xb1, 0xbc, 0xc3, 0xaa, 0xe8, 0xb2, 0x29, 0xd3, 0xa5, 0x42,
                    0x1c, 0xe7, 0xcb, 0x8f, 0x22, 0xc6, 0x3d, 0x1b, 0x1a, 0x72, 0x1c, 0xba, 0xd7,
                    0x6a, 0x7b, 0xf, 0x96, 0xc6, 0x47, 0x57, 0x30, 0x88, 0xa7, 0x9f, 0x97, 0xf1,
                    0x7c, 0x7d, 0x55, 0xbf, 0xf4, 0x1, 0xcd, 0xa1, 0xe0, 0xc6, 0x29, 0xba, 0x26,
                    0x86, 0x9a, 0x35, 0x3b, 0xb9, 0x39, 0x39, 0x24, 0x32, 0x19, 0x12, 0x6b, 0xb6,
                    0x2b, 0x39, 0xee, 0x8a, 0x21, 0xe5, 0x17, 0x3b, 0xd4, 0x5b, 0x2d, 0x6c, 0xdb,
                    0xa7, 0x49, 0xf8, 0x47, 0x68, 0x9b, 0x73, 0xfa, 0xc9, 0x33, 0x23, 0xf0, 0x47,
                    0x4a, 0x82, 0xa5, 0x7f, 0x37, 0x45, 0x4e, 0x56, 0x83, 0x4c, 0xb2, 0x7f, 0x3,
                    0x70, 0x34, 0xd3, 0xcb, 0x37, 0xe9, 0x7a, 0x88, 0x52, 0x2b, 0xd, 0x6f, 0xfc,
                    0x40, 0x80, 0x75, 0x8a, 0x9a, 0xbb, 0x40, 0x53, 0x4a, 0x55, 0xe8, 0xca, 0xaa,
                    0xa1, 0x79, 0x54, 0x22, 0x8a, 0x72, 0x81, 0x85, 0x71, 0xeb, 0x95, 0x2d, 0x15,
                    0xeb, 0xbb, 0xa5, 0xb6, 0x9e, 0x99, 0xa9, 0x58, 0x1b, 0x15, 0x3d, 0xe0, 0x12,
                    0x70, 0xf5, 0xba, 0x45, 0xee, 0x94, 0x92, 0x3d, 0xbb, 0xbd, 0xeb, 0xa9, 0x4e,
                    0xc9, 0x7a, 0x15, 0x33, 0xb2, 0x8b, 0x32, 0xf0, 0x8f, 0x4, 0xd6, 0x66, 0x42,
                    0x86, 0x30, 0xd8, 0x40, 0xb4, 0xda, 0xa3, 0x63, 0xab, 0x17, 0x9, 0x57, 0x83,
                    0x5a, 0xb2, 0x75, 0xb9, 0x9, 0xb2, 0x3d, 0x34, 0xfb, 0x1, 0xfe, 0x29, 0x4b,
                    0x91, 0xd5, 0x8c, 0x42, 0x5b, 0xb6, 0x37, 0x52, 0xcf, 0xf2, 0xfb, 0x9, 0x17,
                    0x37, 0x88, 0x2, 0x2a, 0x8, 0x45, 0x33, 0x5b, 0xab, 0xba, 0x65, 0x4d, 0x9f,
                    0x4e, 0x8a, 0xaa, 0xc2, 0xdf, 0xa8, 0x39, 0xa2, 0x4b, 0xad, 0xf0, 0x67, 0xd9,
                    0x9e, 0x1, 0x9, 0x85, 0x77, 0x6, 0x4e, 0x7b, 0xd1, 0x54, 0xa5, 0xd5, 0x86,
                    0xbe, 0x29, 0xdc, 0x49, 0x4b, 0xc4, 0xd7, 0xef, 0xee, 0x4f, 0xd1, 0x92, 0x35,
                    0xb4, 0xc, 0xeb, 0x8, 0xfc, 0x2b, 0x8f, 0x27, 0x1, 0xa9, 0xc8, 0x7e, 0x6a,
                    0x67, 0xb1, 0x3b, 0x2, 0x0, 0x1d, 0x0, 0x20, 0xd5, 0x86, 0xbe, 0x29, 0xdc,
                    0x49, 0x4b, 0xc4, 0xd7, 0xef, 0xee, 0x4f, 0xd1, 0x92, 0x35, 0xb4, 0xc, 0xeb,
                    0x8, 0xfc, 0x2b, 0x8f, 0x27, 0x1, 0xa9, 0xc8, 0x7e, 0x6a, 0x67, 0xb1, 0x3b,
                    0x2, 0x0, 0x17, 0x0, 0x41, 0x4, 0x31, 0xca, 0xf3, 0xfb, 0x90, 0xe5, 0x48, 0x3f,
                    0x20, 0xd6, 0xbb, 0x7d, 0x93, 0x4f, 0xdb, 0x66, 0x9a, 0x76, 0x9a, 0x1a, 0x5,
                    0x6e, 0xf5, 0xc, 0x87, 0xb1, 0x18, 0xf8, 0x53, 0xdb, 0x3e, 0xa3, 0x45, 0xf,
                    0x92, 0x1e, 0x72, 0xc5, 0x8a, 0x3, 0x81, 0xe6, 0xa, 0x3d, 0xcf, 0xa7, 0x21,
                    0xf3, 0x11, 0x2d, 0xe6, 0x74, 0x98, 0x5f, 0xdb, 0x10, 0x8b, 0x3c, 0xf, 0xc5,
                    0x81, 0x14, 0xc9, 0x2d, 0x0, 0x2b, 0x0, 0x5, 0x4, 0x3, 0x4, 0x3, 0x3, 0x0, 0xd,
                    0x0, 0x18, 0x0, 0x16, 0x4, 0x3, 0x5, 0x3, 0x6, 0x3, 0x8, 0x4, 0x8, 0x5, 0x8,
                    0x6, 0x4, 0x1, 0x5, 0x1, 0x6, 0x1, 0x2, 0x3, 0x2, 0x1, 0x0, 0x2d, 0x0, 0x2,
                    0x1, 0x1, 0x0, 0x1c, 0x0, 0x2, 0x40, 0x1, 0x0, 0x1b, 0x0, 0x7, 0x6, 0x0, 0x1,
                    0x0, 0x2, 0x0, 0x3, 0xfe, 0xd, 0x1, 0x19, 0x0, 0x0, 0x1, 0x0, 0x3, 0x27, 0x0,
                    0x20, 0x22, 0x99, 0x27, 0x41, 0x4c, 0x83, 0x54, 0xfc, 0x61, 0x30, 0x2f, 0x43,
                    0xb8, 0xce, 0xdc, 0xdf, 0xae, 0xee, 0xb6, 0xe0, 0x48, 0xfe, 0x92, 0x3, 0x32,
                    0x44, 0x97, 0xfb, 0xd3, 0xa6, 0x0, 0x76, 0x0, 0xef, 0x50, 0x2e, 0x32, 0x7f,
                    0x5c, 0x8f, 0xaf, 0xb5, 0x59, 0xdd, 0x60, 0xa3, 0x54, 0xbc, 0x16, 0xe3, 0x15,
                    0xd8, 0x14, 0xa2, 0x13, 0x7e, 0xe, 0xb6, 0x6b, 0x5b, 0xf1, 0x97, 0xa3, 0x52,
                    0x16, 0xa6, 0x3f, 0x9b, 0xd4, 0x70, 0x9e, 0xec, 0x3a, 0x7b, 0xf4, 0x30, 0x28,
                    0x8b, 0x71, 0x93, 0x29, 0x6, 0xda, 0xc1, 0x18, 0x40, 0xf, 0xf7, 0xd2, 0x19,
                    0x3c, 0x76, 0x32, 0x38, 0x66, 0xe6, 0x78, 0x19, 0x76, 0x5b, 0x99, 0x2, 0xeb,
                    0x6b, 0xbc, 0x61, 0x37, 0xd4, 0x42, 0x3d, 0x74, 0x74, 0xf3, 0xca, 0xf9, 0x38,
                    0xb6, 0x9f, 0x8b, 0xfb, 0xea, 0x3b, 0x18, 0x2e, 0x0, 0x58, 0x71, 0x3, 0xd0,
                    0xa6, 0xaf, 0xe1, 0x66, 0x64, 0x17, 0x73, 0xeb, 0xc9, 0x38, 0x4c, 0xa, 0xf6,
                    0xaf, 0x7a, 0x9b, 0xe, 0xbe, 0x52, 0x92, 0x8a, 0xf0, 0x7c, 0x82, 0x70, 0xe,
                    0xbe, 0xe3, 0x65, 0xe0, 0xbc, 0x95, 0xdf, 0x3c, 0xe8, 0x13, 0x38, 0xf4, 0x41,
                    0xb0, 0x29, 0xb9, 0xdd, 0x8a, 0xb, 0x4c, 0xc6, 0x0, 0xd, 0x20, 0x76, 0xd9,
                    0xaa, 0x82, 0x14, 0xb9, 0xfa, 0x34, 0x23, 0x83, 0xb8, 0xd2, 0xb3, 0x97, 0xc1,
                    0x26, 0x44, 0x3a, 0x22, 0x55, 0xe9, 0x7f, 0x4c, 0x3f, 0xf5, 0xac, 0xf1, 0xd2,
                    0x95, 0x94, 0xa7, 0x2a, 0x33, 0x20, 0x53, 0xcc, 0xac, 0xd6, 0xd6, 0x89, 0x84,
                    0xed, 0xcf, 0xc9, 0x6f, 0x85, 0x2a, 0x14, 0x42, 0x3, 0x74, 0x9, 0xd3, 0xd3,
                    0xb, 0xfb, 0x6, 0xf3, 0xcb, 0x37, 0x41, 0xc3, 0x13, 0xd6, 0xca, 0x9b, 0x53,
                    0x17, 0x22, 0xfd, 0x52, 0xdf, 0x28, 0x9e, 0x13, 0xd8, 0xfd, 0x95, 0x3b, 0xb1,
                    0x5a, 0xc8, 0x14, 0x23, 0xb, 0x4b, 0xf, 0x22, 0x85, 0xe7, 0x1c, 0x3b, 0xbc,
                    0xd3,
                ],
                negotiated_protocol_version: Some(ProtocolVersion::TLSv1_3),
                pcap: "wireshark_macos_firefox_133_ramaproxy.org.pcap",
                expected_ja4_str: "t13d1716h2_002f,0035,009c,009d,1301,1302,1303,c009,c00a,c013,c014,c02b,c02c,c02f,c030,cca8,cca9_0005,000a,000b,000d,0017,001b,001c,0022,0023,002b,002d,0033,fe0d,ff01_0403,0503,0603,0804,0805,0806,0401,0501,0601,0203,0201",
                expected_ja4_hash: "t13d1716h2_5b57614c22b0_eeeea6562960",
            },
        ];
        for test_case in test_cases {
            let mut ext = Extensions::new();
            ext.insert(SecureTransport::with_client_hello(
                parse_client_hello(&test_case.client_hello).expect(test_case.pcap),
            ));
            if let Some(negotiated_protocol_version) = test_case.negotiated_protocol_version {
                ext.insert(NegotiatedTlsParameters {
                    protocol_version: negotiated_protocol_version,
                    application_layer_protocol: None,
                    peer_certificate_chain: None,
                });
            }

            let ja4 = Ja4::compute(&ext).expect(test_case.pcap);

            assert_eq!(
                test_case.expected_ja4_str,
                format!("{ja4:?}"),
                "pcap: {}",
                test_case.pcap,
            );

            assert_eq!(
                test_case.expected_ja4_hash,
                format!("{ja4}"),
                "pcap: {}",
                test_case.pcap,
            );
        }
    }
}

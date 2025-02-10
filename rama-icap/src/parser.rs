use bytes::{Buf, BytesMut, Bytes};
use rama_http_types::HeaderMap;
use std::collections::HashMap;
use std::default::Default;

use crate::{Error, IcapMessage, Method, Result, SectionType, State, Version, Encapsulated, Request, Response};

const MAX_HEADERS: usize = 100;
const MAX_HEADER_NAME_LEN: usize = 100;
const MAX_HEADER_VALUE_LEN: usize = 4096;

pub struct ByteParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteParser<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    #[inline]
    pub fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    #[inline]
    pub fn advance(&mut self) {
        self.pos += 1;
    }

    #[inline]
    pub fn slice(&self) -> &'a [u8] {
        &self.bytes[..self.pos]
    }

    #[inline]
    pub fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.pos..]
    }

    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }
}

pub struct MessageParser {
    state: State,
    headers: HeaderMap,
    encapsulated: HashMap<SectionType, Vec<u8>>,
    buffer: BytesMut,
    method: Option<Method>,
    uri: Option<String>,
    version: Option<Version>,
    status: Option<u16>,
    reason: Option<String>,
    sections: Vec<(SectionType, usize)>,
}

impl MessageParser {
    pub fn new() -> Self {
        Self {
            state: State::StartLine,
            headers: HeaderMap::new(),
            encapsulated: HashMap::new(),
            buffer: BytesMut::with_capacity(4096),
            method: None,
            uri: None,
            version: None,
            status: None,
            reason: None,
            sections: Vec::new(),
        }
    }

    pub fn parse(&mut self, buf: &[u8]) -> Result<Option<IcapMessage>> {
        self.buffer.extend_from_slice(buf);

        loop {
            match self.state {
                State::StartLine => {
                    if !self.parse_start_line()? {
                        return Ok(None);
                    }
                    self.state = State::Headers;
                }
                State::Headers => {
                    if !self.parse_headers()? {
                        return Ok(None);
                    }
                    self.state = State::EncapsulatedHeader;
                }
                State::EncapsulatedHeader => {
                    if !self.parse_encapsulated()? {
                        return Ok(None);
                    }
                    self.state = State::Body;
                }
                State::Body => {
                    if !self.parse_body()? {
                        return Ok(None);
                    }
                    self.state = State::Complete;
                }
                State::Complete => {
                    let message = self.build_message()?;
                    self.state = State::StartLine;
                    self.headers.clear();
                    self.encapsulated.clear();
                    self.buffer.clear();
                    return Ok(Some(message));
                }
            }
        }
    }

    fn parse_start_line(&mut self) -> Result<bool> {
        if let Some(line) = self.read_line()? {
            if line.is_empty() {
                return Ok(false);
            }

            // Parse line
            let parts: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
            if parts.len() != 3 {
                return Err(Error::InvalidMethod("Incomplete message received".to_string()));
            }

            // Check if this is a response (starts with ICAP/)
            if let Some(version) = self.parse_version(parts[0])? {
                self.version = Some(version);
                // Parse status code
                let status = std::str::from_utf8(parts[1])
                    .map_err(|_| Error::InvalidStatus)?
                    .parse::<u16>()
                    .map_err(|_| Error::InvalidStatus)?;
                self.status = Some(status);
                // Parse reason
                let reason = std::str::from_utf8(parts[2])
                    .map_err(|_| Error::InvalidFormat("Invalid reason".to_string()))?
                    .to_string();
                self.reason = Some(reason);
            } else {
                // This is a request
                // Parse method
                let method = match parts[0] {
                    b"REQMOD" => Method::ReqMod,
                    b"RESPMOD" => Method::RespMod,
                    b"OPTIONS" => Method::Options,
                    _ => return Err(Error::InvalidMethod("Invalid method".to_string())),
                };
                self.method = Some(method);

                // Parse URI
                let uri = std::str::from_utf8(parts[1])
                    .map_err(|_| Error::InvalidFormat("Invalid URI".to_string()))?
                    .to_string();
                self.uri = Some(uri);

                // Parse version
                if let Some(version) = self.parse_version(parts[2])? {
                    self.version = Some(version);
                } else {
                    return Err(Error::InvalidVersion("Invalid version".to_string()));
                }
            }

            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn parse_version(&self, bytes: &[u8]) -> Result<Option<Version>> {
        if bytes.len() < 8 {
            return Ok(None);
        }
        match bytes {
            [b'I', b'C', b'A', b'P', b'/', b'1', b'.', b'0', ..] => Ok(Some(Version::V1_0)),
            [b'I', b'C', b'A', b'P', b'/', b'1', b'.', b'1', ..] => Ok(Some(Version::V1_1)),
            [b'I', b'C', b'A', b'P', b'/', ..] => Err(Error::InvalidVersion("Invalid version".to_string())),
            _ => Ok(None),
        }
    }

    fn parse_headers(&mut self) -> Result<bool> {
        let mut found_encapsulated = false;
        
        while let Some(line) = self.read_line()? {
            // Empty line indicates end of headers
            if line.is_empty() {
                // Check if Encapsulated header is required
                if !found_encapsulated {
                    // For responses, Encapsulated header is not required if there's no body
                    if self.status.is_some() {
                        // This is a response
                        self.state = State::EncapsulatedHeader;
                        return Ok(true);
                    } else if let Some(method) = &self.method {
                        // This is a request
                        match method {
                            Method::Options => {}, // Encapsulated header is optional for OPTIONS
                            _ => return Err(Error::Protocol("Missing encapsulated header".to_string()))
                        }
                    }
                }
                self.state = State::EncapsulatedHeader;
                return Ok(true);
            }
            
            // byte to string
            let test = String::from_utf8_lossy(&line);
            println!("test: {:?}", test);

            // Split into name and value
            let mut parts = line.splitn(2, |&b| b == b':');
            let name = parts.next().ok_or_else(|| Error::InvalidFormat("Missing header name".to_string()))?;
            let value = parts.next().ok_or_else(|| Error::InvalidFormat("Missing header value".to_string()))?;

            // Validate lengths
            if name.len() > MAX_HEADER_NAME_LEN {
                return Err(Error::Protocol("Message too large".to_string()));
            }
            if value.len() > MAX_HEADER_VALUE_LEN {
                return Err(Error::Protocol("Message too large".to_string()));
            }

            // Convert to strings and add to headers
            let name = rama_http_types::HeaderName::from_bytes(name)?;
            let value = String::from_utf8_lossy(value).trim().to_string();
            
            // Check for Encapsulated header
            if name.as_str().eq_ignore_ascii_case("encapsulated") {
                found_encapsulated = true;
            }
            
            self.headers.insert(name, value.parse()?);

            if self.headers.len() > MAX_HEADERS {
                return Err(Error::Protocol("Message too large".to_string()));
            }
        }

        Ok(false)
    }

    fn parse_encapsulated(&mut self) -> Result<bool> {
        // Get the Encapsulated header
        if let Some(enc) = self.headers.get("Encapsulated") {
            let enc = enc.to_str().map_err(|_| Error::Protocol("Invalid encoding".to_string()))?;
            
            // Parse each section's offset
            let mut sections = Vec::new();
            for section in enc.split(',') {
                let mut parts = section.trim().split('=');
                let name = parts.next()
                    .ok_or_else(|| Error::Protocol("Missing header name".to_string()))?
                    .trim()
                    .to_lowercase();
                
                let offset = parts.next()
                    .ok_or_else(|| Error::Protocol("Missing header value".to_string()))?
                    .parse::<usize>()
                    .map_err(|_| Error::Protocol("Invalid header value offset".to_string()))?;
                
                let section_type = match name.as_str() {
                    "null-body" => SectionType::NullBody,
                    "req-hdr" => SectionType::RequestHeader,
                    "req-body" => SectionType::RequestBody,
                    "res-hdr" => SectionType::ResponseHeader,
                    "res-body" => SectionType::ResponseBody,
                    "opt-body" => SectionType::OptionsBody,
                    _ => return Err(Error::Protocol("Invalid encapsulated header".to_string())),
                };
                
                sections.push((section_type, offset));
            }
            
            // Sort sections by offset
            sections.sort_by_key(|(_, offset)| *offset);
            
            // Initialize encapsulated map with empty vectors for each section
            for (section_type, _) in sections.clone() {
                self.encapsulated.insert(section_type, Vec::new());
            }
            
            // Store the sorted sections for later use in parse_body
            self.sections = sections;
        }
        
        self.state = State::Body;
        Ok(true)
    }

    /// Parse the body of an ICAP message which may contain multiple sections.
    /// According to RFC 3507, an ICAP message can have different combinations of sections:
    /// 
    /// # Examples
    /// 
    /// REQMOD request with both headers and body:
    /// ```text
    /// REQMOD icap://server/path ICAP/1.0
    /// Encapsulated: req-hdr=0, req-body=123
    /// 
    /// GET /path HTTP/1.1     <- req-hdr starts at offset 0
    /// Host: example.com
    /// 
    /// a3                     <- req-body starts at offset 123
    /// Hello World!           <- chunk data
    /// 0                     <- end of chunked data
    /// ```
    /// 
    /// RESPMOD request with request headers, response headers and body:
    /// ```text
    /// RESPMOD icap://icap.example.org/satisf ICAP/1.0
    /// Encapsulated: req-hdr=0, res-hdr=137, res-body=200
    /// 
    /// GET /origin-resource HTTP/1.1    <- req-hdr starts at offset 0
    /// Host: www.origin-server.com
    /// Accept: text/html, text/plain, image/gif
    /// Accept-Encoding: gzip, compress
    /// 
    /// HTTP/1.1 200 OK       <- res-hdr starts at offset 137
    /// Date: Mon, 10 Jan 2000 09:52:22 GMT
    /// Server: Apache/1.3.6 (Unix)
    /// ETag: \"63840-1ab7-378d415b\"
    /// Content-Type: text/html
    /// Content-Length: 51
    /// 
    /// 33                    <- res-body starts at offset 200
    /// This is data that was returned by an origin server.
    /// 0                     <- end of chunked data
    /// ```
    fn parse_body(&mut self) -> Result<bool> {
        // Process each section in order
        for i in 0..self.sections.len() {
            let (section_type, start_offset) = self.sections[i].clone();
    
            // Calculate end offset based on next section or buffer length
            let end_offset = if i < self.sections.len() - 1 {
                self.sections[i + 1].1  // Next section's offset
            } else {
                self.buffer.len()  // Use remaining buffer for last section
            };
            println!("end_offset: {}", end_offset);
            
            // Skip if we don't have enough data
            if self.buffer.len() < start_offset {
                return Ok(false);
            }
            
            // Extract and process section
            if start_offset < self.buffer.len() {
                let section_data = if section_type == SectionType::RequestBody || section_type == SectionType::ResponseBody {
                    // For body sections, we need to handle chunked encoding
                    let mut chunk_data = Vec::new();
                    let mut pos = start_offset;
                    
                    while pos < end_offset {
                        // Try to read chunk size
                        let mut size_str = String::new();
                        while pos < end_offset {
                            let byte = self.buffer[pos];
                            pos += 1;
                            if byte == b'\r' && pos < end_offset && self.buffer[pos] == b'\n' {
                                pos += 1;
                                break;
                            }
                            size_str.push(byte as char);
                        }
                        
                        // Parse chunk size (hex)
                        let chunk_size = match usize::from_str_radix(size_str.trim(), 16) {
                            Ok(size) => size,
                            Err(_) => return Err(Error::Protocol("Invalid chunk size".to_string())),
                        };
                        
                        // Last chunk
                        if chunk_size == 0 {
                            break;
                        }
                        
                        // Check if we have enough data for this chunk
                        if pos + chunk_size + 2 > end_offset {
                            return Ok(false);
                        }
                        
                        // Add chunk data
                        chunk_data.extend_from_slice(&self.buffer[pos..pos + chunk_size]);
                        pos += chunk_size;
                        
                        // Skip CRLF
                        if pos + 2 <= end_offset && self.buffer[pos] == b'\r' && self.buffer[pos + 1] == b'\n' {
                            pos += 2;
                        } else {
                            return Err(Error::Protocol("Invalid chunk encoding".to_string()));
                        }
                    }
                    
                    chunk_data
                } else {
                    // For headers and other sections, just copy the data
                    self.buffer[start_offset..std::cmp::min(end_offset, self.buffer.len())].to_vec()
                };
                
                if let Some(data) = self.encapsulated.get_mut(&section_type) {
                    *data = section_data;
                }
            }
        }
        
        self.state = State::Complete;
        Ok(true)
    }

    fn read_line(&mut self) -> Result<Option<Vec<u8>>> {
        let mut line = Vec::new();
        let mut found_line = false;

        println!("self.buffer: {:?}\nTESTEND", String::from_utf8_lossy(self.buffer.as_ref()));
    
        for (i, &b) in self.buffer.iter().enumerate() {
            if b == b'\n' {
                line.extend_from_slice(&self.buffer[..i]);
                if line.ends_with(b"\r") {
                    line.pop();
                }
                self.buffer.advance(i + 1);
                found_line = true;
                break;
            }
        }

        if found_line {
            Ok(Some(line))
        } else {
            Ok(None)
        }
    }

    fn build_encapsulated(&self) -> Result<Encapsulated> {
        let has_request_header = self.encapsulated.contains_key(&SectionType::RequestHeader);
        let has_request_body = self.encapsulated.contains_key(&SectionType::RequestBody);
        let has_response_header = self.encapsulated.contains_key(&SectionType::ResponseHeader);
        let has_response_body = self.encapsulated.contains_key(&SectionType::ResponseBody);
        let has_options_body = self.encapsulated.contains_key(&SectionType::OptionsBody);
        let has_null_body = self.encapsulated.contains_key(&SectionType::NullBody);

        match (has_request_header, has_request_body, has_response_header, has_response_body, has_options_body, has_null_body) {
            (_, _, _, _, _, true) => Ok(Encapsulated::NullBody),
            (_, _, _, _, true, _) => Ok(Encapsulated::Options {
                body: self.encapsulated.get(&SectionType::OptionsBody)
                    .map(|v| Bytes::from(v.to_vec())),
            }),
            (true, _, true, _, _, _) | (_, true, true, _,  _, _) | 
            (true, _, _, true, _, _) | (_, true, _, true, _, _) => Ok(Encapsulated::RequestResponse {
                req_header: self.encapsulated.get(&SectionType::RequestHeader)
                    .map(|_| Request::default()),
                req_body: self.encapsulated.get(&SectionType::RequestBody)
                    .map(|v| Bytes::from(v.to_vec())),
                res_header: self.encapsulated.get(&SectionType::ResponseHeader)
                    .map(|_| Response::default()),
                res_body: self.encapsulated.get(&SectionType::ResponseBody)
                    .map(|v| Bytes::from(v.to_vec())),
            }),
            (true, _, _, _, _, _) | (_, true, _, _, _, _) => Ok(Encapsulated::RequestOnly {
                header: self.encapsulated.get(&SectionType::RequestHeader)
                    .map(|_| Request::default()),
                body: self.encapsulated.get(&SectionType::RequestBody)
                    .map(|v| Bytes::from(v.to_vec())),
            }),
            (_, _, true, _, _, _) | (_, _, _, true, _, _) => Ok(Encapsulated::ResponseOnly {
                header: self.encapsulated.get(&SectionType::ResponseHeader)
                    .map(|_| Response::default()),
                body: self.encapsulated.get(&SectionType::ResponseBody)
                    .map(|v| Bytes::from(v.to_vec())),
            }),
            _ => Ok(Encapsulated::NullBody),
        }
    }

    fn build_message(&self) -> Result<IcapMessage> {
        match (self.method.as_ref(), self.status.as_ref()) {
            (Some(method), None) => {
                // Build request
                Ok(IcapMessage::Request {
                    method: method.clone(),
                    uri: self.uri.clone().unwrap(),
                    version: self.version.unwrap(),
                    headers: self.headers.clone(),
                    encapsulated: self.build_encapsulated()?,
                })
            }
            (None, Some(status)) => {
                // Build response
                Ok(IcapMessage::Response {
                    version: self.version.unwrap(),
                    status: *status,
                    reason: self.reason.clone().unwrap_or_default(),
                    headers: self.headers.clone(),
                    encapsulated: self.build_encapsulated()?,
                })
            }
            _ => Err(Error::Protocol("Invalid message".to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_byte_parser() {
        let data = b"ICAP/1.0 200 OK\r\n";
        let mut parser = ByteParser::new(data);

        assert_eq!(parser.peek(), Some(b'I'));
        parser.advance();
        assert_eq!(parser.peek(), Some(b'C'));
        
        assert_eq!(parser.slice(), b"I");
        assert_eq!(parser.remaining(), b"CAP/1.0 200 OK\r\n");
        assert_eq!(parser.position(), 1);
    }

    #[test]
    fn test_parse_request_line() {
        let mut parser = MessageParser::new();
        let data = b"REQMOD icap://example.org/modify ICAP/1.0\r\n";
        
        let result = parser.parse(data).unwrap();
        assert!(result.is_none()); // Need more data for complete message
        
        match parser.state {
            State::Headers => {},
            _ => panic!("Expected Headers state"),
        }
    }

    #[test]
    fn test_parse_headers() {
        let mut parser = MessageParser::new();
        let data = b"REQMOD icap://example.org/modify ICAP/1.0\r\n\
                    Host: example.org\r\n\
                    Connection: close\r\n\
                    Encapsulated: req-hdr=0\r\n";  // No final \r\n and no HTTP message
        
        let result = parser.parse(data).unwrap();
        assert!(result.is_none()); // Need more data for complete message
        
        match parser.state {
            State::Headers => {},  // Still in Headers state because we haven't seen the final \r\n
            _ => panic!("Expected Headers state"),
        }
    }

    #[test]
    fn test_parse_encapsulated() {
        let mut parser = MessageParser::new();
        let data = b"RESPMOD icap://icap.example.org/satisf ICAP/1.0\r\n\
                    Host: icap.example.org\r\n\
                    Encapsulated: req-hdr=0, res-hdr=137, res-body=296\r\n\r\n\
                    GET /origin-resource HTTP/1.1\r\n\
                    Host: www.origin-server.com\r\n\
                    Accept: text/html, text/plain, image/gif\r\n\
                    Accept-Encoding: gzip, compress\r\n\r\n\
                    HTTP/1.1 200 OK\r\n\
                    Date: Mon, 10 Jan 2000 09:52:22 GMT\r\n\
                    Server: Apache/1.3.6 (Unix)\r\n\
                    ETag: \"63840-1ab7-378d415b\"\r\n\
                    Content-Type: text/html\r\n\
                    Content-Length: 51\r\n\r\n\
                    33\r\n\
                    This is data that was returned by an origin server.\r\n\
                    0\r\n\r\n";

        let result = parser.parse(data).unwrap().unwrap();
        match result {
            IcapMessage::Request { method, uri, headers, encapsulated, .. } => {
                assert_eq!(method, Method::RespMod);
                assert_eq!(uri, "icap://icap.example.org/satisf");
                assert_eq!(headers.get("Host").unwrap(), "icap.example.org");
                assert!(encapsulated.contains(&SectionType::RequestHeader));
                assert!(encapsulated.contains(&SectionType::ResponseHeader));
            }
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_parse_response() {
        let mut parser = MessageParser::new();
        let data = b"ICAP/1.0 200 OK\r\n\
                    Server: IcapServer/1.0\r\n\
                    Connection: close\r\n\
                    Encapsulated: null-body=0\r\n\r\n";
        
        let result = parser.parse(data).unwrap().unwrap();
        match result {
            IcapMessage::Response { version, status, reason, headers, .. } => {
                assert_eq!(version, Version::V1_0);
                assert_eq!(status, 200);
                assert_eq!(reason, "OK");
                assert_eq!(headers.get("Server").unwrap(), "IcapServer/1.0");
            } 
            _ => panic!("Expected Response"),
        }
    }

    #[test]
    fn test_read_line() {
        let mut parser = MessageParser::new();
        parser.buffer.extend_from_slice(b"line1\r\nline2\r\n");
        
        let line1 = parser.read_line().unwrap().unwrap();
        assert_eq!(line1, b"line1");
        
        let line2 = parser.read_line().unwrap().unwrap();
        assert_eq!(line2, b"line2");
        
        assert!(parser.read_line().unwrap().is_none());
    }

    #[test]
    fn test_parse_error_cases() {
        let mut parser = MessageParser::new();
        
        // Invalid method
        let data = b"INVALID icap://example.org/modify ICAP/1.0\r\n\r\n";
        assert!(parser.parse(data).is_err());
        
        // Reset parser
        parser = MessageParser::new();
        
        // Invalid version
        let data = b"REQMOD icap://example.org/modify ICAP/2.0\r\n\r\n";
        assert!(parser.parse(data).is_err());
        
        // Reset parser
        parser = MessageParser::new();
        
        // Missing Encapsulated header for REQMOD
        let data = b"REQMOD icap://example.org/modify ICAP/1.0\r\n\
                    Host: example.org\r\n\r\n";
        assert!(parser.parse(data).is_err());
        
        // Reset parser
        parser = MessageParser::new();
        
        // Missing Encapsulated header for RESPMOD
        let data = b"RESPMOD icap://example.org/modify ICAP/1.0\r\n\
                    Host: example.org\r\n\r\n";
        assert!(parser.parse(data).is_err());
        
        // Reset parser
        parser = MessageParser::new();
        
        // OPTIONS request without Encapsulated header should be OK
        let data = b"OPTIONS icap://example.org/modify ICAP/1.0\r\n\
                    Host: example.org\r\n\r\n";
        assert!(parser.parse(data).is_ok());
        
        // Reset parser
        parser = MessageParser::new();
        
        // Response without Encapsulated header should be OK
        let data = b"ICAP/1.0 200 OK\r\n\
                    Server: test-server/1.0\r\n\r\n";
        assert!(parser.parse(data).is_ok());
    }
}

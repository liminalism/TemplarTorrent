//! What a router may send back to the port mapper: NAT-PMP replies, SSDP
//! answers and their LOCATION URLs, device descriptions, SOAP responses and
//! the route table text, all from the same bytes.
#![no_main]
use libfuzzer_sys::fuzz_target;
use std::net::Ipv4Addr;
use templartorrent_rbit_adapter::portmap::{
    HttpUrl, MAX_SSDP_BYTES, NatPmpError, NatPmpReply, SoapReply, admit_location, control_target,
    parse_description, parse_http_url, parse_natpmp_reply, parse_route_table, parse_soap_response,
    parse_ssdp_response, soap_element,
};

const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 1);

fuzz_target!(|data: &[u8]| {
    match parse_natpmp_reply(data) {
        Ok(NatPmpReply::External { .. }) => assert!(data.len() >= 12 && data[1] == 128),
        Ok(NatPmpReply::Mapping { .. }) => assert!(data.len() >= 16),
        Err(NatPmpError::Refused(code)) => assert!(code != 0),
        Err(NatPmpError::Malformed(_)) => {}
    }
    if let Ok(answer) = parse_ssdp_response(data) {
        assert!(data.len() <= MAX_SSDP_BYTES);
        assert!(answer.location.len() <= 256 && !answer.location.chars().any(char::is_control));
        assert!(answer.server.chars().count() <= 120);
        if let Ok(url) = admit_location(&answer.location, GATEWAY) {
            assert_eq!(url.host, GATEWAY);
        }
    }
    let text = String::from_utf8_lossy(data);
    if let Ok(url) = parse_http_url(&text) {
        assert!(url.port != 0 && url.path.starts_with('/'));
        assert!(!url.path.bytes().any(|b| b <= b' ' || b == b'"'));
    }
    let location = HttpUrl {
        host: GATEWAY,
        port: 5000,
        path: "/rootDesc.xml".into(),
    };
    if let Ok(url) = control_target(&text, &location, GATEWAY) {
        assert_eq!(url.host, GATEWAY);
        assert!(url.path.starts_with('/') && url.path.len() <= 512);
    }
    if let Ok(services) = parse_description(data) {
        assert!(services.len() <= 4);
        for service in services {
            assert!(
                service
                    .service_type
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'.'))
            );
            assert!(!service.control_url.is_empty());
        }
    }
    if let Some(value) = soap_element(&text, "NewExternalIPAddress") {
        assert!(value.len() <= 2048);
    }
    match parse_soap_response(data) {
        // Lossy decoding turns one invalid byte into a 3-byte U+FFFD, so
        // only the character count is bounded by the input length.
        Ok(SoapReply::Success(body)) => assert!(body.chars().count() <= data.len()),
        Ok(SoapReply::Fault(_)) | Err(_) => {}
    }
    let _ = parse_route_table(&text);
});

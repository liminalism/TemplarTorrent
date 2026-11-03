//! Loopback fixtures only: a NAT-PMP responder, a unicast "SSDP" responder
//! and an HTTP/SOAP server, all on 127.0.0.x. No real router is contacted.
use super::*;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

fn fixture_config() -> Config {
    Config {
        gateway: Some(LOOPBACK),
        natpmp_initial: Duration::from_millis(40),
        natpmp_tries: 3,
        ssdp_wait: Duration::from_millis(600),
        http_timeout: Duration::from_secs(2),
        min_renew: Duration::ZERO,
        retry_after: Duration::from_secs(3600),
        ..Config::default()
    }
}

async fn wait_for(mapper: &PortMapper, done: impl Fn(&Status) -> bool) -> Status {
    let start = Instant::now();
    loop {
        let status = mapper.status();
        if done(&status) {
            return status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "status stuck at {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn settled(status: &Status) -> bool {
    !matches!(status, Status::Probing)
}

// ---------------------------------------------------------------------------
// NAT-PMP fixture

type Answer = Arc<dyn Fn(&[u8]) -> Option<Vec<u8>> + Send + Sync>;

struct NatPmpFixture {
    port: u16,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    task: JoinHandle<()>,
}

impl Drop for NatPmpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `spoof` replies first from 127.0.0.2 and from another loopback port,
/// each carrying 192.0.2.66 as the external address.
async fn natpmp_fixture(answer: Answer, spoof: bool) -> NatPmpFixture {
    let socket = UdpSocket::bind((LOOPBACK, 0)).await.unwrap();
    let port = socket.local_addr().unwrap().port();
    let other_host = UdpSocket::bind(("127.0.0.2", 0)).await.unwrap();
    let other_port = UdpSocket::bind((LOOPBACK, 0)).await.unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        let mut buffer = [0u8; 64];
        while let Ok((n, from)) = socket.recv_from(&mut buffer).await {
            let request = buffer[..n].to_vec();
            seen.lock().unwrap().push(request.clone());
            if spoof {
                let fake = external_reply(Ipv4Addr::new(192, 0, 2, 66));
                let mut fake_map = answer(&request).unwrap_or_default();
                if fake_map.len() >= 12 {
                    fake_map[10..12].copy_from_slice(&1u16.to_be_bytes());
                }
                for spoofer in [&other_host, &other_port] {
                    let reply = if request[1] == 0 { &fake } else { &fake_map };
                    let _ = spoofer.send_to(reply, from).await;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if let Some(reply) = answer(&request) {
                let _ = socket.send_to(&reply, from).await;
            }
        }
    });
    NatPmpFixture {
        port,
        requests,
        task,
    }
}

fn external_reply(address: Ipv4Addr) -> Vec<u8> {
    let mut out = vec![0, 128, 0, 0, 0, 0, 0, 9];
    out.extend_from_slice(&address.octets());
    out
}

fn mapping_reply(request: &[u8], result: u16, lifetime: u32) -> Vec<u8> {
    let mut out = vec![0, 128 + request[1]];
    out.extend_from_slice(&result.to_be_bytes());
    out.extend_from_slice(&9u32.to_be_bytes());
    out.extend_from_slice(&request[4..6]);
    let requested = u16::from_be_bytes([request[6], request[7]]);
    let internal = u16::from_be_bytes([request[4], request[5]]);
    let external = if requested == 0 { internal } else { requested };
    out.extend_from_slice(&external.to_be_bytes());
    let asked = u32::from_be_bytes([request[8], request[9], request[10], request[11]]);
    out.extend_from_slice(&(if asked == 0 { 0 } else { lifetime }).to_be_bytes());
    out
}

/// A cooperative router granting `lifetime` seconds.
fn router(lifetime: u32) -> Answer {
    Arc::new(move |request: &[u8]| match request {
        [0, 0] => Some(external_reply(Ipv4Addr::new(203, 0, 113, 7))),
        [0, 1 | 2, ..] if request.len() == 12 => Some(mapping_reply(request, 0, lifetime)),
        _ => None,
    })
}

fn natpmp_only(port: u16) -> Config {
    Config {
        natpmp_port: port,
        upnp: false,
        ..fixture_config()
    }
}

/// (opcode, internal port, lifetime) of each mapping request.
fn mapping_requests(fixture: &NatPmpFixture) -> Vec<(u8, u16, u32)> {
    fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.len() == 12)
        .map(|r| {
            (
                r[1],
                u16::from_be_bytes([r[4], r[5]]),
                u32::from_be_bytes([r[8], r[9], r[10], r[11]]),
            )
        })
        .collect()
}

#[tokio::test]
async fn natpmp_maps_tcp_and_udp_then_deletes_on_stop() {
    let fixture = natpmp_fixture(router(7200), false).await;
    let listen: SocketAddr = "127.0.0.1:40111".parse().unwrap();
    let mapper = PortMapper::start(natpmp_only(fixture.port), listen);
    let status = wait_for(&mapper, settled).await;
    let Status::Mapped {
        method,
        port,
        external,
        expires,
    } = status.clone()
    else {
        panic!("{status:?}");
    };
    assert_eq!(method, Method::NatPmp);
    assert_eq!(port, 40111);
    assert_eq!(
        external,
        Some(SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 40111))
    );
    assert!(expires.is_some_and(|at| at > Instant::now() + Duration::from_secs(7000)));
    let text = status.describe();
    assert!(text.contains("NAT-PMP") && text.contains("40111"), "{text}");
    assert!(
        !text.contains("203.0.113"),
        "describe leaked the address: {text}"
    );
    assert_eq!(
        mapping_requests(&fixture),
        vec![(2, 40111, NATPMP_LIFETIME_S), (1, 40111, NATPMP_LIFETIME_S)]
    );
    let start = Instant::now();
    mapper.stop(STOP_BOUND).await;
    assert!(start.elapsed() < Duration::from_secs(2));
    let requests = mapping_requests(&fixture);
    assert_eq!(&requests[2..], &[(2, 40111, 0), (1, 40111, 0)]);
    // Deletes carry a zero suggested external port.
    let raw = fixture.requests.lock().unwrap();
    assert!(raw.iter().rev().take(2).all(|r| r[6..8] == [0, 0]));
}

#[tokio::test]
async fn natpmp_result_code_fails_with_its_reason() {
    let answer: Answer = Arc::new(|request: &[u8]| match request {
        [0, 0] => Some(external_reply(Ipv4Addr::new(203, 0, 113, 7))),
        [0, 1 | 2, ..] => Some(mapping_reply(request, 2, 0)),
        _ => None,
    });
    let fixture = natpmp_fixture(answer, false).await;
    let mapper = PortMapper::start(
        natpmp_only(fixture.port),
        "127.0.0.1:40112".parse().unwrap(),
    );
    let status = wait_for(&mapper, settled).await;
    assert_eq!(
        status,
        Status::Failed("NAT-PMP: router refused the mapping (not authorized)".into())
    );
    // Only TCP was asked; nothing was granted, so nothing is deleted.
    assert_eq!(
        mapping_requests(&fixture),
        vec![(2, 40112, NATPMP_LIFETIME_S)]
    );
    // A truncated error reply (header only) still yields its code.
    assert_eq!(
        parse_natpmp_reply(&[0, 128, 0, 3]),
        Err(NatPmpError::Refused(3))
    );
}

#[tokio::test]
async fn natpmp_ignores_replies_from_other_sources() {
    let fixture = natpmp_fixture(router(7200), true).await;
    let mapper = PortMapper::start(
        natpmp_only(fixture.port),
        "127.0.0.1:40113".parse().unwrap(),
    );
    let status = wait_for(&mapper, settled).await;
    assert_eq!(
        status,
        Status::Mapped {
            method: Method::NatPmp,
            port: 40113,
            external: Some(SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 40113)),
            expires: match &status {
                Status::Mapped { expires, .. } => *expires,
                _ => None,
            },
        }
    );
    mapper.stop(STOP_BOUND).await;
}

#[tokio::test]
async fn natpmp_silence_times_out_on_the_capped_schedule() {
    let fixture = natpmp_fixture(Arc::new(|_: &[u8]| None), false).await;
    let start = Instant::now();
    let mapper = PortMapper::start(
        natpmp_only(fixture.port),
        "127.0.0.1:40114".parse().unwrap(),
    );
    let status = wait_for(&mapper, settled).await;
    // 40 + 80 + 160 ms.
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(
        status,
        Status::Failed("NAT-PMP: no NAT-PMP reply from the gateway".into())
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
    // The default schedule stays under four seconds per request.
    let config = Config::default();
    let total: Duration = (0..config.natpmp_tries)
        .map(|i| config.natpmp_initial * 2u32.pow(i))
        .sum();
    assert!(total < Duration::from_secs(4));
}

#[tokio::test]
async fn natpmp_renews_at_half_the_granted_lifetime() {
    let fixture = natpmp_fixture(router(2), false).await;
    let mapper = PortMapper::start(
        natpmp_only(fixture.port),
        "127.0.0.1:40115".parse().unwrap(),
    );
    wait_for(&mapper, |s| matches!(s, Status::Mapped { .. })).await;
    let start = Instant::now();
    loop {
        let tcp = mapping_requests(&fixture)
            .iter()
            .filter(|(op, _, lifetime)| *op == 2 && *lifetime != 0)
            .count();
        if tcp >= 2 {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "no renewal");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Renewed after about one second (half of 2 s), not immediately.
    assert!(
        start.elapsed() >= Duration::from_millis(700),
        "{:?}",
        start.elapsed()
    );
    wait_for(&mapper, |s| matches!(s, Status::Mapped { .. })).await;
    mapper.stop(STOP_BOUND).await;
}

#[tokio::test]
async fn driver_shutdown_removes_the_mapping() {
    let fixture = natpmp_fixture(router(7200), false).await;
    let mut driver = crate::driver::Driver::new(Default::default()).unwrap();
    assert!(driver.enable_port_mapping().is_err(), "no listener yet");
    let bound = driver.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    driver.set_port_mapping_config(natpmp_only(fixture.port));
    assert_eq!(driver.port_mapping_status(), Status::Off);
    driver.enable_port_mapping().unwrap();
    let start = Instant::now();
    while !matches!(driver.port_mapping_status(), Status::Mapped { .. }) {
        assert!(start.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    driver.shutdown().await;
    assert_eq!(driver.port_mapping_status(), Status::Off);
    let port = bound.port();
    let requests = mapping_requests(&fixture);
    assert!(
        requests.contains(&(2, port, 0)) && requests.contains(&(1, port, 0)),
        "{requests:?}"
    );
}

#[tokio::test]
async fn listener_off_the_gateway_route_sends_nothing() {
    let fixture = natpmp_fixture(router(7200), false).await;
    // The gateway route's address is 127.0.0.1; a listener on 127.0.0.3
    // cannot be reached through it.
    let mapper = PortMapper::start(
        natpmp_only(fixture.port),
        "127.0.0.3:40116".parse().unwrap(),
    );
    let status = wait_for(&mapper, settled).await;
    assert_eq!(
        status,
        Status::Failed("the listener is not bound on the gateway's network".into())
    );
    assert!(fixture.requests.lock().unwrap().is_empty());
    mapper.stop(STOP_BOUND).await;
}

// ---------------------------------------------------------------------------
// UPnP fixtures

/// Shaped after miniupnpd 2.x's rootDesc.xml.
fn description(service: &str, control: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0" configId="1337">
<specVersion><major>1</major><minor>1</minor></specVersion>
<device>
<deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
<friendlyName>OpenWRT router</friendlyName>
<manufacturer>OpenWRT</manufacturer>
<manufacturerURL>https://openwrt.org/</manufacturerURL>
<modelDescription>OpenWRT router</modelDescription>
<modelName>OpenWRT router</modelName>
<modelNumber>1</modelNumber>
<modelURL>https://openwrt.org/</modelURL>
<serialNumber>00000000</serialNumber>
<UDN>uuid:c0ffee00-0000-4000-8000-000000000001</UDN>
<serviceList>
<service>
<serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
<serviceId>urn:upnp-org:serviceId:L3Forwarding1</serviceId>
<SCPDURL>/L3F.xml</SCPDURL><controlURL>/ctl/L3F</controlURL>
<eventSubURL>/evt/L3F</eventSubURL>
</service>
</serviceList>
<deviceList>
<device>
<deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>
<friendlyName>WANDevice</friendlyName>
<UDN>uuid:c0ffee00-0000-4000-8000-000000000002</UDN>
<serviceList>
<service>
<serviceType>urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1</serviceType>
<serviceId>urn:upnp-org:serviceId:WANCommonIFC1</serviceId>
<SCPDURL>/WANCfg.xml</SCPDURL><controlURL>/ctl/CmnIfCfg</controlURL>
<eventSubURL>/evt/CmnIfCfg</eventSubURL>
</service>
</serviceList>
<deviceList>
<device>
<deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType>
<friendlyName>WANConnectionDevice</friendlyName>
<UDN>uuid:c0ffee00-0000-4000-8000-000000000003</UDN>
<serviceList>
<!-- the connection service -->
<service>
<serviceType>{service}</serviceType>
<serviceId>urn:upnp-org:serviceId:WANConn1</serviceId>
<SCPDURL>/WANIPCn.xml</SCPDURL>
<controlURL>{control}</controlURL>
<eventSubURL>/evt/IPConn</eventSubURL>
</service>
</serviceList>
</device>
</deviceList>
</device>
</deviceList>
<presentationURL>http://192.168.1.1/</presentationURL>
</device>
</root>
"#
    )
}

#[derive(Clone)]
struct Soap {
    /// Fault every AddPortMapping with a non-zero lease with this code.
    lease_fault: Option<u16>,
    external: &'static str,
}

struct HttpFixture {
    port: u16,
    /// (path, SOAPAction, body) of each request.
    requests: Arc<Mutex<Vec<(String, String, String)>>>,
    task: JoinHandle<()>,
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&chunk[..n]);
        let Some(end) = find(&data, b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&data[..end]).into_owned();
        let length = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        if data.len() - end - 4 < length {
            continue;
        }
        let path = head.split(' ').nth(1).unwrap_or("").to_string();
        let action = head
            .lines()
            .find_map(|l| l.strip_prefix("SOAPAction: "))
            .unwrap_or("")
            .trim_matches('"')
            .to_string();
        let body = String::from_utf8_lossy(&data[end + 4..]).into_owned();
        return Some((path, action, body));
    }
}

fn http_response(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\nServer: OpenWRT/23.05 UPnP/1.1 MiniUPnPd/2.3.3\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn fault(code: u16) -> String {
    format!(
        r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>{code}</errorCode><errorDescription>OnlyPermanentLeasesSupported</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#
    )
}

async fn http_fixture(host: Ipv4Addr, description: String, soap: Soap) -> HttpFixture {
    let listener = TcpListener::bind((host, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let Some((path, action, body)) = read_request(&mut stream).await else {
                continue;
            };
            seen.lock()
                .unwrap()
                .push((path.clone(), action.clone(), body.clone()));
            let service = action.split('#').next().unwrap_or("").to_string();
            let response = if path == "/rootDesc.xml" {
                http_response("200 OK", &description)
            } else if action.ends_with("#AddPortMapping") {
                match soap.lease_fault {
                    Some(code) if !body.contains("<NewLeaseDuration>0<") => {
                        http_response("500 Internal Server Error", &fault(code))
                    }
                    _ => http_response(
                        "200 OK",
                        &format!(
                            "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:AddPortMappingResponse xmlns:u=\"{service}\"/></s:Body></s:Envelope>"
                        ),
                    ),
                }
            } else if action.ends_with("#GetExternalIPAddress") {
                http_response(
                    "200 OK",
                    &format!(
                        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:GetExternalIPAddressResponse xmlns:u=\"{service}\"><NewExternalIPAddress>{}</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>",
                        soap.external
                    ),
                )
            } else if action.ends_with("#DeletePortMapping") {
                http_response("200 OK", "<s:Envelope><s:Body/></s:Envelope>")
            } else {
                http_response("404 Not Found", "")
            };
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
        }
    });
    HttpFixture {
        port,
        requests,
        task,
    }
}

/// Answers every search on loopback with `location`.
async fn ssdp_fixture(location: String) -> (SocketAddrV4, Arc<Mutex<Vec<String>>>, JoinHandle<()>) {
    let socket = UdpSocket::bind((LOOPBACK, 0)).await.unwrap();
    let SocketAddr::V4(address) = socket.local_addr().unwrap() else {
        unreachable!()
    };
    let searches = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&searches);
    let task = tokio::spawn(async move {
        let mut buffer = [0u8; 2048];
        while let Ok((n, from)) = socket.recv_from(&mut buffer).await {
            let search = String::from_utf8_lossy(&buffer[..n]).into_owned();
            let target = search
                .lines()
                .find_map(|l| l.strip_prefix("ST: "))
                .unwrap_or("")
                .to_string();
            seen.lock().unwrap().push(search);
            let reply = format!(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nST: {target}\r\nUSN: uuid:c0ffee00-0000-4000-8000-000000000001::{target}\r\nEXT:\r\nSERVER: OpenWRT/23.05 UPnP/1.1 MiniUPnPd/2.3.3\r\nLOCATION: {location}\r\nOPT: \"http://schemas.upnp.org/upnp/1/0/\"; ns=01\r\n\r\n"
            );
            let _ = socket.send_to(reply.as_bytes(), from).await;
        }
    });
    (address, searches, task)
}

fn upnp_only(ssdp: SocketAddrV4) -> Config {
    Config {
        // NAT-PMP is tried first and answered by nobody, so UPnP is the
        // fallback; a closed port keeps that quick.
        natpmp_port: 9,
        natpmp_tries: 1,
        natpmp_initial: Duration::from_millis(20),
        ssdp_target: ssdp,
        ..fixture_config()
    }
}

fn actions(fixture: &HttpFixture) -> Vec<(String, String, String)> {
    fixture.requests.lock().unwrap().clone()
}

#[tokio::test]
async fn upnp_maps_through_wan_ip_connection_and_deletes_on_stop() {
    let service = "urn:schemas-upnp-org:service:WANIPConnection:1";
    let http = http_fixture(
        LOOPBACK,
        description(service, "/ctl/IPConn"),
        Soap {
            lease_fault: None,
            external: "198.51.100.4",
        },
    )
    .await;
    let (ssdp, searches, ssdp_task) =
        ssdp_fixture(format!("http://127.0.0.1:{}/rootDesc.xml", http.port)).await;
    let mapper = PortMapper::start(upnp_only(ssdp), "0.0.0.0:40121".parse().unwrap());
    let status = wait_for(&mapper, settled).await;
    let Status::Mapped {
        method,
        port,
        external,
        expires,
    } = status
    else {
        panic!("{status:?}");
    };
    assert_eq!((method, port), (Method::Upnp, 40121));
    assert_eq!(
        external,
        Some(SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 4), 40121))
    );
    assert!(expires.is_some());
    let searched = searches.lock().unwrap().clone();
    assert_eq!(searched.len(), SEARCH_TARGETS.len());
    assert!(searched[0].starts_with("M-SEARCH * HTTP/1.1\r\n"));
    assert!(searched[0].contains("MAN: \"ssdp:discover\""));
    let requests = actions(&http);
    let names: Vec<_> = requests
        .iter()
        .map(|(path, action, _)| format!("{path} {action}"))
        .collect();
    assert_eq!(
        names,
        vec![
            "/rootDesc.xml ".to_string(),
            format!("/ctl/IPConn {service}#AddPortMapping"),
            format!("/ctl/IPConn {service}#AddPortMapping"),
            format!("/ctl/IPConn {service}#GetExternalIPAddress"),
        ]
    );
    let tcp = &requests[1].2;
    for field in [
        "<NewRemoteHost></NewRemoteHost>",
        "<NewExternalPort>40121</NewExternalPort>",
        "<NewProtocol>TCP</NewProtocol>",
        "<NewInternalPort>40121</NewInternalPort>",
        "<NewInternalClient>127.0.0.1</NewInternalClient>",
        "<NewEnabled>1</NewEnabled>",
        "<NewPortMappingDescription>TemplarTorrent</NewPortMappingDescription>",
        "<NewLeaseDuration>3600</NewLeaseDuration>",
    ] {
        assert!(tcp.contains(field), "{field} missing from {tcp}");
    }
    assert!(requests[2].2.contains("<NewProtocol>UDP</NewProtocol>"));
    mapper.stop(STOP_BOUND).await;
    let requests = actions(&http);
    let deletes: Vec<_> = requests[4..]
        .iter()
        .map(|(_, action, body)| {
            assert!(action.ends_with("#DeletePortMapping"), "{action}");
            assert!(body.contains("<NewExternalPort>40121</NewExternalPort>"));
            body.contains("<NewProtocol>TCP</NewProtocol>")
        })
        .collect();
    assert_eq!(deletes, vec![true, false]);
    ssdp_task.abort();
}

#[tokio::test]
async fn upnp_uses_a_wan_ppp_connection_when_that_is_all_there_is() {
    let service = "urn:schemas-upnp-org:service:WANPPPConnection:1";
    let http = http_fixture(
        LOOPBACK,
        description(service, "/ctl/PPPConn"),
        Soap {
            lease_fault: None,
            external: "198.51.100.5",
        },
    )
    .await;
    let (ssdp, _, ssdp_task) =
        ssdp_fixture(format!("http://127.0.0.1:{}/rootDesc.xml", http.port)).await;
    let mapper = PortMapper::start(upnp_only(ssdp), "0.0.0.0:40122".parse().unwrap());
    let status = wait_for(&mapper, settled).await;
    assert!(
        matches!(
            status,
            Status::Mapped {
                method: Method::Upnp,
                port: 40122,
                ..
            }
        ),
        "{status:?}"
    );
    assert!(
        actions(&http)
            .iter()
            .skip(1)
            .all(|(path, action, _)| path == "/ctl/PPPConn" && action.starts_with(service))
    );
    mapper.stop(STOP_BOUND).await;
    ssdp_task.abort();
}

#[tokio::test]
async fn upnp_error_725_falls_back_to_a_permanent_lease() {
    let service = "urn:schemas-upnp-org:service:WANIPConnection:2";
    let http = http_fixture(
        LOOPBACK,
        description(service, "ctl/IPConn"),
        Soap {
            lease_fault: Some(725),
            external: "10.64.0.2",
        },
    )
    .await;
    let (ssdp, _, ssdp_task) =
        ssdp_fixture(format!("http://127.0.0.1:{}/rootDesc.xml", http.port)).await;
    let mapper = PortMapper::start(upnp_only(ssdp), "0.0.0.0:40123".parse().unwrap());
    let status = wait_for(&mapper, settled).await;
    let Status::Mapped { expires, .. } = &status else {
        panic!("{status:?}");
    };
    assert_eq!(*expires, None, "a permanent lease has no expiry");
    // A private external address is called out without being printed.
    let text = status.describe();
    assert!(
        text.contains("private") && !text.contains("10.64"),
        "{text}"
    );
    let leases: Vec<_> = actions(&http)
        .iter()
        .filter(|(_, action, _)| action.ends_with("#AddPortMapping"))
        .map(|(path, _, body)| {
            assert_eq!(path, "/ctl/IPConn", "relative controlURL resolves");
            body.contains("<NewLeaseDuration>0</NewLeaseDuration>")
        })
        .collect();
    // TCP: 3600 refused, then 0. UDP goes straight to 0.
    assert_eq!(leases, vec![false, true, true]);
    mapper.stop(STOP_BOUND).await;
    ssdp_task.abort();
}

#[tokio::test]
async fn upnp_refuses_a_location_off_the_gateway() {
    let service = "urn:schemas-upnp-org:service:WANIPConnection:1";
    let elsewhere = http_fixture(
        Ipv4Addr::new(127, 0, 0, 2),
        description(service, "/ctl/IPConn"),
        Soap {
            lease_fault: None,
            external: "198.51.100.4",
        },
    )
    .await;
    let (ssdp, _, ssdp_task) =
        ssdp_fixture(format!("http://127.0.0.2:{}/rootDesc.xml", elsewhere.port)).await;
    let mapper = PortMapper::start(upnp_only(ssdp), "0.0.0.0:40124".parse().unwrap());
    let status = wait_for(&mapper, settled).await;
    let Status::Failed(reason) = &status else {
        panic!("{status:?}");
    };
    assert!(
        reason.contains("UPnP: SSDP LOCATION points to a host other than the gateway"),
        "{reason}"
    );
    assert!(
        actions(&elsewhere).is_empty(),
        "the other host was contacted"
    );
    mapper.stop(STOP_BOUND).await;
    ssdp_task.abort();
}

#[tokio::test]
async fn upnp_refuses_an_oversized_description() {
    let service = "urn:schemas-upnp-org:service:WANIPConnection:1";
    let padding = format!("<!-- {} -->", "x".repeat(70 * 1024));
    let huge =
        description(service, "/ctl/IPConn").replace("<device>", &format!("<device>{padding}"));
    let http = http_fixture(
        LOOPBACK,
        huge,
        Soap {
            lease_fault: None,
            external: "198.51.100.4",
        },
    )
    .await;
    let (ssdp, _, ssdp_task) =
        ssdp_fixture(format!("http://127.0.0.1:{}/rootDesc.xml", http.port)).await;
    let mapper = PortMapper::start(upnp_only(ssdp), "0.0.0.0:40125".parse().unwrap());
    let status = wait_for(&mapper, settled).await;
    let Status::Failed(reason) = &status else {
        panic!("{status:?}");
    };
    assert!(
        reason.contains("UPnP: gateway response exceeds 64 KiB"),
        "{reason}"
    );
    assert!(
        actions(&http)
            .iter()
            .all(|(_, action, _)| !action.contains("AddPortMapping"))
    );
    mapper.stop(STOP_BOUND).await;
    ssdp_task.abort();
}

// ---------------------------------------------------------------------------
// Parsers

#[test]
fn route_table_picks_the_lowest_metric_default_gateway() {
    let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
        wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
        eth0\t00000000\t010A0A0A\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
        eth0\t000A0A0A\t00000000\t0001\t0\t0\t100\t00FEFFFF\t0\t0\t0\n\
        tun0\t00000000\t00000000\t0001\t0\t0\t50\t00000000\t0\t0\t0\n";
    let expected = if cfg!(target_endian = "little") {
        Ipv4Addr::new(10, 10, 10, 1)
    } else {
        Ipv4Addr::new(1, 10, 10, 10)
    };
    assert_eq!(parse_route_table(table), Some(expected));
    assert_eq!(parse_route_table("Iface\n"), None);
    assert_eq!(parse_route_table("Iface\nbad line\n"), None);
}

#[test]
fn natpmp_wire_format_round_trips() {
    assert_eq!(
        natpmp_mapping_request(Protocol::Tcp, 6881, 6881, 7200),
        [0, 2, 0, 0, 0x1a, 0xe1, 0x1a, 0xe1, 0, 0, 0x1c, 0x20]
    );
    let reply = mapping_reply(
        &natpmp_mapping_request(Protocol::Udp, 6881, 6881, 7200),
        0,
        3600,
    );
    assert_eq!(
        parse_natpmp_reply(&reply),
        Ok(NatPmpReply::Mapping {
            epoch: 9,
            protocol: Protocol::Udp,
            internal: 6881,
            external: 6881,
            lifetime: 3600,
        })
    );
    assert!(matches!(
        parse_natpmp_reply(&reply[..15]),
        Err(NatPmpError::Malformed(_))
    ));
    assert!(matches!(
        parse_natpmp_reply(&[2, 128, 0, 0]),
        Err(NatPmpError::Malformed(_))
    ));
    assert!(matches!(
        parse_natpmp_reply(&[0, 2, 0, 0]),
        Err(NatPmpError::Malformed(_))
    ));
}

#[test]
fn ssdp_and_url_admission() {
    let answer = b"HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nlocation: http://10.10.10.1:5000/rootDesc.xml\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nServer: x\x07y\r\n\r\n";
    let parsed = parse_ssdp_response(answer).unwrap();
    assert_eq!(parsed.location, "http://10.10.10.1:5000/rootDesc.xml");
    assert_eq!(parsed.server, "xy");
    let gateway = Ipv4Addr::new(10, 10, 10, 1);
    assert_eq!(
        admit_location(&parsed.location, gateway).unwrap(),
        HttpUrl {
            host: gateway,
            port: 5000,
            path: "/rootDesc.xml".into()
        }
    );
    for refused in [
        "https://10.10.10.1/rootDesc.xml",
        "http://router.local/rootDesc.xml",
        "http://user@10.10.10.1/rootDesc.xml",
        "http://10.10.10.2/rootDesc.xml",
        "http://10.10.10.1:0/",
        "http://10.10.10.1/a b",
        "ftp://10.10.10.1/",
    ] {
        assert!(admit_location(refused, gateway).is_err(), "{refused}");
    }
    assert!(
        parse_ssdp_response(b"NOTIFY * HTTP/1.1\r\nLOCATION: http://10.10.10.1/\r\n\r\n").is_err()
    );
    assert!(parse_ssdp_response(b"HTTP/1.1 200 OK\r\nST: x\r\n\r\n").is_err());
    let location = admit_location("http://10.10.10.1:5000/rootDesc.xml", gateway).unwrap();
    assert_eq!(
        control_target("/ctl/IPConn", &location, gateway)
            .unwrap()
            .path,
        "/ctl/IPConn"
    );
    assert_eq!(
        control_target("http://10.10.10.1:5001/c", &location, gateway)
            .unwrap()
            .port,
        5001
    );
    assert!(control_target("http://10.10.10.9/c", &location, gateway).is_err());
    assert!(control_target("https://10.10.10.1/c", &location, gateway).is_err());
}

#[test]
fn description_scan_is_bounded() {
    let service = "urn:schemas-upnp-org:service:WANIPConnection:1";
    let services = parse_description(description(service, "/ctl/IPConn&amp;x").as_bytes()).unwrap();
    assert_eq!(
        services,
        vec![IgdService {
            service_type: service.into(),
            control_url: "/ctl/IPConn&x".into(),
        }]
    );
    let deep = "<a>".repeat(MAX_XML_DEPTH + 1);
    assert_eq!(
        parse_description(deep.as_bytes()),
        Err("XML is nested too deeply")
    );
    assert_eq!(
        parse_description(b"<!DOCTYPE x [<!ENTITY a \"aaaa\">]><root/>"),
        Err("XML DOCTYPE is refused")
    );
    let many = "<a/>".repeat(MAX_XML_TAGS + 1);
    assert_eq!(
        parse_description(many.as_bytes()),
        Err("XML has too many tags")
    );
    // A service type that would break the SOAPAction header is dropped.
    let hostile = description("urn:schemas-upnp-org:service:WANIPConnection:1\"x", "/c");
    assert_eq!(parse_description(hostile.as_bytes()), Ok(Vec::new()));
}

#[test]
fn soap_replies_and_faults() {
    let ok = http_response(
        "200 OK",
        "<s:Envelope><s:Body><u:R><NewExternalIPAddress> 198.51.100.4 </NewExternalIPAddress></u:R></s:Body></s:Envelope>",
    );
    let SoapReply::Success(body) = parse_soap_response(&ok).unwrap() else {
        panic!();
    };
    assert_eq!(
        soap_element(&body, "NewExternalIPAddress").as_deref(),
        Some("198.51.100.4")
    );
    assert_eq!(
        parse_soap_response(&http_response("500 Internal Server Error", &fault(718))),
        Ok(SoapReply::Fault(Some(718)))
    );
    assert_eq!(
        parse_soap_response(&http_response("500 Internal Server Error", "garbage")),
        Ok(SoapReply::Fault(None))
    );
    assert!(parse_soap_response(&http_response("302 Found", "")).is_err());
    assert_eq!(
        SoapFailure::Code(Some(718)).reason(),
        "router already maps this port to another host"
    );
}

/// Opt-in, read-only probe of this machine's real default gateway:
/// NAT-PMP opcode 0, SSDP search, and a GET of the advertised description.
/// It never requests a mapping. Prints no external address, only its class.
///   TT_PORTMAP_PROBE=1 cargo test -p templartorrent-rbit-adapter real_gateway_probe -- --ignored --nocapture
#[tokio::test]
#[ignore = "contacts the real default gateway; set TT_PORTMAP_PROBE=1"]
async fn real_gateway_probe() {
    if std::env::var_os("TT_PORTMAP_PROBE").is_none() {
        eprintln!("TT_PORTMAP_PROBE is not set; skipped");
        return;
    }
    let report = probe(&Config::default()).await;
    match &report.route {
        Ok(route) => eprintln!("gateway {} (our side {})", route.gateway, route.local),
        Err(reason) => eprintln!("route: {reason}"),
    }
    match &report.natpmp {
        Ok(address) => eprintln!(
            "NAT-PMP: answered, external address is {}",
            if is_public(*address) {
                "public"
            } else {
                "private or special"
            }
        ),
        Err(reason) => eprintln!("NAT-PMP: {reason}"),
    }
    match &report.ssdp {
        Ok(outcome) => {
            eprintln!(
                "SSDP: {} answer(s) from other hosts, {} refused from the gateway{}",
                outcome.other_hosts,
                outcome.refused_count,
                outcome
                    .refused
                    .map(|r| format!(" (first: {r})"))
                    .unwrap_or_default()
            );
            if let Some((location, response)) = &outcome.accepted {
                eprintln!(
                    "SSDP: gateway answered ST={} SERVER={} LOCATION path {} port {}",
                    response.search_target, response.server, location.path, location.port
                );
            } else {
                eprintln!("SSDP: no admitted gateway answer");
            }
        }
        Err(reason) => eprintln!("SSDP: {reason}"),
    }
    match &report.services {
        Some(Ok(services)) => {
            for service in services {
                eprintln!(
                    "description: {} control {}",
                    service.service_type, service.control_url
                );
            }
            if services.is_empty() {
                eprintln!("description: no WAN connection service");
            }
        }
        Some(Err(reason)) => eprintln!("description: {reason}"),
        None => {}
    }
}

#[test]
fn route_follows_the_egress_through_a_vpn_tunnel() {
    let hotel = Ipv4Addr::new(10, 10, 10, 1);
    let lan = Ipv4Addr::new(10, 10, 10, 125);
    let tunnel = Ipv4Addr::new(10, 2, 0, 2);
    // Plain LAN: traffic leaves on the main default route.
    assert_eq!(
        pick_route(Some(hotel), Some(lan), Some(lan)),
        Some(Route {
            gateway: hotel,
            local: lan
        })
    );
    // Policy-routed VPN: the main table still names the hotel router.
    assert_eq!(
        pick_route(Some(hotel), Some(lan), Some(tunnel)),
        Some(Route {
            gateway: Ipv4Addr::new(10, 2, 0, 1),
            local: tunnel
        })
    );
    // A public egress or a .1 egress never invents a gateway.
    assert_eq!(
        pick_route(Some(hotel), Some(lan), Some(Ipv4Addr::new(203, 0, 113, 7))),
        None
    );
    assert_eq!(
        pick_route(None, None, Some(Ipv4Addr::new(10, 2, 0, 1))),
        None
    );
    assert_eq!(
        pick_route(None, None, Some(Ipv4Addr::new(100, 64, 3, 9))),
        Some(Route {
            gateway: Ipv4Addr::new(100, 64, 3, 1),
            local: Ipv4Addr::new(100, 64, 3, 9)
        })
    );
    assert_eq!(
        pick_route(Some(hotel), Some(lan), None),
        Some(Route {
            gateway: hotel,
            local: lan
        })
    );
}

/// A router that assigns its own external port, as Proton VPN's NAT-PMP
/// does: the driver announces that port and also listens on it.
#[tokio::test]
async fn assigned_external_port_is_announced_and_listened_on() {
    let assigned = {
        let probe = std::net::TcpListener::bind((LOOPBACK, 0)).unwrap();
        probe.local_addr().unwrap().port()
    };
    let answer: Answer = Arc::new(move |request: &[u8]| match request {
        [0, 0] => Some(external_reply(Ipv4Addr::new(203, 0, 113, 7))),
        [0, 1 | 2, ..] if request.len() == 12 => {
            let mut reply = mapping_reply(request, 0, 60);
            let asked = u32::from_be_bytes([request[8], request[9], request[10], request[11]]);
            if asked != 0 {
                reply[10..12].copy_from_slice(&assigned.to_be_bytes());
            }
            Some(reply)
        }
        _ => None,
    });
    let fixture = natpmp_fixture(answer, false).await;
    let mut driver = crate::driver::Driver::new(Default::default()).unwrap();
    let bound = driver.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    assert_ne!(bound.port(), assigned);
    assert_eq!(driver.announced_port(), Some(bound.port()));
    driver.set_port_mapping_config(natpmp_only(fixture.port));
    driver.enable_port_mapping().unwrap();
    let start = Instant::now();
    while driver.announced_port() != Some(assigned) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            driver.port_mapping_status()
        );
        driver.step(Duration::from_millis(5)).await.unwrap();
    }
    driver.step(Duration::from_millis(5)).await.unwrap();
    let reached = tokio::net::TcpStream::connect((LOOPBACK, assigned)).await;
    assert!(reached.is_ok(), "the mapped port has a listener");
    driver.shutdown().await;
    assert!(
        tokio::net::TcpStream::connect((LOOPBACK, assigned))
            .await
            .is_err()
    );
}

#[test]
fn renewal_stays_inside_a_short_lifetime() {
    let mapping = |lifetime| Mapping {
        method: Method::NatPmp,
        port: 1,
        external: None,
        lifetime: Some(lifetime),
        upnp: None,
    };
    let config = Config::default();
    assert_eq!(mapping(60).renew_after(&config), Duration::from_secs(45));
    assert_eq!(
        mapping(7200).renew_after(&config),
        Duration::from_secs(3600)
    );
    assert_eq!(
        mapping(10).renew_after(&config),
        Duration::from_millis(7500)
    );
}

/// With a mapping in place, a newly added job's first tracker announce
/// carries the mapped port, not the listen port. (A mapping that appears
/// after a job announced marks its trackers stale; they announce again when
/// the 60 s manual cooldown allows, which a fast test cannot wait for.)
#[tokio::test]
async fn announces_carry_the_mapped_port() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let assigned = {
        let probe = std::net::TcpListener::bind((LOOPBACK, 0)).unwrap();
        probe.local_addr().unwrap().port()
    };
    let answer: Answer = Arc::new(move |request: &[u8]| match request {
        [0, 0] => Some(external_reply(Ipv4Addr::new(203, 0, 113, 7))),
        [0, 1 | 2, ..] if request.len() == 12 => {
            let mut reply = mapping_reply(request, 0, 60);
            reply[10..12].copy_from_slice(&assigned.to_be_bytes());
            Some(reply)
        }
        _ => None,
    });
    let fixture = natpmp_fixture(answer, false).await;
    // A tracker that records the port of every announce.
    let tracker = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
    let tracker_port = tracker.local_addr().unwrap().port();
    let ports = Arc::new(Mutex::new(Vec::<u16>::new()));
    let seen = Arc::clone(&ports);
    let tracker_task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = tracker.accept().await {
            let mut request = vec![0u8; 4096];
            let n = stream.read(&mut request).await.unwrap_or(0);
            let text = String::from_utf8_lossy(&request[..n]).to_string();
            if let Some(port) = text
                .split(['?', '&', ' '])
                .find_map(|part| part.strip_prefix("port="))
                .and_then(|value| value.parse().ok())
            {
                seen.lock().unwrap().push(port);
            }
            let body = b"d8:intervali1800e12:min intervali0e5:peers0:e";
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(body).await;
        }
    });
    let data = vec![7u8; 40_000];
    let torrent = rbit::TorrentBuilder::new("mapped")
        .version(rbit::TorrentVersion::V1)
        .piece_length(16384)
        .add_tracker(format!("http://127.0.0.1:{tracker_port}/announce"))
        .add_file("a.bin", data)
        .build()
        .unwrap();
    let root = std::env::temp_dir().join(format!("tt-portmap-reannounce-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut driver = crate::driver::Driver::new(Default::default()).unwrap();
    driver.set_allow_local_trackers(true);
    let bound = driver.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    driver.set_port_mapping_config(natpmp_only(fixture.port));
    driver.enable_port_mapping().unwrap();
    let start = Instant::now();
    while driver.announced_port() != Some(assigned) {
        assert!(start.elapsed() < Duration::from_secs(5), "no mapping");
        driver.step(Duration::from_millis(5)).await.unwrap();
    }
    driver
        .add_torrent(&torrent, &root, crate::driver::ExistingData::Refuse)
        .await
        .unwrap();
    while !ports.lock().unwrap().contains(&assigned) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "trackers never heard the mapped port: {:?}",
            ports.lock().unwrap()
        );
        driver.step(Duration::from_millis(5)).await.unwrap();
    }
    assert!(
        !ports.lock().unwrap().contains(&bound.port()),
        "the unreachable listen port was never announced"
    );
    driver.shutdown().await;
    tracker_task.abort();
    let _ = std::fs::remove_dir_all(&root);
}

// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! IS-13 round-trip through the Rust wrapper: create-time annotation, then
//! HTTP PATCH on the Annotation API. The test keeps the stand-in store an
//! application would keep; the library does not.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nvnmos::{
    Annotation, AnnotationChange, NodeConfig, NodeServer, ResourceType, SenderConfig, Transport,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Captured {
    resource_type: ResourceType,
    name: Option<String>,
    annotation: Annotation,
    label_changed: bool,
    description_changed: bool,
    tags_changed: bool,
}

fn apply_change(stored: &mut Annotation, change: &Captured) {
    if change.label_changed {
        stored.label.clone_from(&change.annotation.label);
    }
    if change.description_changed {
        stored
            .description
            .clone_from(&change.annotation.description);
    }
    if change.tags_changed {
        stored.tags.clone_from(&change.annotation.tags);
    }
}

fn local_iface_ip() -> String {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0").expect("bind iface probe");
    sock.connect("8.8.8.8:80").expect("probe local interface");
    sock.local_addr()
        .expect("local interface address")
        .ip()
        .to_string()
}

fn ephemeral_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("ephemeral addr")
        .port()
}

fn http(method: &str, port: u16, path: &str, body: Option<&str>) -> (u16, String) {
    let mut last_error = String::new();
    for _ in 0..50 {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut sock) => {
                sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                sock.set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let request = match body {
                    Some(body) => format!(
                        "{method} {path} HTTP/1.1\r\n\
                         Host: 127.0.0.1:{port}\r\n\
                         Content-Type: application/json\r\n\
                         Content-Length: {len}\r\n\
                         Connection: close\r\n\
                         \r\n\
                         {body}",
                        len = body.len(),
                    ),
                    None => format!(
                        "{method} {path} HTTP/1.1\r\n\
                         Host: 127.0.0.1:{port}\r\n\
                         Connection: close\r\n\
                         \r\n",
                    ),
                };
                sock.write_all(request.as_bytes()).unwrap();
                let mut response = String::new();
                sock.read_to_string(&mut response).unwrap();
                let status = response
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|status| status.parse().ok())
                    .unwrap_or_else(|| panic!("bad HTTP status in {response:?}"));
                return (status, response);
            }
            Err(error) => {
                last_error = error.to_string();
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    panic!("could not connect to 127.0.0.1:{port}: {last_error}");
}

fn json_body(response: &str) -> serde_json::Value {
    let start = response
        .find('{')
        .unwrap_or_else(|| panic!("no JSON in {response}"));
    serde_json::from_str(&response[start..]).unwrap_or_else(|error| panic!("{error}: {response}"))
}

fn get_json(port: u16, path: &str) -> serde_json::Value {
    let (status, response) = http("GET", port, path, None);
    assert!(
        (200..300).contains(&status),
        "GET {path} returned {status}: {response}"
    );
    json_body(&response)
}

fn patch_json(port: u16, path: &str, body: serde_json::Value) -> serde_json::Value {
    let body = body.to_string();
    let (status, response) = http("PATCH", port, path, Some(&body));
    assert!(
        (200..300).contains(&status),
        "PATCH {path} {body} returned {status}: {response}"
    );
    json_body(&response)
}

fn minimal_sender_sdp(name: &str, iface_ip: &str) -> String {
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {iface_ip}\r\n\
         s=session-default\r\n\
         t=0 0\r\n\
         a=x-nvnmos-name:{name}\r\n\
         m=video 5020 RTP/AVP 96\r\n\
         c=IN IP4 233.252.0.0/64\r\n\
         a=source-filter: incl IN IP4 233.252.0.0 {iface_ip}\r\n\
         a=x-nvnmos-iface-ip:{iface_ip}\r\n\
         a=rtpmap:96 raw/90000\r\n\
         a=fmtp:96 sampling=YCbCr-4:2:2; width=1920; height=1080; \
         exactframerate=50; depth=10; TCS=SDR; colorimetry=BT709; \
         PM=2110GPM; SSN=ST2110-20:2017; TP=2110TPN;\r\n\
         a=mediaclk:direct=0\r\n\
         a=x-nvnmos-src-port:5004\r\n\
         a=ts-refclk:localmac=CA-FE-01-CA-FE-02\r\n"
    )
}

#[test]
fn annotation_http_round_trip() {
    let port = ephemeral_port();
    let events = Arc::new(Mutex::new(Vec::<Captured>::new()));
    let recorded = Arc::clone(&events);
    let config = NodeConfig {
        seed: "annotation-http".into(),
        host_addresses: vec!["127.0.0.1".into()],
        http_port: port,
        label: "node-default".into(),
        description: "node-description".into(),
        node_annotation: Some(Annotation {
            label: Some("node-overlay".into()),
            ..Annotation::default()
        }),
        ..NodeConfig::default()
    };
    let iface_ip = local_iface_ip();
    let server = NodeServer::builder(&config)
        .on_annotation_changed(move |change: &AnnotationChange<'_>| {
            recorded.lock().unwrap().push(Captured {
                resource_type: change.resource_type,
                name: change.name.map(str::to_owned),
                annotation: change.annotation.clone(),
                label_changed: change.label_changed,
                description_changed: change.description_changed,
                tags_changed: change.tags_changed,
            });
        })
        .build()
        .expect("node server");

    let sender_name = "video";
    let mut stored = Annotation {
        label: Some("sender-overlay".into()),
        tags: BTreeMap::from([("foo".into(), vec!["a".into()])]),
        ..Annotation::default()
    };
    server
        .add_sender(&SenderConfig {
            transport: Transport::Rtp,
            transport_file: minimal_sender_sdp(sender_name, &iface_ip),
            sender_annotation: Some(stored.clone()),
            source_annotation: None,
            flow_annotation: None,
        })
        .expect("add sender");
    assert!(
        events.lock().unwrap().is_empty(),
        "create-time annotation does not invoke the callback"
    );

    let sender_id = server.sender_id(sender_name).unwrap().expect("sender id");
    let node = get_json(port, "/x-nmos/node/v1.3/self");
    assert_eq!(node["label"], "node-overlay");
    let sender = get_json(port, &format!("/x-nmos/node/v1.3/senders/{sender_id}"));
    assert_eq!(sender["label"], "sender-overlay");
    assert_eq!(sender["tags"]["foo"][0], "a");
    assert_eq!(
        sender["tags"]["urn:x-nvnmos:tag:name"][0], sender_name,
        "read-only name tag stays on the resource"
    );

    let sender_path = format!("/x-nmos/annotation/v1.0/node/senders/{sender_id}/");
    patch_json(
        port,
        &sender_path,
        serde_json::json!({ "description": "only-description" }),
    );
    {
        let events = events.lock().unwrap();
        let change = events.last().unwrap();
        assert_eq!(change.resource_type, ResourceType::Sender);
        assert_eq!(change.name.as_deref(), Some(sender_name));
        assert!(!change.label_changed);
        assert!(change.description_changed);
        assert!(!change.tags_changed);
        assert_eq!(change.annotation.label, None);
        assert_eq!(
            change.annotation.description.as_deref(),
            Some("only-description")
        );
        apply_change(&mut stored, change);
    }
    assert_eq!(stored.label.as_deref(), Some("sender-overlay"));

    patch_json(port, &sender_path, serde_json::json!({ "label": "" }));
    {
        let change = events.lock().unwrap().last().unwrap().clone();
        assert!(change.label_changed);
        assert_eq!(change.annotation.label.as_deref(), Some(""));
        apply_change(&mut stored, &change);
    }
    assert_eq!(
        get_json(port, &format!("/x-nmos/node/v1.3/senders/{sender_id}"))["label"],
        ""
    );

    patch_json(port, &sender_path, serde_json::json!({ "label": null }));
    {
        let change = events.lock().unwrap().last().unwrap().clone();
        assert!(change.label_changed);
        assert_eq!(change.annotation.label, None);
        apply_change(&mut stored, &change);
    }
    assert_eq!(stored.label, None);
    assert_eq!(
        get_json(port, &format!("/x-nmos/node/v1.3/senders/{sender_id}"))["label"],
        "session-default"
    );

    patch_json(
        port,
        &sender_path,
        serde_json::json!({ "tags": { "foo": ["a", "b"], "bar": [] } }),
    );
    {
        let change = events.lock().unwrap().last().unwrap().clone();
        assert!(change.tags_changed);
        assert_eq!(
            change.annotation.tags.get("foo").map(Vec::as_slice),
            Some(["a".into(), "b".into()].as_slice())
        );
        assert_eq!(
            change.annotation.tags.get("bar").map(Vec::as_slice),
            Some([].as_slice())
        );
        assert!(!change.annotation.tags.contains_key("urn:x-nvnmos:tag:name"));
        apply_change(&mut stored, &change);
    }

    patch_json(
        port,
        &sender_path,
        serde_json::json!({ "tags": { "foo": null } }),
    );
    {
        let change = events.lock().unwrap().last().unwrap().clone();
        assert!(change.tags_changed);
        assert!(!change.annotation.tags.contains_key("foo"));
        assert!(change.annotation.tags.contains_key("bar"));
        apply_change(&mut stored, &change);
    }
    let sender = get_json(port, &format!("/x-nmos/node/v1.3/senders/{sender_id}"));
    assert!(sender["tags"].get("foo").is_none());
    assert_eq!(sender["tags"]["bar"].as_array().unwrap().len(), 0);
    assert_eq!(sender["tags"]["urn:x-nvnmos:tag:name"][0], sender_name);

    let before = events.lock().unwrap().len();
    patch_json(
        port,
        &sender_path,
        serde_json::json!({ "description": "again" }),
    );
    {
        let events = events.lock().unwrap();
        assert_eq!(events.len(), before + 1);
        let change = events.last().unwrap();
        assert!(!change.tags_changed);
        assert_eq!(
            stored.tags.get("bar").map(Vec::as_slice),
            Some([].as_slice())
        );
    }

    patch_json(
        port,
        "/x-nmos/annotation/v1.0/node/self/",
        serde_json::json!({ "label": null }),
    );
    {
        let change = events.lock().unwrap().last().unwrap().clone();
        assert_eq!(change.resource_type, ResourceType::Node);
        assert_eq!(change.name, None);
        assert!(change.label_changed);
        assert_eq!(change.annotation.label, None);
    }
    assert_eq!(
        get_json(port, "/x-nmos/node/v1.3/self")["label"],
        "node-default"
    );

    drop(server);
}

#[test]
fn annotation_api_is_unmounted_without_a_callback() {
    let port = ephemeral_port();
    let config = NodeConfig {
        seed: "annotation-off".into(),
        host_addresses: vec!["127.0.0.1".into()],
        http_port: port,
        label: "node-default".into(),
        node_annotation: Some(Annotation {
            label: Some("node-overlay".into()),
            ..Annotation::default()
        }),
        ..NodeConfig::default()
    };
    let server = NodeServer::new(&config).expect("node server");

    let node = get_json(port, "/x-nmos/node/v1.3/self");
    assert_eq!(node["label"], "node-overlay");
    assert_eq!(node["services"].as_array().map(Vec::len), Some(0));

    let (status, response) = http("GET", port, "/x-nmos/", None);
    assert!(
        (200..300).contains(&status),
        "GET /x-nmos/ returned {status}: {response}"
    );
    let start = response
        .find('[')
        .unwrap_or_else(|| panic!("no JSON array in {response}"));
    let listing: Vec<String> = serde_json::from_str(&response[start..])
        .unwrap_or_else(|error| panic!("{error}: {response}"));
    assert!(
        !listing.iter().any(|name| name == "annotation/"),
        "annotation API mounted without a callback: {listing:?}"
    );
    assert!(listing.iter().any(|name| name == "connection/"));

    drop(server);
}

// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! IS-13 round-trip through the Rust wrapper: create-time annotation, then
//! HTTP PATCH and `update_annotation`. The callback report and the IS-04
//! resource are what the test checks.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nvnmos::{
    Annotation, AnnotationTag, AnnotationUpdate, NodeConfig, NodeServer, ResourceType,
    SenderConfig, Transport, Update,
};

fn tag_values<'a>(tags: &'a [AnnotationTag], key: &str) -> Option<&'a [String]> {
    tags.iter()
        .find(|tag| tag.key == key)
        .map(|tag| tag.values.as_slice())
}

fn tag_update(tags: &[AnnotationTag], key: &str) -> Option<Update> {
    tags.iter()
        .find(|tag| tag.key == key)
        .map(|tag| tag.values_update)
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
    let events = Arc::new(Mutex::new(Vec::<AnnotationUpdate>::new()));
    let recorded = Arc::clone(&events);
    let config = NodeConfig {
        seed: "annotation-http".into(),
        host_addresses: vec!["127.0.0.1".into()],
        http_port: port,
        label: "node-default".into(),
        description: "node-description".into(),
        node_annotation: Some(Annotation {
            label_update: Update::Set,
            label: Some("node-overlay".into()),
            ..Annotation::default()
        }),
        ..NodeConfig::default()
    };
    let iface_ip = local_iface_ip();
    let server = NodeServer::builder(&config)
        .on_annotation_updated(move |update: &AnnotationUpdate| {
            recorded.lock().unwrap().push(update.clone());
            Ok(())
        })
        .build()
        .expect("node server");

    let sender_name = "video";
    server
        .add_sender(&SenderConfig {
            transport: Transport::Rtp,
            transport_file: minimal_sender_sdp(sender_name, &iface_ip),
            sender_annotation: Some(Annotation {
                label_update: Update::Set,
                label: Some("sender-overlay".into()),
                tags_update: Update::Set,
                tags: vec![AnnotationTag {
                    key: "foo".into(),
                    values_update: Update::Set,
                    values: vec!["a".into()],
                }],
                ..Annotation::default()
            }),
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
        let update = events.last().unwrap();
        assert_eq!(update.resource_type, ResourceType::Sender);
        assert_eq!(update.name.as_deref(), Some(sender_name));
        assert_eq!(update.annotation.label_update, Update::Unchanged);
        assert_eq!(update.annotation.description_update, Update::Set);
        assert_eq!(update.annotation.tags_update, Update::Unchanged);
        assert_eq!(update.annotation.label.as_deref(), Some("sender-overlay"));
        assert_eq!(
            update.annotation.description.as_deref(),
            Some("only-description")
        );
        assert_eq!(
            tag_values(&update.annotation.tags, "foo"),
            Some(["a".into()].as_slice())
        );
        assert_eq!(
            update
                .annotation
                .tags
                .iter()
                .find(|tag| tag.key == "foo")
                .map(|tag| tag.values_update),
            Some(Update::Unchanged)
        );
    }

    patch_json(
        port,
        &sender_path,
        serde_json::json!({ "label": "session-default" }),
    );
    {
        let update = events.lock().unwrap().last().unwrap().clone();
        assert_eq!(update.annotation.label_update, Update::Set);
        assert_eq!(update.annotation.label.as_deref(), Some("session-default"));
    }
    patch_json(
        port,
        &sender_path,
        serde_json::json!({ "description": "after-default" }),
    );
    {
        let update = events.lock().unwrap().last().unwrap().clone();
        assert_eq!(update.annotation.label_update, Update::Unchanged);
        assert_eq!(update.annotation.label.as_deref(), Some("session-default"));
        assert_eq!(update.annotation.description_update, Update::Set);
    }

    patch_json(port, &sender_path, serde_json::json!({ "label": "" }));
    {
        let update = events.lock().unwrap().last().unwrap().clone();
        assert_eq!(update.annotation.label_update, Update::Set);
        assert_eq!(update.annotation.label.as_deref(), Some(""));
    }
    assert_eq!(
        get_json(port, &format!("/x-nmos/node/v1.3/senders/{sender_id}"))["label"],
        ""
    );

    patch_json(port, &sender_path, serde_json::json!({ "label": null }));
    {
        let update = events.lock().unwrap().last().unwrap().clone();
        assert_eq!(update.annotation.label_update, Update::Reset);
        assert_eq!(update.annotation.label.as_deref(), Some("session-default"));
    }
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
        let update = events.lock().unwrap().last().unwrap().clone();
        assert_eq!(update.annotation.tags_update, Update::Set);
        assert_eq!(
            tag_values(&update.annotation.tags, "foo"),
            Some(["a".into(), "b".into()].as_slice())
        );
        assert_eq!(
            tag_values(&update.annotation.tags, "bar"),
            Some([].as_slice())
        );
        assert!(
            update
                .annotation
                .tags
                .iter()
                .all(|tag| tag.key != "urn:x-nvnmos:tag:name")
        );
    }

    patch_json(
        port,
        &sender_path,
        serde_json::json!({ "tags": { "foo": null } }),
    );
    {
        let update = events.lock().unwrap().last().unwrap().clone();
        assert_eq!(update.annotation.tags_update, Update::Set);
        assert!(tag_values(&update.annotation.tags, "foo").is_none());
        assert_eq!(
            tag_values(&update.annotation.tags, "bar"),
            Some([].as_slice())
        );
        assert_eq!(
            update
                .annotation
                .tags
                .iter()
                .find(|tag| tag.key == "bar")
                .map(|tag| tag.values_update),
            Some(Update::Unchanged)
        );
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
        let update = events.last().unwrap();
        assert_eq!(update.annotation.tags_update, Update::Unchanged);
        assert_eq!(update.annotation.label_update, Update::Unchanged);
        assert_eq!(update.annotation.label.as_deref(), Some("session-default"));
        assert_eq!(
            tag_values(&update.annotation.tags, "bar"),
            Some([].as_slice())
        );
    }

    patch_json(
        port,
        "/x-nmos/annotation/v1.0/node/self/",
        serde_json::json!({ "label": null }),
    );
    {
        let update = events.lock().unwrap().last().unwrap().clone();
        assert_eq!(update.resource_type, ResourceType::Node);
        assert_eq!(update.name, None);
        assert_eq!(update.annotation.label_update, Update::Reset);
        assert_eq!(update.annotation.label.as_deref(), Some("node-default"));
    }
    assert_eq!(
        get_json(port, "/x-nmos/node/v1.3/self")["label"],
        "node-default"
    );

    server
        .update_annotation(
            ResourceType::Sender,
            Some(sender_name),
            &Annotation {
                description_update: Update::Set,
                description: Some("from-update".into()),
                tags_update: Update::Set,
                tags: vec![AnnotationTag {
                    key: "bar".into(),
                    values_update: Update::Reset,
                    values: Vec::new(),
                }],
                ..Annotation::default()
            },
        )
        .expect("annotation update");
    {
        let update = events.lock().unwrap().last().unwrap().clone();
        assert_eq!(update.annotation.description_update, Update::Set);
        assert_eq!(
            update.annotation.description.as_deref(),
            Some("from-update")
        );
        assert_eq!(update.annotation.tags_update, Update::Set);
        assert!(tag_values(&update.annotation.tags, "bar").is_none());
    }
    let sender = get_json(port, &format!("/x-nmos/node/v1.3/senders/{sender_id}"));
    assert_eq!(sender["description"], "from-update");
    assert!(sender["tags"].get("bar").is_none());
    assert_eq!(sender["tags"]["urn:x-nvnmos:tag:name"][0], sender_name);

    let sender_url = format!("/x-nmos/node/v1.3/senders/{sender_id}");
    server
        .update_annotation(
            ResourceType::Sender,
            Some(sender_name),
            &Annotation {
                tags_update: Update::Set,
                tags: vec![
                    AnnotationTag {
                        key: "baz".into(),
                        values_update: Update::Set,
                        values: vec!["a".into()],
                    },
                    AnnotationTag {
                        key: "ghost".into(),
                        values_update: Update::Unchanged,
                        values: vec!["nope".into()],
                    },
                ],
                ..Annotation::default()
            },
        )
        .expect("annotation update");
    {
        let update = events.lock().unwrap().last().unwrap().clone();
        assert_eq!(update.annotation.tags_update, Update::Set);
        assert_eq!(
            tag_update(&update.annotation.tags, "baz"),
            Some(Update::Set)
        );
        assert_eq!(
            tag_values(&update.annotation.tags, "baz"),
            Some(["a".to_string()].as_slice())
        );
        assert!(tag_values(&update.annotation.tags, "ghost").is_none());
    }
    let sender = get_json(port, &sender_url);
    assert_eq!(sender["tags"]["baz"][0], "a");
    assert!(sender["tags"].get("ghost").is_none());

    let version = get_json(port, &sender_url)["version"]
        .as_str()
        .expect("sender version")
        .to_string();
    server
        .update_annotation(
            ResourceType::Sender,
            Some(sender_name),
            &Annotation {
                tags_update: Update::Set,
                tags: vec![AnnotationTag {
                    key: "baz".into(),
                    values_update: Update::Unchanged,
                    values: vec!["nope".into()],
                }],
                ..Annotation::default()
            },
        )
        .expect("unchanged tag list");
    {
        let update = events.lock().unwrap().last().unwrap().clone();
        assert_eq!(update.annotation.tags_update, Update::Set);
        assert_eq!(
            tag_update(&update.annotation.tags, "baz"),
            Some(Update::Unchanged)
        );
        assert_eq!(
            tag_values(&update.annotation.tags, "baz"),
            Some(["a".to_string()].as_slice())
        );
    }
    let after = get_json(port, &sender_url);
    assert_eq!(after["tags"]["baz"][0], "a");
    assert_ne!(after["version"].as_str(), Some(version.as_str()));

    let events_before = events.lock().unwrap().len();
    server
        .update_annotation(
            ResourceType::Sender,
            Some(sender_name),
            &Annotation::default(),
        )
        .expect("unchanged update of an existing sender");
    assert_eq!(
        events.lock().unwrap().len(),
        events_before,
        "an unchanged update does not run the callback"
    );
    assert_eq!(
        get_json(port, &sender_url)["version"].as_str(),
        after["version"].as_str(),
        "an unchanged update does not bump the version"
    );
    server
        .update_annotation(
            ResourceType::Sender,
            Some("missing"),
            &Annotation::default(),
        )
        .expect_err("unchanged update of a missing sender");
    server
        .update_annotation(ResourceType::Node, Some("node"), &Annotation::default())
        .expect_err("unchanged update of a Node with a name");

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
            label_update: Update::Set,
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

#[test]
fn annotation_create_skips_unread_tags_and_rejects_reset() {
    let port = ephemeral_port();
    let config = NodeConfig {
        seed: "annotation-create".into(),
        host_addresses: vec!["127.0.0.1".into()],
        http_port: port,
        label: "node-default".into(),
        ..NodeConfig::default()
    };
    let iface_ip = local_iface_ip();
    let server = NodeServer::new(&config).expect("node server");

    let add = |name: &str, annotation: Annotation| {
        server.add_sender(&SenderConfig {
            transport: Transport::Rtp,
            transport_file: minimal_sender_sdp(name, &iface_ip),
            source_annotation: None,
            flow_annotation: None,
            sender_annotation: Some(annotation),
        })
    };

    add(
        "kept",
        Annotation {
            tags_update: Update::Set,
            tags: vec![
                AnnotationTag {
                    key: String::new(),
                    values_update: Update::Unchanged,
                    values: vec!["not-read".into()],
                },
                AnnotationTag {
                    key: "foo".into(),
                    values_update: Update::Set,
                    values: vec!["a".into()],
                },
            ],
            ..Annotation::default()
        },
    )
    .expect("unchanged tag is not read");
    let kept_id = server.sender_id("kept").unwrap().expect("kept sender");
    let kept = get_json(port, &format!("/x-nmos/node/v1.3/senders/{kept_id}"));
    assert_eq!(kept["tags"]["foo"][0], "a");
    assert!(kept["tags"].get("").is_none());

    add(
        "omitted",
        Annotation {
            tags_update: Update::Unchanged,
            tags: vec![AnnotationTag {
                key: String::new(),
                values_update: Update::Set,
                values: vec!["nope".into()],
            }],
            ..Annotation::default()
        },
    )
    .expect("unchanged tag list is not read");
    let omitted_id = server
        .sender_id("omitted")
        .unwrap()
        .expect("omitted sender");
    let omitted = get_json(port, &format!("/x-nmos/node/v1.3/senders/{omitted_id}"));
    assert!(omitted["tags"].get("foo").is_none());

    add(
        "reset-label",
        Annotation {
            label_update: Update::Reset,
            label: Some("x".into()),
            ..Annotation::default()
        },
    )
    .expect_err("label reset at create");
    assert!(server.sender_id("reset-label").unwrap().is_none());

    add(
        "reset-tag",
        Annotation {
            tags_update: Update::Set,
            tags: vec![AnnotationTag {
                key: "foo".into(),
                values_update: Update::Reset,
                values: vec![],
            }],
            ..Annotation::default()
        },
    )
    .expect_err("tag reset at create");
    assert!(server.sender_id("reset-tag").unwrap().is_none());

    drop(server);
}

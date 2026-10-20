//! End-to-end tests: the real binary, real stdio, real Graphviz.

mod common;

use base64::Engine as _;
use common::{decode_png, path_of, wait_for_file, Client, Options, SAMPLE};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn svg_options<'a>() -> roxmltree::ParsingOptions<'a> {
    // Graphviz SVG carries a DOCTYPE.
    roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    }
}

fn diagnostics_for<'a>(uri: &'a str) -> impl Fn(&Value) -> bool + 'a {
    move |n| n["method"] == "textDocument/publishDiagnostics" && n["params"]["uri"] == uri
}

#[test]
fn r1_open_shows_one_png_document() {
    let mut client = Client::start(Options::default());
    let uri = client.open("flow.dot", SAMPLE);
    client.command("graphviz.openPreview", &uri);
    client.pump(Duration::from_millis(500));

    let uris = client.show_document_uris();
    assert_eq!(
        uris.len(),
        1,
        "expected exactly one showDocument, got {uris:?}"
    );
    let shown = &uris[0];
    assert!(shown.starts_with("file://"), "{shown}");
    assert!(shown.ends_with("/flow.png"), "{shown}");
    let params = client.show_documents.lock().unwrap()[0].clone();
    assert_eq!(params["external"], json!(false));
    assert_eq!(params["takeFocus"], json!(true));

    let path = path_of(shown);
    assert!(
        path.starts_with(client.cache.path().canonicalize().unwrap())
            || path.starts_with(client.cache.path())
    );
    let image = decode_png(&std::fs::read(&path).unwrap());
    assert!(image.width > 10 && image.height > 10);
    client.shutdown();
}

#[test]
fn r2_live_change_rewrites_png_without_new_show_document() {
    let mut client = Client::start(Options {
        settings: json!({"render": "live", "debounce_ms": 300}),
        ..Options::default()
    });
    let uri = client.open("flow.dot", SAMPLE);
    client.command("graphviz.openPreview", &uri);
    let path = path_of(&client.show_document_uris()[0]);
    let before = std::fs::read(&path).unwrap();

    let changed = SAMPLE.replace(
        "beta -> gamma;",
        "beta -> gamma;\n  gamma -> delta;\n  delta -> alpha;",
    );
    let sent = Instant::now();
    client.change(&uri, 2, &changed);
    let mut after = before.clone();
    while sent.elapsed() < Duration::from_secs(2) {
        client.pump(Duration::from_millis(20));
        after = std::fs::read(&path).unwrap_or_default();
        if !after.is_empty() && after != before {
            break;
        }
    }
    let took = sent.elapsed();
    assert_ne!(
        after,
        before,
        "PNG did not change within 2 s; log:\n{}",
        client.log()
    );
    assert!(
        took >= Duration::from_millis(300),
        "rendered before the debounce: {took:?}"
    );
    let image = decode_png(&after);
    let old = decode_png(&before);
    assert!(
        image.height > old.height,
        "the longer graph should be taller"
    );

    client.pump(Duration::from_millis(600));
    assert_eq!(
        client.show_document_uris().len(),
        1,
        "a live render sent another showDocument"
    );
    client.shutdown();
}

#[test]
fn save_mode_renders_on_save_only() {
    let mut client = Client::start(Options {
        settings: json!({"render": "save", "debounce_ms": 50}),
        ..Options::default()
    });
    let uri = client.open("flow.dot", SAMPLE);
    client.command("graphviz.openPreview", &uri);
    let path = path_of(&client.show_document_uris()[0]);
    let before = std::fs::read(&path).unwrap();

    let changed = SAMPLE.replace("beta -> gamma;", "beta -> gamma;\n  gamma -> delta;");
    client.change(&uri, 2, &changed);
    client.pump(Duration::from_millis(800));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "rendered before the save"
    );

    client.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": uri}}),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::read(&path).unwrap() == before {
        assert!(
            Instant::now() < deadline,
            "no render after save; log:\n{}",
            client.log()
        );
        client.pump(Duration::from_millis(20));
    }
    assert!(client.log().contains("kind=save"));
    assert_eq!(client.show_document_uris().len(), 1);
    client.shutdown();
}

#[test]
fn r3_opening_twice_reuses_the_same_uri() {
    let mut client = Client::start(Options::default());
    let uri = client.open("flow.dot", SAMPLE);
    client.command("graphviz.openPreview", &uri);
    client.command("graphviz.openPreview", &uri);
    client.pump(Duration::from_millis(200));
    let uris = client.show_document_uris();
    assert_eq!(uris.len(), 2, "{uris:?}");
    assert_eq!(uris[0], uris[1]);

    // A file with the same name in another folder gets its own preview.
    std::fs::create_dir(client.workspace.path().join("other")).unwrap();
    let other = client.open("other/flow.dot", SAMPLE);
    client.command("graphviz.openPreview", &other);
    let uris = client.show_document_uris();
    assert_eq!(uris.len(), 3);
    assert!(uris[2].ends_with("/flow.png"));
    assert_ne!(uris[2], uris[0]);
    client.shutdown();
}

#[test]
fn r4_dot_renderer() {
    let mut client = Client::start(Options {
        settings: json!({"renderer": "dot"}),
        ..Options::default()
    });
    let uri = client.open("flow.dot", SAMPLE);
    client.command("graphviz.openPreview", &uri);
    let path = path_of(&client.show_document_uris()[0]);
    decode_png(&std::fs::read(path).unwrap());
    let log = client.log();
    assert!(log.contains("renderer: engine=dot"), "{log}");
    assert!(log.contains("render: engine=dot"), "{log}");
    client.shutdown();
}

#[cfg(feature = "bundled")]
#[test]
fn r4_bundled_renderer_without_dot_on_path() {
    let empty = tempfile::tempdir().unwrap();
    assert!(
        which::which_in("dot", Some(empty.path()), empty.path()).is_err(),
        "the test PATH must not contain dot"
    );
    let mut client = Client::start(Options {
        settings: json!({"renderer": "bundled", "timeout_ms": 120000}),
        path_env: Some(empty.path().to_path_buf()),
    });
    let uri = client.open("flow.dot", SAMPLE);
    client.command("graphviz.openPreview", &uri);
    let uris = client.show_document_uris();
    assert_eq!(uris.len(), 1, "log:\n{}", client.log());
    decode_png(&std::fs::read(path_of(&uris[0])).unwrap());

    client.command("graphviz.exportSvg", &uri);
    let svg_path = client.workspace.path().join("flow.svg");
    let svg = std::fs::read_to_string(&svg_path).unwrap();
    let doc = roxmltree::Document::parse_with_options(&svg, svg_options()).expect("valid SVG");
    let labels: Vec<&str> = doc
        .descendants()
        .filter(|n| n.has_tag_name("text"))
        .filter_map(|n| n.text())
        .collect();
    for label in ["alpha", "beta", "gamma"] {
        assert!(labels.contains(&label), "{label} missing from {labels:?}");
    }
    // Graphviz messages from the WebAssembly build become diagnostics too.
    let broken = "// A small sample graph\ndigraph flow {\n  alpha -> beta;\n  beta -> ;\n}\n";
    let since = client.notifications.len();
    client.change(&uri, 2, broken);
    let note = client
        .wait_notification(since, Duration::from_secs(10), |n| {
            diagnostics_for(&uri)(n)
                && n["params"]["diagnostics"]
                    .as_array()
                    .is_some_and(|d| !d.is_empty())
        })
        .unwrap_or_else(|| panic!("no diagnostics; log:\n{}", client.log()));
    assert_eq!(
        note["params"]["diagnostics"][0]["range"]["start"]["line"],
        3
    );
    assert!(note["params"]["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains("syntax error in line 4"));

    let log = client.log();
    assert!(log.contains("renderer: engine=bundled"), "{log}");
    assert!(log.contains("render: engine=bundled"), "{log}");
    assert!(!log.contains("engine=dot"), "{log}");
    client.shutdown();
}

#[test]
fn r5_syntax_error_becomes_a_diagnostic_and_keeps_the_png() {
    let mut client = Client::start(Options::default());
    let uri = client.open("flow.dot", SAMPLE);
    client.command("graphviz.openPreview", &uri);
    let path = path_of(&client.show_document_uris()[0]);
    let good = std::fs::read(&path).unwrap();

    // Line 4 (1-based) is broken.
    let broken = "// A small sample graph\ndigraph flow {\n  alpha -> beta;\n  beta -> ;\n}\n";
    let since = client.notifications.len();
    client.change(&uri, 2, broken);
    let note = client
        .wait_notification(since, Duration::from_secs(5), |n| {
            diagnostics_for(&uri)(n)
                && n["params"]["diagnostics"]
                    .as_array()
                    .is_some_and(|d| !d.is_empty())
        })
        .unwrap_or_else(|| panic!("no diagnostics; log:\n{}", client.log()));
    let diagnostics = note["params"]["diagnostics"].as_array().unwrap();
    let error = &diagnostics[0];
    assert_eq!(error["source"], "graphviz");
    assert_eq!(error["severity"], 1);
    assert_eq!(error["range"]["start"]["line"], 3, "{error}");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("syntax error in line 4"));
    assert_eq!(note["params"]["version"], 2);

    client.pump(Duration::from_millis(300));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        good,
        "the last good PNG must stay untouched"
    );

    // Fixing the error clears the diagnostics.
    let since = client.notifications.len();
    client.change(&uri, 3, SAMPLE);
    let cleared = client
        .wait_notification(since, Duration::from_secs(5), |n| {
            diagnostics_for(&uri)(n) && n["params"]["version"] == 3
        })
        .expect("diagnostics after the fix");
    assert_eq!(cleared["params"]["diagnostics"], json!([]));
    client.shutdown();
}

#[test]
fn r5_save_reports_diagnostics_without_a_preview() {
    let mut client = Client::start(Options {
        settings: json!({"render": "save"}),
        ..Options::default()
    });
    let uri = client.open("flow.dot", "digraph {\n  a -> }\n");
    let first = client
        .wait_notification(0, Duration::from_secs(5), |n| {
            diagnostics_for(&uri)(n)
                && n["params"]["diagnostics"]
                    .as_array()
                    .is_some_and(|d| !d.is_empty())
        })
        .expect("diagnostics on open");
    assert_eq!(
        first["params"]["diagnostics"][0]["range"]["start"]["line"],
        1
    );
    let since = client.notifications.len();
    client.change(&uri, 2, "digraph {\n  a -> b\n}\n");
    client.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": uri}}),
    );
    let cleared = client
        .wait_notification(since, Duration::from_secs(5), diagnostics_for(&uri))
        .expect("diagnostics after save");
    assert_eq!(cleared["params"]["diagnostics"], json!([]));
    assert!(client.show_document_uris().is_empty());
    client.shutdown();
}

#[test]
fn r6_hover_on_the_header_returns_a_thumbnail() {
    let mut client = Client::start(Options::default());
    let uri = client.open("flow.dot", SAMPLE);
    let hover = client.request(
        "textDocument/hover",
        json!({"textDocument": {"uri": uri}, "position": {"line": 1, "character": 3}}),
    );
    let value = hover["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("{hover}"));
    assert_eq!(hover["contents"]["kind"], "markdown");
    let start =
        value.find("data:image/png;base64,").expect("data URI") + "data:image/png;base64,".len();
    let end = start + value[start..].find(')').expect("closing paren");
    let png = base64::engine::general_purpose::STANDARD
        .decode(&value[start..end])
        .expect("valid base64");
    let image = decode_png(&png);
    assert!(image.width <= 480, "thumbnail too wide: {}", image.width);
    assert!(value.contains("Open Graphviz preview"));

    // Not on the header: nothing.
    let other = client.request(
        "textDocument/hover",
        json!({"textDocument": {"uri": uri}, "position": {"line": 2, "character": 3}}),
    );
    assert_eq!(other, Value::Null);
    // The comment line above the header: nothing either.
    let comment = client.request(
        "textDocument/hover",
        json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 3}}),
    );
    assert_eq!(comment, Value::Null);
    client.shutdown();
}

#[test]
fn r7_exports_are_valid_and_light_even_in_dark_mode() {
    let mut client = Client::start(Options {
        settings: json!({"appearance": "dark"}),
        ..Options::default()
    });
    let uri = client.open("flow.dot", SAMPLE);
    let since = client.notifications.len();
    client.command("graphviz.exportSvg", &uri);
    client.command("graphviz.exportPng", &uri);

    let svg_path = client.workspace.path().join("flow.svg");
    let png_path = client.workspace.path().join("flow.png");
    let svg = std::fs::read_to_string(&svg_path).expect("flow.svg next to the source");
    let doc = roxmltree::Document::parse_with_options(&svg, svg_options()).expect("valid SVG");
    assert!(doc.root_element().has_tag_name("svg"));
    assert!(!svg.contains("#282c33"), "export used the dark palette");

    let image = decode_png(&std::fs::read(&png_path).expect("flow.png next to the source"));
    assert!(
        image.luma(0, 0) > 240.0,
        "export background is not light: {:?}",
        image.pixel(0, 0)
    );

    let messages: Vec<String> = client.notifications[since..]
        .iter()
        .filter(|n| n["method"] == "window/showMessage")
        .map(|n| {
            n["params"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert!(
        messages.contains(&"Exported flow.svg".to_string()),
        "{messages:?}"
    );
    assert!(
        messages.contains(&"Exported flow.png".to_string()),
        "{messages:?}"
    );
    client.shutdown();
}

#[test]
fn r8_dark_appearance_and_file_colours() {
    let mut client = Client::start(Options {
        settings: json!({"appearance": "dark"}),
        ..Options::default()
    });
    let uri = client.open("dark.dot", SAMPLE);
    client.command("graphviz.openPreview", &uri);
    let path = path_of(&client.show_document_uris()[0]);
    let image = decode_png(&std::fs::read(&path).unwrap());
    assert!(
        image.luma(0, 0) < 80.0,
        "corner is not dark: {:?}",
        image.pixel(0, 0)
    );
    assert_eq!(image.pixel(0, 0)[3], 255);

    let white = SAMPLE.replace("digraph flow {", "digraph flow {\n  bgcolor=white;");
    let uri = client.open("white.dot", &white);
    client.command("graphviz.openPreview", &uri);
    let path = path_of(&client.show_document_uris()[1]);
    let bytes = wait_for_file(&path, Duration::from_secs(5)).unwrap();
    let image = decode_png(&bytes);
    assert_eq!(
        image.pixel(0, 0),
        [255, 255, 255, 255],
        "the file's bgcolor must win"
    );
    client.shutdown();
}

#[test]
fn code_actions_offer_the_three_commands() {
    let mut client = Client::start(Options::default());
    let uri = client.open("flow.dot", SAMPLE);
    let actions = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri},
            "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 0}},
            "context": {"diagnostics": []},
        }),
    );
    let titles: Vec<&str> = actions
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        [
            "Open Graphviz preview",
            "Export graph as SVG",
            "Export graph as PNG"
        ]
    );
    let commands: Vec<&str> = actions
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["command"]["command"].as_str().unwrap())
        .collect();
    assert_eq!(
        commands,
        [
            "graphviz.openPreview",
            "graphviz.exportSvg",
            "graphviz.exportPng"
        ]
    );
    assert_eq!(actions[0]["command"]["arguments"], json!([uri]));
    assert!(actions[0].get("edit").is_none());

    // Format-on-save style requests for specific kinds get nothing.
    let filtered = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri},
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
            "context": {"diagnostics": [], "only": ["source.organizeImports"]},
        }),
    );
    assert_eq!(filtered, json!([]));
    client.shutdown();
}

#[test]
fn render_timeout_stops_graphviz_and_reports_it() {
    // A dense graph that takes dot far longer than the 200 ms budget.
    let text = dense_graph();
    let mut client = Client::start(Options {
        settings: json!({"timeout_ms": 200, "renderer": "dot"}),
        ..Options::default()
    });
    let started = Instant::now();
    let uri = client.open("big.dot", &text);
    let note = client
        .wait_notification(0, Duration::from_secs(10), |n| {
            diagnostics_for(&uri)(n)
                && n["params"]["diagnostics"]
                    .as_array()
                    .is_some_and(|d| !d.is_empty())
        })
        .unwrap_or_else(|| panic!("no timeout diagnostic; log:\n{}", client.log()));
    assert!(started.elapsed() < Duration::from_secs(5));
    let message = note["params"]["diagnostics"][0]["message"]
        .as_str()
        .unwrap();
    assert!(
        message.contains("did not finish within 200 ms"),
        "{message}"
    );
    client.shutdown();
}

/// A graph that makes Graphviz recurse deeply (and takes it many seconds).
fn dense_graph() -> String {
    let mut text = String::from("digraph big {\n");
    for i in 0..400 {
        for j in (i + 1)..(i + 12).min(400) {
            text.push_str(&format!("  n{i} -> n{j};\n"));
        }
    }
    text.push_str("}\n");
    text
}

#[cfg(feature = "bundled")]
#[test]
fn bundled_graphviz_has_the_stack_for_deep_recursion_and_is_bounded() {
    // With wasmtime's default 512 KiB stack this graph traps with a stack
    // overflow after about two seconds; it must instead keep running until
    // the timeout stops it.
    let empty = tempfile::tempdir().unwrap();
    let mut client = Client::start(Options {
        settings: json!({"renderer": "bundled", "timeout_ms": 4000}),
        path_env: Some(empty.path().to_path_buf()),
    });
    let uri = client.open("big.dot", &dense_graph());
    let note = client
        .wait_notification(0, Duration::from_secs(60), |n| {
            diagnostics_for(&uri)(n)
                && n["params"]["diagnostics"]
                    .as_array()
                    .is_some_and(|d| !d.is_empty())
        })
        .unwrap_or_else(|| panic!("no diagnostic; log:\n{}", client.log()));
    let message = note["params"]["diagnostics"][0]["message"]
        .as_str()
        .unwrap();
    assert!(
        message.contains("did not finish within 4000 ms"),
        "{message}"
    );
    client.shutdown();
}

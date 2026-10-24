use super::*;
use crate::artifacts::source_io::with_before_open;

// Swap only after canonicalization and boundary validation have completed.
// Exercise the exact same open primitive through the HTML and image routes.
fn rejects_swap(image: bool, scratch: bool, swap: &str) {
    let fixture = Fixture::new();
    let root = if scratch {
        &fixture.scratch
    } else {
        &fixture.workspace
    };
    let directory = root.join("nested");
    let name = if image { "hero.png" } else { "mock.html" };
    let target = directory.join(name);
    fixture.write(&target, if image { PNG } else { b"<p>original</p>" });
    let outside = fixture.other_thread.join(name);
    fixture.write(&outside, if image { OTHER_PNG } else { b"<p>escaped</p>" });
    let source = if image {
        let source = root.join("mock.html");
        fixture.write(&source, "<img src=nested/hero.png>");
        source
    } else {
        target.clone()
    };
    let changed = target.clone();
    let outside_dir = fixture.other_thread.clone();
    let swap = swap.to_owned();
    let error = with_before_open(
        &target,
        move || match swap.as_str() {
            "symlink" => {
                std::fs::remove_file(&changed).unwrap();
                std::os::unix::fs::symlink(&outside, &changed).unwrap();
            }
            "ancestor" => {
                std::fs::rename(&directory, directory.with_file_name("original")).unwrap();
                std::os::unix::fs::symlink(&outside_dir, &directory).unwrap();
            }
            "fifo" => {
                std::fs::remove_file(&changed).unwrap();
                rustix::fs::mkfifoat(rustix::fs::CWD, &changed, rustix::fs::Mode::RUSR).unwrap();
            }
            "ancestor-fifo" => {
                std::fs::rename(&directory, directory.with_file_name("original")).unwrap();
                rustix::fs::mkfifoat(rustix::fs::CWD, &directory, rustix::fs::Mode::RUSR).unwrap();
            }
            _ => unreachable!(),
        },
        || fixture.capture(&source),
    )
    .unwrap_err();
    assert!(
        error.contains("cannot be opened") || error.contains("not a regular file"),
        "{error}"
    );
    assert_eq!(fixture.stored_count(), (0, 0));
}

#[test]
fn source_and_image_symlink_swaps_are_refused() {
    for image in [false, true] {
        for scratch in [false, true] {
            rejects_swap(image, scratch, "symlink");
        }
    }
}

#[test]
fn source_and_image_ancestor_swaps_are_refused() {
    for image in [false, true] {
        for scratch in [false, true] {
            rejects_swap(image, scratch, "ancestor");
        }
    }
}

#[test]
fn source_and_image_fifo_swaps_do_not_block_or_save() {
    for image in [false, true] {
        for scratch in [false, true] {
            rejects_swap(image, scratch, "fifo");
            rejects_swap(image, scratch, "ancestor-fifo");
        }
    }
}

#[test]
fn replacing_a_root_does_not_redirect_its_open_descriptor() {
    for image in [false, true] {
        for scratch in [false, true] {
            let fixture = Fixture::new();
            let root = if scratch {
                &fixture.scratch
            } else {
                &fixture.workspace
            };
            let target = root.join(if image { "hero.png" } else { "mock.html" });
            fixture.write(&target, if image { PNG } else { b"<p>original</p>" });
            let source = root.join("mock.html");
            if image {
                fixture.write(&source, "<img src=hero.png>");
            }
            let outside = fixture.other_thread.clone();
            fixture.write(&outside.join("hero.png"), OTHER_PNG);
            fixture.write(&outside.join("mock.html"), "<p>escaped</p>");
            let original_root = root.clone();
            let moved_root = root.with_extension("moved");
            let moved_cleanup = moved_root.clone();
            let record = with_before_open(
                &target,
                move || {
                    std::fs::rename(&original_root, &moved_root).unwrap();
                    std::os::unix::fs::symlink(&outside, &original_root).unwrap();
                },
                || fixture.capture(&source),
            )
            .unwrap();
            let expected = if image {
                format!("<img src={}>", data_uri("image/png", PNG))
            } else {
                "<p>original</p>".to_owned()
            };
            assert_eq!(fixture.stored(&record), expected.as_bytes());
            std::fs::remove_file(root).unwrap();
            std::fs::rename(moved_cleanup, root).unwrap();
        }
    }
}

fn css_document(css: &str, attribute: bool) -> String {
    if attribute {
        format!("<p style='background:{css};color:red'>unchanged</p>")
    } else {
        format!("<style>p{{background:{css};color:red}}</style>")
    }
}

#[test]
fn escaped_css_url_names_and_comments_are_inlined() {
    let fixture = Fixture::new();
    fixture.write(&fixture.workspace.join("hero.png"), PNG);
    for css in [
        r"\75rl(hero.png)",
        r#"url("hero.png"/* comment */)"#,
        r#"\000075RL( "hero.png" /*after*/ )"#,
    ] {
        for attribute in [false, true] {
            let source = css_document(css, attribute);
            fixture.write(&fixture.workspace.join("mock.html"), &source);
            let record = fixture.capture("mock.html").unwrap();
            let expected = source.replace(css, &format!("url({})", data_uri("image/png", PNG)));
            assert_eq!(fixture.stored(&record), expected.as_bytes());
        }
    }
    let source = r#"<style>/* \75rl(missing.png) url("missing.png") */ p{content:'url(missing.png)';background:url(hero.png)}</style>"#;
    fixture.write(&fixture.workspace.join("mock.html"), source);
    let record = fixture.capture("mock.html").unwrap();
    assert_eq!(
        fixture.stored(&record),
        source
            .replace(
                "url(hero.png)",
                &format!("url({})", data_uri("image/png", PNG))
            )
            .as_bytes()
    );
}

#[test]
fn escaped_css_urls_with_missing_or_outside_images_save_nothing() {
    for attribute in [false, true] {
        for escaped_name in [false, true] {
            for outside in [false, true] {
                let fixture = Fixture::new();
                let path = if outside {
                    let path = fixture.other_thread.join("hero.png");
                    fixture.write(&path, OTHER_PNG);
                    path.to_string_lossy().into_owned()
                } else {
                    "missing.png".to_owned()
                };
                let css = if escaped_name {
                    format!(r"\75rl({path})")
                } else {
                    format!(r#"url("{path}"/* comment */)"#)
                };
                fixture.write(
                    &fixture.workspace.join("mock.html"),
                    css_document(&css, attribute),
                );
                let error = fixture.capture("mock.html").unwrap_err();
                assert!(error.contains(&path), "{error}");
                assert!(
                    error.contains(if outside { "outside" } else { "not found" }),
                    "{error}"
                );
                assert_eq!(fixture.stored_count(), (0, 0));
            }
        }
    }
}

#[test]
fn uppercase_non_ascii_and_legacy_entities_work_in_img_and_style() {
    let fixture = Fixture::new();
    for name in [
        "a&b.png",
        "\"hero\".png",
        "café.png",
        "fj.png",
        "\u{2afd}\u{20e5}.png",
        "&notit;.png",
        "a&AMP=b.png",
    ] {
        fixture.write(&fixture.workspace.join(name), PNG);
    }
    let png = data_uri("image/png", PNG);
    let references = [
        "a&AMP;b.png",
        "&QUOT;hero&QUOT;.png",
        "caf&eacute;.png",
        "caf&eacute.png",
        "&fjlig;.png",
        "&nparsl;.png",
        "&notit;.png",
        "a&AMP=b.png",
    ];
    for written in references {
        let source = format!(
            "<img src='{written}' data-kept='&eacute;'><p style='color:red;background:url(&QUOT;{written}&QUOT;);border:0'>é</p>"
        );
        // The quote filename needs CSS escapes inside the enclosing CSS string.
        let source = if written == "&QUOT;hero&QUOT;.png" {
            source.replace(
                "url(&QUOT;&QUOT;hero&QUOT;.png&QUOT;)",
                "url(&QUOT;\\&QUOT;hero\\&QUOT;.png&QUOT;)",
            )
        } else {
            source
        };
        fixture.write(&fixture.workspace.join("mock.html"), &source);
        let record = fixture
            .capture("mock.html")
            .unwrap_or_else(|e| panic!("{source}: {e}"));
        let expected = format!(
            "<img src='{png}' data-kept='&eacute;'><p style='color:red;background:url({png});border:0'>é</p>"
        );
        assert_eq!(fixture.stored(&record), expected.as_bytes(), "{source}");
    }
}

#[test]
fn every_whatwg_named_entity_and_legacy_exception_is_decoded() {
    let mut count = 0;
    for (name, &(first, second)) in NAMED_ENTITIES.entries() {
        if first == 0 {
            continue;
        }
        count += 1;
        let mut expected = char::from_u32(first).unwrap().to_string();
        if second != 0 {
            expected.push(char::from_u32(second).unwrap());
        }
        let raw = format!("&{name}.png");
        let decoded = Decoded::new(&raw, CharacterReferences::Attribute);
        assert_eq!(decoded.text, format!("{expected}.png"), "{name}");
        assert_eq!(
            Decoded::new(&raw, CharacterReferences::Text).text,
            format!("{expected}.png"),
            "{name} in Data/RCDATA"
        );
        assert_eq!(Decoded::new(&raw, CharacterReferences::Literal).text, raw);
        assert_eq!(
            &raw[decoded.raw_range(0..expected.len())],
            format!("&{name}")
        );
        if !name.ends_with(';') {
            for suffix in ["a", "=", "0"] {
                let raw = format!("&{name}{suffix}");
                assert_eq!(
                    Decoded::new(&raw, CharacterReferences::Attribute).text,
                    raw,
                    "{name}"
                );
                assert_eq!(
                    Decoded::new(&raw, CharacterReferences::Text).text,
                    format!("{expected}{suffix}"),
                    "{name} in Data/RCDATA"
                );
            }
        }
    }
    assert_eq!(count, 2231);
    assert_eq!(
        Decoded::new(
            "&#128;&#x80;&#0;&#xD800;&#999999999999999999;",
            CharacterReferences::Attribute,
        )
        .text,
        "€€���"
    );
}

#[test]
fn excessive_css_nesting_is_rejected_instead_of_missing_references() {
    let nested = |depth| format!("{}url(hero.png){}", "f(".repeat(depth), ")".repeat(depth));
    assert_eq!(css_urls(&nested(128)).unwrap().len(), 1);
    assert!(css_urls(&nested(129)).unwrap_err().contains("128"));
    let fixture = Fixture::new();
    fixture.write(
        &fixture.workspace.join("mock.html"),
        format!(
            "<style>{}url(missing.png){}</style>",
            "f(".repeat(129),
            ")".repeat(129)
        ),
    );
    let error = fixture.capture("mock.html").unwrap_err();
    assert!(error.contains("nesting exceeds 128"), "{error}");
    assert_eq!(fixture.stored_count(), (0, 0));
}

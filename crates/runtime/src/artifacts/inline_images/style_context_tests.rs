use super::*;

#[test]
fn style_character_references_follow_html_context_and_preserve_raw_spans() {
    // The alternate file exists and differs, so a successful capture alone
    // cannot hide an attribute/Data/RawText context mix-up.
    for (source, correct, decoy) in [
        (
            "<svg><style>.hero{fill:url(&notit;.png)}</style></svg>",
            "¬it;.png",
            "&notit;.png",
        ),
        (
            "<math><style>.hero{fill:url(a&AMP=b.png)}</style></math>",
            "a&=b.png",
            "a&AMP=b.png",
        ),
        (
            "<svg><path style='fill:url(&notit;.png)'/></svg>",
            "&notit;.png",
            "¬it;.png",
        ),
        (
            "<style>.hero{fill:url(a&amp;b.png)}</style>",
            "a&amp;b.png",
            "a&b.png",
        ),
        (
            "<svg><style>.hero{fill:url(a&amp;b.png)}</style></svg>",
            "a&b.png",
            "a&amp;b.png",
        ),
    ] {
        for present in [true, false] {
            let fixture = Fixture::new();
            let target = fixture.workspace.join(correct);
            if present {
                fixture.write(&target, PNG);
            }
            fixture.write(&fixture.workspace.join(decoy), OTHER_PNG);
            fixture.write(&fixture.workspace.join("mock.html"), source);
            if present {
                let record = fixture.capture("mock.html").unwrap();
                let start = source.find("url(").unwrap();
                let end = source[start..].find(')').unwrap() + start + 1;
                let expected = format!(
                    "{}url({}){}",
                    &source[..start],
                    data_uri("image/png", PNG),
                    &source[end..]
                );
                assert_eq!(fixture.stored(&record), expected.as_bytes(), "{source}");
                assert_eq!(record.inlined_assets, [asset(&target, PNG)]);
            } else {
                let error = fixture.capture("mock.html").unwrap_err();
                assert!(error.contains(correct), "{source}: {error}");
                assert!(error.contains("not found"), "{source}: {error}");
                assert_eq!(fixture.stored_count(), (0, 0));
            }
        }
    }
}

#[test]
fn foreign_style_cdata_is_refused_without_decoding_or_saving() {
    for namespace in ["svg", "math"] {
        for files in [0, 1, 2, 3] {
            let fixture = Fixture::new();
            if files & 1 != 0 {
                fixture.write(&fixture.workspace.join("a&amp;b.png"), PNG);
            }
            if files & 2 != 0 {
                fixture.write(&fixture.workspace.join("a&b.png"), OTHER_PNG);
            }
            fixture.write(
                &fixture.workspace.join("mock.html"),
                format!(
                    "<{namespace}><style><![CDATA[.hero{{fill:url(a&amp;b.png)}}]]></style></{namespace}>"
                ),
            );
            let error = fixture.capture("mock.html").unwrap_err();
            assert!(error.contains("unsupported style content"), "{error}");
            assert!(error.contains("CDATA"), "{error}");
            assert_eq!(fixture.stored_count(), (0, 0));
        }
    }
}

#[test]
fn foreign_style_split_urls_are_refused_for_existing_missing_and_outside_targets() {
    for namespace in ["svg", "math"] {
        for target in ["existing", "missing", "outside"] {
            let fixture = Fixture::new();
            fixture.write(&fixture.workspace.join("hero.png"), PNG);
            let path = match target {
                "existing" => "hero".to_owned(),
                "missing" => "missing".to_owned(),
                "outside" => {
                    let path = fixture.other_thread.join("outside");
                    fixture.write(&path.with_extension("png"), OTHER_PNG);
                    path.to_string_lossy().into_owned()
                }
                _ => unreachable!(),
            };
            for css in [
                format!(".hero{{fill:url(<![CDATA[{path}]]>.png)}}"),
                format!(".hero{{fill:url({path}<![CDATA[.png]]>)}}"),
                format!(".hero{{fill:u<![CDATA[rl]]>({path}.png)}}"),
                format!(".hero{{fill:url({path}<!-- split -->.png)}}"),
                format!(".hero{{fill:url({path}<g></g>.png)}}"),
            ] {
                // A valid image before the unsupported style must not be saved
                // either: refusal is atomic for the entire capture.
                let source =
                    format!("<img src=hero.png><{namespace}><style>{css}</style></{namespace}>");
                fixture.write(&fixture.workspace.join("mock.html"), &source);
                let error = fixture.capture("mock.html").unwrap_err();
                assert!(
                    error.contains("unsupported style content"),
                    "{source}: {error}"
                );
                assert!(error.contains("foreign <style> content at byte"), "{error}");
                assert_eq!(fixture.stored_count(), (0, 0));
            }
        }
    }
}

#[test]
fn unsupported_foreign_styles_are_listed_with_a_bounded_count() {
    let fixture = Fixture::new();
    let source = "<svg><style><!-- split --></style></svg>".repeat(10);
    fixture.write(&fixture.workspace.join("mock.html"), &source);
    let error = fixture.capture("mock.html").unwrap_err();
    assert_eq!(
        error.matches("\n- foreign <style> content at byte").count(),
        8
    );
    assert!(error.contains("\n- and 2 more"), "{error}");
    assert_eq!(fixture.stored_count(), (0, 0));
}

#[test]
fn whole_style_spans_preserve_raw_html_and_keep_elements_separate() {
    let fixture = Fixture::new();
    fixture.write(&fixture.workspace.join("hero.png"), PNG);
    for source in [
        format!(
            "<style>/*{}*/.a{{background:url(hero.png)}}</style><svg><style>.b{{fill:url(hero&#46;png)}}</style></svg>",
            "é".repeat(10000)
        ),
        "<svg><style/><style></style></svg><style>.a{background:url(hero.png)}".into(),
        "<svg><foreignObject><style>.a{background:url(hero.png)}/* <![CDATA[ */</style></foreignObject></svg>".into(),
        "<svg><style>.a::before{content:'&lt;'} .a{fill:url(hero.png)}</style></svg>".into(),
    ] {
        fixture.write(&fixture.workspace.join("mock.html"), &source);
        let record = fixture.capture("mock.html").unwrap();
        let replacement = format!("url({})", data_uri("image/png", PNG));
        let expected = source
            .replace("url(hero.png)", &replacement)
            .replace("url(hero&#46;png)", &replacement);
        assert_eq!(fixture.stored(&record), expected.as_bytes());
    }
}

#[test]
fn unclosed_foreign_style_is_refused_even_without_text_callbacks() {
    let fixture = Fixture::new();
    for source in ["<svg><style>", "<svg><style>.a{fill:url(missing.png)}"] {
        fixture.write(&fixture.workspace.join("mock.html"), source);
        let error = fixture.capture("mock.html").unwrap_err();
        assert!(error.contains("no explicit closing tag"), "{error}");
        assert_eq!(fixture.stored_count(), (0, 0));
    }
}

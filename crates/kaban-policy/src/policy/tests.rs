use super::*;

fn policy_with(yaml: &str) -> Policy {
    let f = Policy::parse(yaml.as_bytes()).unwrap();
    Policy::from_file(f, "test".into()).unwrap()
}
fn has_rule(v: &Verdict, id: &str) -> bool {
    v.hits.iter().any(|h| h.rule == id)
}

#[test]
fn builtin_catches_aws_key_and_allows_exact_example() {
    let p = Policy::empty();
    assert!(!p.scan("key AKIAIOSFODNN7EXAMPLE is documented").rejected);
    let v = p.scan("key AKIAQWERTYUIOPASDFGH leaked");
    assert!(v.rejected && has_rule(&v, "aws-access-key-id"));
    assert!(
        !serde_json::to_string(&v)
            .unwrap()
            .contains("AKIAQWERTYUIOPASDFGH"),
        "verdict must not echo the secret"
    );
}

#[test]
fn user_words_redact_and_phrases_reject() {
    let p = policy_with(
        "words:\n  - { term: hunter2, action: redact, id: pw }\nphrases:\n  - { term: 'Project Nightjar', id: codename }\n",
    );
    let v = p.scan("the old pw was Hunter2 ok");
    assert!(!v.rejected);
    assert_eq!(v.redacted, "the old pw was [REDACTED:pw] ok");
    assert!(p.scan("about project nightjar launch").rejected);
}

#[test]
fn unicode_and_separator_bypasses_are_caught() {
    let p = policy_with("phrases:\n  - { term: 'Project Nightjar', id: codename }\n");
    for s in [
        "Project  Nightjar",
        "Project\u{00A0}Nightjar",
        "Project-Nightjar",
        "Project_Nightjar",
        "ProjectNightjar",
        "PROJECT\nNIGHTJAR",
        "Pro\u{200B}ject Nightjar",
        "Ｐｒｏｊｅｃｔ Ｎｉｇｈｔｊａｒ",
        "Projéct Nightjar",
    ] {
        assert!(p.scan(s).rejected, "bypass: {s:?}");
    }
    assert!(!p.scan("Project Nightingale").rejected);
}

#[test]
fn redaction_offsets_map_back_through_normalization() {
    let p = policy_with("words:\n  - { term: hunter2, action: redact, id: pw }\n");
    let v = p.scan("note Ｈｕｎｔｅｒ２ done");
    assert_eq!(v.redacted, "note [REDACTED:pw] done");
    let v = p.scan("note hun\u{200B}ter2 done");
    assert_eq!(v.redacted, "note [REDACTED:pw] done");
}

#[test]
fn zero_width_space_inside_aws_key_is_caught_with_raw_offsets() {
    let p = Policy::empty();
    let text = "key AK\u{200B}IAQWERTYUIOPASDFGH leaked";
    let v = p.scan(text);
    assert!(
        v.rejected && has_rule(&v, "aws-access-key-id"),
        "zero-width space must not hide the key: {v:?}"
    );
    let h = v
        .hits
        .iter()
        .find(|h| h.rule == "aws-access-key-id")
        .unwrap();
    assert!(text.is_char_boundary(h.start) && text.is_char_boundary(h.end));
    assert_eq!(&text[h.start..h.end], "AK\u{200B}IAQWERTYUIOPASDFGH");
}

#[test]
fn markdown_escaped_key_name_is_caught() {
    let p = Policy::empty();
    let text = r"set api\_key=Qx9vLm2pRt7wZk leaked";
    let v = p.scan(text);
    assert!(
        v.rejected && has_rule(&v, "generic-credential-assignment"),
        "markdown escape must not hide the assignment: {v:?}"
    );
    let h = v
        .hits
        .iter()
        .find(|h| h.rule == "generic-credential-assignment")
        .unwrap();
    assert_eq!(&text[h.start..h.end], "Qx9vLm2pRt7wZk");
}

#[test]
fn confusable_cyrillic_letter_in_phrase_is_caught() {
    let p = policy_with("phrases:\n  - { term: 'Project Nightjar', id: codename }\n");
    // Cyrillic Er (U+0420) in place of Latin P, Cyrillic a (U+0430) in place of Latin a
    let text = "about \u{0420}roject Nightj\u{0430}r launch";
    let v = p.scan(text);
    assert!(
        v.rejected && has_rule(&v, "codename"),
        "confusable bypass: {v:?}"
    );
    let h = &v.hits[0];
    assert_eq!(&text[h.start..h.end], "\u{0420}roject Nightj\u{0430}r");
    assert!(!p.scan("Project Nightingale").rejected);
}

#[test]
fn credential_assignment_shapes() {
    let p = Policy::empty();
    for s in [
        "password = Sup3rS3cretValue!",
        "DB_PASSWORD=hunter2hunter2",
        "AWS_SECRET_ACCESS_KEY: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        r#"{"password":"s3cretvalue"}"#,
        "export STRIPE_API_KEY='sk_live_abcdefghijklmnop'",
        "postgres://admin:Passw0rd!@db.internal:5432/app",
        "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123",
    ] {
        assert!(p.scan(s).rejected, "should reject: {s}");
    }
    for s in [
        "the password field is required",
        "password = ${DB_PASSWORD}",
        "api_key: <your-key>",
        "password=changeme",
        "commit \"3f2a9c1b7d6e5f4a3b2c1d0e9f8a7b6c5d4e3f2a\" fixed it",
        "uuid \"550e8400-e29b-41d4-a716-446655440000\"",
    ] {
        assert!(!p.scan(s).rejected, "should pass: {s}");
    }
    assert!(
        p.scan("token \"Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MEFCQ0RFRkdISUpL\" here")
            .rejected
    );
}

#[test]
fn credential_class_rules_reject_even_when_default_action_is_redact() {
    let p = policy_with("defaults: { action: redact }\n");
    for s in [
        "password = hunter2hunter2",
        "key AKIAQWERTYUIOPASDFGH leaked",
        "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123",
    ] {
        let v = p.scan(s);
        assert!(
            v.rejected,
            "credential class must reject under redact default: {s}"
        );
        assert_eq!(v.redacted, "");
    }
    // a generic (non-credential) builtin rule follows the default
    let v = p.scan("token \"Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MEFCQ0RFRkdISUpL\" here");
    assert!(!v.rejected && has_rule(&v, "high-entropy-quoted"), "{v:?}");
    assert!(v.redacted.contains("[REDACTED:high-entropy-quoted]"));
    // the kv view too
    assert!(p.scan_kv("password", "hunter2hunter2").rejected);
}

#[test]
fn credential_assignment_covers_prose_markdown_and_table_forms() {
    let p = Policy::empty();
    for s in [
        "GITHUB_TOKEN is ghp_q8Zt3x9LmA",
        "passphrase: correct-horse-battery",
        "| api_key | Qx9vLm2pRt7wZk |",
        "the password was Tr0ub4dor&3 back then",
        "**password**: `hunter2hunter2`",
        "credentials => admin:Passw0rd9",
    ] {
        let v = p.scan(s);
        assert!(v.rejected, "should reject: {s} -> {v:?}");
    }
    for s in [
        "the token is 12 chars long",
        "auth was refreshed yesterday",
        "token = refreshed",
        "rotate the password monthly",
        "| api_key | required |",
    ] {
        let v = p.scan(s);
        assert!(!v.rejected, "false positive: {s} -> {v:?}");
    }
}

#[test]
fn angle_bracket_placeholder_allow_is_tight() {
    let p = Policy::empty();
    assert!(!p.scan("password=<your-key>").rejected);
    assert!(!p.scan("api_key: <API KEY>").rejected);
    assert!(!p.scan_kv("secret", "<redacted_value>").rejected);
    // a real secret wrapped in angle brackets is not a placeholder
    assert!(p.scan("password=<S3cr3t!>").rejected);
    assert!(p.scan_kv("password", "<Tr0ub4dor&3>").rejected);
}

#[test]
fn scan_windows_merges_hits_across_window_boundaries() {
    let p = policy_with("words:\n  - { term: hunter2, action: redact, id: pw }\n");
    // 20-char windows, 10-char stride: the hit straddles the first window's end
    let text = "aaaaaaaaaaaaaaaahunter2 bbbbbbbbbbbbbbbbbbbbbb";
    let v = p.scan_windows(text, 20);
    assert!(!v.rejected);
    assert_eq!(v.hits.len(), 1, "{v:?}");
    assert_eq!(&text[v.hits[0].start..v.hits[0].end], "hunter2");
    assert_eq!(
        v.redacted,
        "aaaaaaaaaaaaaaaa[REDACTED:pw] bbbbbbbbbbbbbbbbbbbbbb"
    );
    assert!(p.scan_windows("short", 20).hits.is_empty());
}

#[test]
fn word_terms_match_only_at_word_boundaries() {
    let p =
        policy_with("words:\n  - { term: ace, id: ace, word: true }\n  - { term: jar, id: jar }\n");
    assert!(!p.scan("in that place").rejected, "ace inside place");
    assert!(!p.scan("spaces everywhere").rejected);
    assert!(p.scan("the ace of spades").rejected);
    assert!(p.scan("ACE").rejected);
    assert!(
        p.scan("a-c-e").rejected,
        "separator-free view still applies"
    );
    // non-word terms keep substring semantics
    assert!(p.scan("nightjar").rejected);
}

#[test]
fn effective_hash_covers_builtin_and_user_rules_not_file_bytes() {
    let a = policy_with("words:\n  - { term: hunter2, id: pw }\n");
    let b = policy_with("words:\n  - {term: hunter2,  id: pw}\n\n");
    assert_eq!(
        a.effective_hash(),
        b.effective_hash(),
        "formatting must not matter"
    );
    let c = policy_with("words:\n  - { term: hunter3, id: pw }\n");
    assert_ne!(a.effective_hash(), c.effective_hash());
    let d =
        policy_with("extend: { disabled_rules: [jwt] }\nwords:\n  - { term: hunter2, id: pw }\n");
    assert_ne!(
        a.effective_hash(),
        d.effective_hash(),
        "builtin set is part of the hash"
    );
    assert_eq!(a.effective_hash().len(), 64);
    assert_ne!(Policy::empty().effective_hash(), "builtin");
}

#[test]
fn unknown_policy_key_fails_to_load_and_names_the_key() {
    let yaml = "defaults:\n  action: reject\n  case_insensitive: true\nallow:\n  - description: documented examples\n    regexes: ['^sk-test-[a-z]{20,}$']\n    stopwords: [example]\n";
    let err = Policy::parse(yaml.as_bytes()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("case_insensitive"),
        "first unknown key named: {msg}"
    );
    let yaml2 = "allow:\n  - description: documented examples\n    regexes: ['^sk-test-[a-z]{20,}$']\n    stopwords: [example]\n";
    let msg2 = Policy::parse(yaml2.as_bytes()).unwrap_err().to_string();
    assert!(
        msg2.contains("stopwords") && msg2.contains("allow"),
        "field path: {msg2}"
    );
    assert!(
        !msg2.contains("example"),
        "term values must not be echoed: {msg2}"
    );
}

#[test]
fn unanchored_allow_regex_fails_to_compile_without_echoing_it() {
    let yaml = "allow:\n  - rule_ids: [openai-api-key]\n    regexes: ['^sk-test-[A-Za-z0-9_-]{20,}$', 'sk-test-']\n";
    let file = Policy::parse(yaml.as_bytes()).unwrap();
    let err = Policy::from_file(file, "t".into()).unwrap_err();
    match &err {
        PolicyError::UnanchoredAllow { rule_ids, index } => {
            assert_eq!(rule_ids, &vec!["openai-api-key".to_string()]);
            assert_eq!(*index, 1);
        }
        other => panic!("expected UnanchoredAllow, got {other:?}"),
    }
    let msg = err.to_string();
    assert!(
        !msg.contains("sk-test"),
        "allow pattern must not be echoed: {msg}"
    );
    assert!(msg.contains("openai-api-key") && msg.contains('1'), "{msg}");
    // anchored on both ends is accepted; `\A`/`\z` count as anchors too
    let ok = "allow:\n  - regexes: ['^(?i)abc$', '\\Aabc\\z']\n";
    assert!(Policy::from_file(Policy::parse(ok.as_bytes()).unwrap(), "t".into()).is_ok());
}

#[test]
fn allowlist_is_per_rule_and_exact() {
    let p = Policy::empty();
    // 'example' as a substring must NOT whitelist a real-looking secret
    assert!(p.scan("password=Example-realSecret9").rejected);
    assert!(
        p.scan("host db1.corp.example.internal password=abcdefgh12")
            .rejected
    );
}

#[test]
fn overlapping_hits_fully_masked() {
    let p = policy_with(
        "words:\n  - { term: abcdef, action: redact, id: a }\n  - { term: defghi, action: redact, id: b }\n",
    );
    let v = p.scan("x abcdefghi y");
    assert_eq!(v.redacted, "x [REDACTED:a] y");
    assert!(!v.redacted.contains("ghi"));
}

#[test]
fn too_long_input_rejected() {
    let p = policy_with("defaults: { max_field_chars: 10 }\n");
    let v = p.scan("12345678901");
    assert!(v.rejected && v.reject_rules == vec!["input-too-long".to_string()]);
}

fn hits_are_char_boundaries(text: &str, v: &Verdict) -> bool {
    v.hits
        .iter()
        .all(|h| h.start < h.end && text.is_char_boundary(h.start) && text.is_char_boundary(h.end))
}

proptest::proptest! {
    /// Extends `redaction_offsets_map_back_through_normalization`: whatever surrounds an
    /// injected literal hit, nothing of the hit reaches storage. The random context can
    /// itself form a genuine credential assignment (`PW:...`), which the builtin rules
    /// reject; rejection is correct product behaviour, so the property then requires that
    /// nothing is returned for storage. Otherwise the redacted output never contains the
    /// hit and every offset is a char boundary of the raw input.
    #[test]
    fn redaction_never_leaks_injected_literal(prefix in "\\PC{0,24}", suffix in "\\PC{0,24}") {
        let p = policy_with("words:\n  - { term: hunter2, action: redact, id: pw }\n");
        let text = format!("{prefix}hunter2{suffix}");
        let v = p.scan(&text);
        proptest::prop_assert!(!v.hits.is_empty(), "hit lost in {text:?}");
        proptest::prop_assert!(hits_are_char_boundaries(&text, &v), "offsets {v:?}");
        if v.rejected {
            proptest::prop_assert!(v.redacted.is_empty(), "a rejection returned text for storage: {:?}", v.redacted);
            proptest::prop_assert!(!v.reject_rules.is_empty(), "rejected without a rule id");
        } else {
            proptest::prop_assert!(!v.redacted.to_lowercase().contains("hunter2"), "leak: {:?}", v.redacted);
        }
    }

    /// Same property for the regex tier: an AWS key id with zero-width code points injected
    /// at random positions is still found on the NFKC view and fully masked in the raw text.
    #[test]
    fn regex_hit_through_ignorables_is_fully_masked(
        prefix in "\\PC{0,16}",
        suffix in "\\PC{0,16}",
        cuts in proptest::collection::vec(1usize..20, 0..4),
    ) {
        // a generic (redactable) user rule with the AWS key shape; credential-class rules
        // always reject, so the masking property is checked on a generic rule
        let p = policy_with(
            "extend: { disabled_rules: [aws-access-key-id] }\nrules:\n  - { id: key-shape, regex: '\\b(AKIA[0-9A-Z]{16})\\b', secret_group: 1, keywords: [AKIA], action: redact }\n",
        );
        let key = "AKIAQWERTYUIOPASDFGH";
        let mut injected = String::new();
        for (i, c) in key.chars().enumerate() {
            if cuts.contains(&i) {
                injected.push('\u{200B}');
            }
            injected.push(c);
        }
        let text = format!("{prefix} {injected} {suffix}");
        let v = p.scan(&text);
        proptest::prop_assert!(!v.rejected);
        proptest::prop_assert!(has_rule(&v, "key-shape"), "key lost in {text:?}: {v:?}");
        proptest::prop_assert!(hits_are_char_boundaries(&text, &v), "offsets {v:?}");
        let visible: String = v.redacted.chars().filter(|c| *c != '\u{200B}').collect();
        proptest::prop_assert!(!visible.contains(key), "leak: {:?}", v.redacted);
    }
}

#[test]
fn entropy_values() {
    assert!(shannon_entropy("aaaa") < 0.01);
    assert!(shannon_entropy("abcdefghijklmnop") > 3.9);
}

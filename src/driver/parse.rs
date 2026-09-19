//! Podman machine-output parsing (the shapes are pinned by fixture
//! tests, so a format change fails a unit test, not a user's command).

/// Pinned (live measurement): podman's inspect JSON holds the labels
/// at `Config.Labels`, NOT at the top level. Fixture-tested so a
/// format change fails a unit test, not a silent `None`.
/// Presence is the marker check (the value, conventionally `true`, is
/// never read).
pub(crate) fn box_marker_from_inspect(v: &serde_json::Value) -> Option<String> {
    v.get(0)
        .and_then(|c| c.get("Config"))
        .and_then(|cfg| cfg.get("Labels"))
        .and_then(|l| l.get("com.github.simon3z.steelbx.box"))
        .and_then(|b| b.as_str())
        .map(str::to_string)
}

/// Pinned: `podman image inspect` JSON holds the WORKDIR at
/// `Config.WorkingDir` (the image's own field — no steelbx label
/// invented). Absent or empty ⇒ the image has no WORKDIR.
pub(crate) fn workdir_from_image_inspect(v: &serde_json::Value) -> Option<String> {
    v.get(0)
        .and_then(|c| c.get("Config"))
        .and_then(|c| c.get("WorkingDir"))
        .and_then(|w| w.as_str())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
}

/// The declared main command: the image's own optional label
/// `com.github.simon3z.steelbx.box.cmd` — presence is the check (the value is
/// conventionally `true` and never read). Set ⇒ the image ships its
/// own complete command (its ENTRYPOINT+CMD), and steelbx appends
/// nothing; absent ⇒ a profile `entry` or the heartbeat.
pub(crate) fn cmd_label_from_image_inspect(v: &serde_json::Value) -> bool {
    v.get(0)
        .and_then(|c| c.get("Config"))
        .and_then(|cfg| cfg.get("Labels"))
        .and_then(|l| l.get("com.github.simon3z.steelbx.box.cmd"))
        .is_some()
}

/// The declared base name: the image's own optional label
/// `com.github.simon3z.steelbx.box.name` (toolbox-style: the distribution unit
/// carries its identity). `create` uses it as the prefix of the generated
/// box name when `-n` is absent. Absent or empty ⇒ no declaration (the
/// image's name component is the base instead).
pub(crate) fn name_label_from_image_inspect(v: &serde_json::Value) -> Option<String> {
    v.get(0)
        .and_then(|c| c.get("Config"))
        .and_then(|cfg| cfg.get("Labels"))
        .and_then(|l| l.get("com.github.simon3z.steelbx.box.name"))
        .and_then(|n| n.as_str())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// The declared runtime env names: the optional label
/// `com.github.simon3z.steelbx.box.env` — a single value holding a
/// comma-separated list of env names (a label is one value; podman
/// labels cannot repeat a key). The same shape on images and
/// containers (`[0].Config.Labels`). Absent, empty, or no valid names
/// ⇒ none; invalid names are dropped (the label is written by humans
/// and image authors — defensive).
pub(crate) fn env_names_from_inspect(v: &serde_json::Value) -> Vec<String> {
    let label = v
        .get(0)
        .and_then(|c| c.get("Config"))
        .and_then(|cfg| cfg.get("Labels"))
        .and_then(|l| l.get("com.github.simon3z.steelbx.box.env"))
        .and_then(|e| e.as_str())
        .unwrap_or("");
    env_names_from_label(label)
}

/// One label value into env names: comma-separated, trimmed, invalid
/// names dropped (an env name is [A-Za-z0-9_]+ — a comma can never be
/// part of one, so the split is unambiguous).
pub(crate) fn env_names_from_label(label: &str) -> Vec<String> {
    label
        .split(',')
        .map(str::trim)
        .filter(|n| crate::validate::is_valid_env_name(n))
        .map(str::to_string)
        .collect()
}

/// Pinned parse: podman's `ps` JSON holds the name in `Names`
/// (an array, docker-style) — NOT a `Name` string. Defensive on the
/// way in, so a format change fails a unit test (fixture), not a
/// user's `ps`.
/// A row is a `BoxRow` (name, state, image, created-age); a container
/// is a box iff it carries the marker (presence — the value is never
/// read).
pub(crate) fn ps_rows_from_json(v: &serde_json::Value) -> Vec<super::BoxRow> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let name = c
                .get("Names")
                .and_then(|n| n.as_array())
                .and_then(|a| a.first())
                .and_then(|s| s.as_str())
                .or_else(|| c.get("Name").and_then(|s| s.as_str()))?;
            // Marker presence: a container without it is not a steelbx box.
            let is_box = c
                .get("Labels")
                .and_then(|l| l.get("com.github.simon3z.steelbx.box"))
                .is_some();
            if !is_box {
                return None;
            }
            Some(super::BoxRow {
                name: name.to_string(),
                state: ps_str(c, "State"),
                image: ps_str(c, "Image"),
                created: ps_str(c, "CreatedAt"),
            })
        })
        .collect()
}

/// A `podman ps` string field; empty when absent (older podman that omits
/// `Image`/`CreatedAt` just leaves the column blank, not an error).
fn ps_str(c: &serde_json::Value, key: &str) -> String {
    c.get(key)
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Defensive: `podman images --format json` rows carry their tag
/// references under different keys across podman versions
/// (`repoTags` in the podman 5 rich format; `Names`/`names`
/// docker-style earlier; a `repository`+`tag` pair in table output).
/// Take the first spelling that yields a reference; `:<none>`
/// rows are untappable and dropped. The `--filter label=` arg has
/// already restricted the list to marked images, so no label field
/// is needed here (the JSON may not carry labels at all).
/// The tag-ref keys, tried in order (podman-version spellings); empty
/// and `:<none>` refs dropped. First key that yields refs wins.
fn refs_from_keys(c: &serde_json::Value) -> Vec<String> {
    for key in ["repoTags", "repo_tags", "Names", "names", "Tags", "tags"] {
        if let Some(arr) = c.get(key).and_then(|a| a.as_array()) {
            let refs: Vec<String> = arr
                .iter()
                .filter_map(|t| t.as_str())
                .filter(|t| !t.is_empty() && !t.ends_with(":<none>"))
                .map(str::to_string)
                .collect();
            if !refs.is_empty() {
                return refs;
            }
        }
    }
    Vec::new()
}

/// The `repository`+`tag` fallback (table output); `:<none>` rows are
/// untappable and yield none.
fn ref_from_repo_tag(c: &serde_json::Value) -> Option<String> {
    let repo = c.get("repository").and_then(|r| r.as_str());
    let tag = c.get("tag").and_then(|t| t.as_str());
    if let (Some(r), Some(t)) = (repo, tag) {
        if !t.is_empty() && t != "<none>" && !r.ends_with(":<none>") {
            return Some(format!("{r}:{t}"));
        }
    }
    None
}

pub(crate) fn image_refs_from_images_json(v: &serde_json::Value) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for c in v.as_array().into_iter().flatten() {
        let mut refs = refs_from_keys(c);
        if let Some(r) = ref_from_repo_tag(c) {
            refs.push(r);
        }
        for r in refs {
            if seen.insert(r.clone()) {
                out.push(r);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pinned: `podman image inspect` JSON — the WORKDIR is the
    // image's own `Config.WorkingDir` field.
    #[test]
    fn image_inspect_workdir_shape_is_pinned() {
        let v: serde_json::Value =
            serde_json::from_str(r#"[{"Config":{"WorkingDir":"/root/.pi"}}]"#).unwrap();
        assert_eq!(workdir_from_image_inspect(&v).as_deref(), Some("/root/.pi"));

        // No WORKDIR (absent) and empty WORKDIR are both "no layout".
        let v: serde_json::Value = serde_json::from_str(r#"[{"Config":{}}]"#).unwrap();
        assert_eq!(workdir_from_image_inspect(&v), None);
        let v: serde_json::Value =
            serde_json::from_str(r#"[{"Config":{"WorkingDir":""}}]"#).unwrap();
        assert_eq!(workdir_from_image_inspect(&v), None);
    }

    // Pinned: the exact shape of `podman inspect` JSON, measured live.
    // Labels live under Config.Labels — NOT at the top level.
    #[test]
    fn inspect_json_marker_shape_is_pinned() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"Config":{"Labels":{"com.github.simon3z.steelbx.box":"true","com.github.simon3z.steelbx.box.name":"pi"}},"Name":"pi-1"}]"#,
        )
        .unwrap();
        assert_eq!(box_marker_from_inspect(&v).as_deref(), Some("true"));

        // Container without the marker → None (not an error).
        let v: serde_json::Value = serde_json::from_str(r#"[{"Config":{"Labels":{}}}]"#).unwrap();
        assert_eq!(box_marker_from_inspect(&v), None);
    }

    // Pinned: the `com.github.simon3z.steelbx.box.cmd` label — presence is the
    // check (any value, even empty); it means "the image's own
    // ENTRYPOINT+CMD is the complete main command".
    #[test]
    fn image_declared_cmd_label_is_pinned() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"Config":{"Labels":{"com.github.simon3z.steelbx.box.cmd":"true"}}}]"#,
        )
        .unwrap();
        assert!(cmd_label_from_image_inspect(&v));
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"Config":{"Labels":{"com.github.simon3z.steelbx.box.cmd":""}}}]"#,
        )
        .unwrap();
        assert!(cmd_label_from_image_inspect(&v));
        let v: serde_json::Value = serde_json::from_str(r#"[{"Config":{"Labels":{}}}]"#).unwrap();
        assert!(!cmd_label_from_image_inspect(&v));
    }

    #[test]
    fn image_declared_name_is_pinned() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"Config":{"Labels":{"com.github.simon3z.steelbx.box":"true","com.github.simon3z.steelbx.box.name":"pi"},"WorkingDir":"/root"}}]"#,
        )
        .unwrap();
        assert_eq!(name_label_from_image_inspect(&v).as_deref(), Some("pi"));
        assert_eq!(workdir_from_image_inspect(&v).as_deref(), Some("/root"));

        // No declaration (absent or empty) ⇒ no name.
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"Config":{"Labels":{"com.github.simon3z.steelbx.box":"true"}}}]"#,
        )
        .unwrap();
        assert_eq!(name_label_from_image_inspect(&v), None);
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"Config":{"Labels":{"com.github.simon3z.steelbx.box.name":""}}}]"#,
        )
        .unwrap();
        assert_eq!(name_label_from_image_inspect(&v), None);
    }

    // Pinned: the `podman ps` JSON shape — the name is `Names` (array,
    // docker-style), NOT a `Name` string; the marker is presence.
    // Pinned: the `box.env` label — one value, comma-separated names,
    // trimmed, invalid names dropped, absent ⇒ none.
    #[test]
    fn env_names_label_is_pinned() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"Config":{"Labels":{"com.github.simon3z.steelbx.box.env":"A, B ,bad-name,,C"}}}]"#,
        )
        .unwrap();
        assert_eq!(
            env_names_from_inspect(&v),
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
        // Absent and empty are both "none".
        let v: serde_json::Value = serde_json::from_str(r#"[{"Config":{"Labels":{}}}]"#).unwrap();
        assert_eq!(env_names_from_inspect(&v), Vec::<String>::new());
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"Config":{"Labels":{"com.github.simon3z.steelbx.box.env":""}}}]"#,
        )
        .unwrap();
        assert_eq!(env_names_from_inspect(&v), Vec::<String>::new());
    }

    #[test]
    fn ps_json_shape_is_pinned() {
        let v: serde_json::Value =
            serde_json::from_str(
                r#"[{"Names":["pi-1"],"State":"created","Image":"quay.io/fedora-toolbox:44","CreatedAt":"8 hours ago","Labels":{"com.github.simon3z.steelbx.box":"true"}},{"Names":["pi-2"],"State":"running"}]"#,
            )
            .unwrap();
        let rows = ps_rows_from_json(&v);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "pi-1");
        assert_eq!(rows[0].state, "created");
        assert_eq!(rows[0].image, "quay.io/fedora-toolbox:44");
        assert_eq!(rows[0].created, "8 hours ago");

        // Marker with any value (even empty) counts: presence is the
        // check.
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"Names":["pi-4"],"State":"created","Labels":{"com.github.simon3z.steelbx.box":""}}]"#,
        )
        .unwrap();
        assert_eq!(ps_rows_from_json(&v).len(), 1);

        // A `Name` string (docker-less variants) is still parsed.
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"Name":"pi-3","State":"created","Labels":{"com.github.simon3z.steelbx.box":"true"}}]"#,
        )
        .unwrap();
        assert_eq!(ps_rows_from_json(&v).len(), 1);
    }

    // Defensive: the `podman images` row shape. Podman 5 rich format
    // first (`repoTags`); `:<none>` dropped; duplicates collapsed.
    #[test]
    fn images_json_refs_shape_is_pinned() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"repoTags":["registry.fedoraproject.org/fedora:42"]},{"repoTags":["mybox:<none>"]},{"repoTags":["registry.fedoraproject.org/fedora:42","registry.fedoraproject.org/fedora:latest"]}]"#,
        )
        .unwrap();
        assert_eq!(
            image_refs_from_images_json(&v),
            vec![
                "registry.fedoraproject.org/fedora:42",
                "registry.fedoraproject.org/fedora:latest"
            ]
        );
    }

    #[test]
    fn images_json_refs_fallback_shapes() {
        let v: serde_json::Value = serde_json::from_str(r#"[{"Names":["a/b:1"]}]"#).unwrap();
        assert_eq!(image_refs_from_images_json(&v), vec!["a/b:1"]);
        let v: serde_json::Value =
            serde_json::from_str(r#"[{"repository":"e/f","tag":"3"}]"#).unwrap();
        assert_eq!(image_refs_from_images_json(&v), vec!["e/f:3"]);
        let v: serde_json::Value =
            serde_json::from_str(r#"[{"repository":"<none>","tag":"<none>"}]"#).unwrap();
        assert_eq!(image_refs_from_images_json(&v), Vec::<String>::new());
    }
}

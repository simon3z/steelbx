//! Caller-env expansion engine for profile values: `$IDENT`,
//! `${IDENT}`, `${IDENT:-default}`, and `$$`. A pure string engine —
//! the config layer applies it per field with its layered env
//! (caller env, `env_files`, the profile's own `[env]`, and the
//! effective box name as `STEELBX_BOX_NAME`).
use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};

/// Caller-env expansion of a config string value: `$IDENT`,
/// `${IDENT}` expand against the caller's environment; `${IDENT:-default}`
/// falls back to `default` when the variable is unset or empty — the
/// default is the text up to the first `}` (no nesting of `${...}`
/// inside it), and that text itself expands against the same
/// environment; `$$` is a literal `$`; any other `$` is an error.
/// Unset references fail loudly (unless a fallback is given). No shell:
/// an expanded *value* is never re-scanned.
pub(crate) fn expand_env(value: &str, env: &HashMap<String, String>, path: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(d) = rest.find('$') {
        out.push_str(&rest[..d]);
        let (next, val) = expand_token(&rest[d..], env, path)?;
        out.push_str(&val);
        rest = next;
    }
    out.push_str(rest);
    Ok(out)
}

/// One expansion reference: `$IDENT`, `${IDENT}`, `${IDENT:-default}`
/// (the fallback: the variable set *and* non-empty wins, else the
/// default), or `$$`. The default is the text up to the first `}` (no
/// nesting of `${...}` inside it); it is not expanded here —
/// `expand_token` expands it against the caller's environment. The
/// bare form has no fallback: `$IDENT:-x` is the value plus the
/// literal `:-x` (still never an error, still never a shell).
/// One expansion reference, parsed: the name, an optional fallback,
/// and the rest of the string. `${IDENT}` / `${IDENT:-default}` take
/// the text up to the first `}`; the bare form takes the longest
/// `[A-Za-z0-9_]` run.
fn parse_expansion_ref<'a>(
    after: &'a str,
    path: &str,
) -> Result<(String, Option<String>, &'a str)> {
    if let Some(inner) = after.strip_prefix('{') {
        let (inner, close) = inner
            .split_once('}')
            .ok_or_else(|| anyhow!("{path}: unterminated ${{...}} reference (no closing }}"))?;
        return match inner.split_once(":-") {
            Some((id, fallback)) => Ok((id.to_string(), Some(fallback.to_string()), close)),
            None => Ok((inner.to_string(), None, close)),
        };
    }
    let id_len = after
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    if id_len == 0 {
        bail!("{path}: '$' must introduce $IDENT, ${{IDENT}} (optionally ${{IDENT:-default}}), or $$ (a literal dollar)");
    }
    Ok((after[..id_len].to_string(), None, &after[id_len..]))
}

fn expand_token<'a>(
    token: &'a str,
    env: &HashMap<String, String>,
    path: &str,
) -> Result<(&'a str, String)> {
    let after = &token[1..];
    if let Some(rest) = after.strip_prefix('$') {
        return Ok((rest, "$".to_string()));
    }
    let (ident, default, rest) = parse_expansion_ref(after, path)?;
    if !crate::validate::is_valid_env_name(&ident) {
        bail!("{path}: {ident:?} is not a valid variable name");
    }
    // Set *and* non-empty wins. Empty with no fallback is a value
    // ("", unchanged); a fallback treats empty the same as unset
    // (the `:-` semantics, not POSIX's unset-only `-`).
    let val = match env.get(&ident) {
        Some(v) if !v.is_empty() => v.clone(),
        Some(_) if default.is_none() => String::new(),
        _ => match default {
            // The default itself expands against the same caller env
            // (so `${HOMEDIR:-$HOME/box}` resolves `$HOME`); an unset
            // reference inside it fails loudly, as anywhere else.
            Some(d) => expand_env(&d, env, &format!("{path} (default)"))?,
            None => return Err(anyhow!("{path}: references unset variable {ident:?}")),
        },
    };
    Ok((rest, val))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion_bare_and_braced_refs() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());

        assert_eq!(
            expand_env("no dollar signs", &env, "t").unwrap(),
            "no dollar signs"
        );
        assert_eq!(expand_env("$FOO", &env, "t").unwrap(), "bar");
        assert_eq!(expand_env("${FOO}", &env, "t").unwrap(), "bar");
        assert_eq!(
            expand_env("pre-$FOO-post", &env, "t").unwrap(),
            "pre-bar-post"
        );
        assert_eq!(
            expand_env("pre-${FOO}post", &env, "t").unwrap(),
            "pre-barpost"
        );
        assert_eq!(expand_env("$$", &env, "t").unwrap(), "$");
        assert_eq!(expand_env("$$FOO", &env, "t").unwrap(), "$FOO");
        // Expansion is single-pass: a value containing $ is not re-scanned.
        env.insert("RAW".to_string(), "$FOO".to_string());
        assert_eq!(expand_env("$RAW", &env, "t").unwrap(), "$FOO");
    }

    #[test]
    fn expansion_empty_and_fallbacks() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        env.insert("EMPTY".to_string(), String::new());

        // Empty values expand to empty.
        assert_eq!(expand_env("${EMPTY}", &env, "t").unwrap(), "");
        // Fallback `${IDENT:-default}`: unset or empty → the default;
        // set and non-empty → the value.
        assert_eq!(expand_env("${NOFALL:-x}", &env, "t").unwrap(), "x");
        assert_eq!(expand_env("${EMPTY:-x}", &env, "t").unwrap(), "x");
        assert_eq!(expand_env("${FOO:-x}", &env, "t").unwrap(), "bar");
        // The default is the text up to the first `}` (no nesting
        // of `${...}` inside it), and that text itself expands:
        // `$$` inside it is a literal `$`.
        assert_eq!(expand_env("${NOFALL:-a$$b}c}", &env, "t").unwrap(), "a$bc}");
    }

    #[test]
    fn expansion_fallbacks_expand_against_the_caller_env() {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/home/user".to_string());

        // Unset variable → the fallback, which itself expands.
        assert_eq!(
            expand_env("${STEELBX_HOME:-$HOME/steelbx-pi}", &env, "t").unwrap(),
            "/home/user/steelbx-pi"
        );
        // Set and non-empty → the value; the fallback is never touched.
        env.insert("STEELBX_HOME".to_string(), "/box".to_string());
        assert_eq!(
            expand_env("${STEELBX_HOME:-$HOME/steelbx-pi}", &env, "t").unwrap(),
            "/box"
        );
        // An unset reference inside the fallback fails loudly.
        env.remove("STEELBX_HOME");
        assert!(expand_env("${STEELBX_HOME:-$NOPE}", &env, "t").is_err());
        // Pure-literal defaults still work (the toolbox idiom).
        assert_eq!(
            expand_env("${SHELL:-/bin/bash}", &env, "t").unwrap(),
            "/bin/bash"
        );
    }

    #[test]
    fn expansion_bad_dollars_are_errors() {
        let env = HashMap::new();
        // Unset fails loudly.
        assert!(expand_env("$UNSET", &env, "t").is_err());
        // An empty ident is still an error.
        assert!(expand_env("${:-x}", &env, "t").is_err());
        // Anything else with a dollar is an error, not a shell.
        assert!(expand_env("trailing $", &env, "t").is_err());
        assert!(expand_env("$(shell)", &env, "t").is_err());
        assert!(expand_env("${UNCLOSED", &env, "t").is_err());
    }
}

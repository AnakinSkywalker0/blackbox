//! `bb ci pick-comment`: the one decision the GitHub Action needs a real parser for.
//! Given the comments already on a pull request, find the one blackbox wrote earlier for
//! this workflow, so a re-run updates it instead of adding another. Internal: the action's
//! script calls it, and it is hidden from `bb --help`.

use serde_json::Value;
use std::io::Read;
use std::path::Path;

type Res = Result<(), String>;

/// A marker is `<!-- blackbox-diagnosis:` + letters, digits and dashes + ` -->`. Anything
/// else is refused, so a marker can't be used to smuggle other text into a search.
fn valid_marker(m: &str) -> bool {
    m.strip_prefix("<!-- blackbox-diagnosis:")
        .and_then(|r| r.strip_suffix(" -->"))
        .is_some_and(|k| !k.is_empty() && k.len() <= 60 && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// `none`, `update <id>`, or `same <id>` (the comment already says exactly this).
///
/// `listing` is what `gh api --paginate` prints: one JSON array per page, back to back. Only
/// comments written by a bot count, and only if they start with the marker, so a person
/// quoting the marker in their own comment can't be edited, and log text can't redirect it.
pub fn pick(marker: &str, new_body: &str, listing: &str) -> Result<String, String> {
    if !valid_marker(marker) {
        return Err("the marker isn't a blackbox marker".into());
    }
    for page in serde_json::Deserializer::from_str(listing).into_iter::<Value>() {
        let page = page.map_err(|e| format!("not valid JSON: {e}"))?;
        let items = match &page {
            Value::Array(a) => a.as_slice(),
            Value::Object(_) => std::slice::from_ref(&page),
            _ => return Err("expected a list of comments".into()),
        };
        for c in items {
            let by_bot = c["user"]["type"].as_str() == Some("Bot");
            let body = c["body"].as_str().unwrap_or("");
            if by_bot && body.starts_with(marker) {
                let Some(id) = c["id"].as_u64() else { continue };
                let same = body.trim() == new_body.trim();
                return Ok(format!("{} {id}", if same { "same" } else { "update" }));
            }
        }
    }
    Ok("none".into())
}

pub fn pick_comment(marker: &str, body_file: &Path) -> Res {
    let body = std::fs::read_to_string(body_file).map_err(|e| format!("can't read {}: {e}", body_file.display()))?;
    let mut listing = String::new();
    std::io::stdin().take(16 * 1024 * 1024).read_to_string(&mut listing).map_err(|e| format!("can't read the comments: {e}"))?;
    println!("{}", pick(marker, &body, &listing)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = "<!-- blackbox-diagnosis:ci -->";

    fn bot(id: u64, body: &str) -> String {
        format!(r#"{{"id":{id},"user":{{"type":"Bot","login":"github-actions[bot]"}},"body":{}}}"#, serde_json::to_string(body).unwrap())
    }
    fn person(id: u64, body: &str) -> String {
        format!(r#"{{"id":{id},"user":{{"type":"User","login":"someone"}},"body":{}}}"#, serde_json::to_string(body).unwrap())
    }

    #[test]
    fn no_earlier_comment_means_create() {
        assert_eq!(pick(M, "new", "[]").unwrap(), "none");
        assert_eq!(pick(M, "new", "").unwrap(), "none");
        assert_eq!(pick(M, "new", &format!("[{}]", bot(1, "<!-- blackbox-diagnosis:other --> x"))).unwrap(), "none");
    }

    #[test]
    fn a_re_run_updates_the_one_comment_and_notices_when_nothing_changed() {
        let list = format!("[{},{}]", person(1, "LGTM"), bot(7, &format!("{M}\nold diagnosis")));
        assert_eq!(pick(M, &format!("{M}\nnew diagnosis"), &list).unwrap(), "update 7");
        assert_eq!(pick(M, &format!("{M}\nold diagnosis\n"), &list).unwrap(), "same 7");
    }

    #[test]
    fn people_cannot_be_edited_and_quoted_markers_do_not_count() {
        let list = format!("[{},{}]", person(1, &format!("{M} hijack")), bot(2, &format!("quoting {M} in the middle")));
        assert_eq!(pick(M, "new", &list).unwrap(), "none");
    }

    #[test]
    fn paginated_output_and_the_oldest_match_win() {
        // `gh api --paginate` prints one array per page with nothing between them.
        let page1 = format!("[{}]", person(1, "hi"));
        let page2 = format!("[{},{}]", bot(20, &format!("{M} first")), bot(30, &format!("{M} duplicate")));
        assert_eq!(pick(M, "new", &format!("{page1}{page2}")).unwrap(), "update 20");
        assert_eq!(pick(M, "new", &format!("{page1}\n{page2}\n")).unwrap(), "update 20");
    }

    #[test]
    fn bad_input_is_an_error_so_the_script_posts_nothing() {
        assert!(pick(M, "x", "{not json").is_err());
        assert!(pick(M, "x", "42").is_err());
        assert!(pick("<!-- something else -->", "x", "[]").is_err());
        assert!(pick("<!-- blackbox-diagnosis:a b -->", "x", "[]").is_err());
        assert!(pick("<!-- blackbox-diagnosis: -->", "x", "[]").is_err());
        // Comments missing fields are skipped, not fatal.
        assert_eq!(pick(M, "x", r#"[{"id":1},{"body":"hi"},{"id":"nan","user":{"type":"Bot"},"body":"<!-- blackbox-diagnosis:ci -->"}]"#).unwrap(), "none");
    }
}

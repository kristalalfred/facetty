//! What clients make of chat messages.

use std::ops::Range;

/// Byte ranges of the `http://` and `https://` links in a chat message,
/// without punctuation that ends a sentence around them.
pub fn links(text: &str) -> Vec<Range<usize>> {
    let mut links = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find("http") {
        let start = from + found;
        let word = text[start..]
            .split(char::is_whitespace)
            .next()
            .unwrap_or_default();
        from = start + word.len();
        let url = trim_url(word);
        let joined = text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        let has_host = ["https://", "http://"].iter().any(|scheme| {
            url.strip_prefix(scheme)
                .is_some_and(|rest| !rest.is_empty())
        });
        if has_host && !joined {
            links.push(start..start + url.len());
        }
    }
    links
}

fn trim_url(mut url: &str) -> &str {
    while let Some(last) = url.chars().next_back() {
        let unbalanced = |open: char| url.matches(last).count() > url.matches(open).count();
        let trailing = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '>' => true,
            ')' => unbalanced('('),
            ']' => unbalanced('['),
            _ => false,
        };
        if !trailing {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_links_and_leaves_out_trailing_punctuation() {
        let text = "see https://a.io/x_(y), (http://b.org/z). xhttp://c.io https:// end";
        let links: Vec<&str> = links(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(links, ["https://a.io/x_(y)", "http://b.org/z"]);
    }
}

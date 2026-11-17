//! Tool-catalog pagination.
//!
//! The catalog is a snapshot: a cursor is only valid for the catalog version
//! it was minted from. Pages are bounded by both a tool count and a byte
//! budget, so a page can never grow into the MCP bridge's frame limit.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;

use crate::ipc::CATALOG_PAGE_MAX;

pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 200;

#[derive(Debug, PartialEq)]
pub struct Page {
    pub tools: Vec<Value>,
    pub next_cursor: Option<String>,
}

/// Why a cursor was rejected. The management layer projects this to a
/// [`crate::kernel::failure::Problem`] without re-parsing the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageError {
    /// Not base64, not `version:offset`, or a non-numeric field.
    InvalidCursor,
    /// Minted from a different catalog snapshot.
    VersionMismatch,
    /// The offset is beyond the current tool list.
    PastEnd,
}

impl std::fmt::Display for PageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            PageError::InvalidCursor => "invalid cursor",
            PageError::VersionMismatch => "cursor does not match the current catalog version",
            PageError::PastEnd => "cursor is past the end of the catalog",
        })
    }
}

impl std::error::Error for PageError {}

/// Slice one page out of a stable, ordered tool list.
///
/// `cursor` is opaque to clients. It encodes the catalog version and the
/// offset; a cursor from an older snapshot is rejected rather than silently
/// returning the wrong page.
pub fn page(
    tools: &[Value],
    version: u64,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<Page, PageError> {
    let offset = match cursor {
        Some(cursor) => decode_cursor(cursor, version, tools.len())?,
        None => 0,
    };
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let mut page = Vec::new();
    let mut bytes = 0usize;
    for tool in tools.iter().skip(offset) {
        if page.len() >= limit {
            break;
        }
        let size = serde_json::to_vec(tool)
            .map(|value| value.len())
            .unwrap_or(0);
        if !page.is_empty() && bytes.saturating_add(size) > CATALOG_PAGE_MAX {
            break;
        }
        page.push(tool.clone());
        bytes = bytes.saturating_add(size);
    }

    let next = offset.saturating_add(page.len());
    Ok(Page {
        tools: page,
        next_cursor: (next < tools.len()).then(|| encode_cursor(version, next)),
    })
}

fn encode_cursor(version: u64, offset: usize) -> String {
    URL_SAFE_NO_PAD.encode(format!("{}:{}", version, offset))
}

fn decode_cursor(cursor: &str, version: u64, len: usize) -> Result<usize, PageError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| PageError::InvalidCursor)?;
    let text = String::from_utf8(bytes).map_err(|_| PageError::InvalidCursor)?;
    let (cursor_version, offset) = text
        .split_once(':')
        .ok_or(PageError::InvalidCursor)?;
    let cursor_version: u64 = cursor_version
        .parse()
        .map_err(|_| PageError::InvalidCursor)?;
    if cursor_version != version {
        return Err(PageError::VersionMismatch);
    }
    let offset: usize = offset.parse().map_err(|_| PageError::InvalidCursor)?;
    if offset > len {
        return Err(PageError::PastEnd);
    }
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools(count: usize) -> Vec<Value> {
        (0..count)
            .map(|index| {
                json!({
                    "name": format!("tool.{index}"),
                    "description": format!("tool {index}"),
                    "inputSchema": {"type": "object"}
                })
            })
            .collect()
    }

    fn names(page: &Page) -> Vec<String> {
        page.tools
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn pages_are_count_and_version_bounded() {
        let all = tools(5);
        let first = page(&all, 7, None, Some(2)).expect("first page");
        assert_eq!(names(&first), vec!["tool.0", "tool.1"]);
        assert!(first.next_cursor.is_some());

        let second = page(&all, 7, first.next_cursor.as_deref(), Some(2)).expect("second page");
        assert_eq!(names(&second), vec!["tool.2", "tool.3"]);
        assert!(second.next_cursor.is_some());

        let last = page(&all, 7, second.next_cursor.as_deref(), Some(2)).expect("last page");
        assert_eq!(names(&last), vec!["tool.4"]);
        assert!(last.next_cursor.is_none());
    }

    #[test]
    fn a_cursor_from_another_catalog_version_is_rejected() {
        let all = tools(2);
        let first = page(&all, 1, None, Some(1)).expect("page");
        let cursor = first.next_cursor.expect("cursor");
        assert_eq!(
            page(&all, 2, Some(&cursor), Some(1)),
            Err(PageError::VersionMismatch)
        );
    }

    #[test]
    fn invalid_and_past_end_cursors_are_rejected() {
        let all = tools(2);
        assert_eq!(
            page(&all, 1, Some("not-base64"), Some(1)),
            Err(PageError::InvalidCursor)
        );
        let past_end = URL_SAFE_NO_PAD.encode("1:99");
        assert_eq!(
            page(&all, 1, Some(&past_end), Some(1)),
            Err(PageError::PastEnd)
        );
    }

    #[test]
    fn a_single_oversized_tool_is_still_returned() {
        let mut all = tools(1);
        all[0]["description"] = json!("x".repeat(CATALOG_PAGE_MAX + 1));
        let page = page(&all, 1, None, Some(10)).expect("page");
        assert_eq!(page.tools.len(), 1);
    }

    #[test]
    fn an_empty_catalog_has_no_cursor() {
        let page = page(&[], 1, None, None).expect("page");
        assert!(page.tools.is_empty());
        assert!(page.next_cursor.is_none());
    }
}

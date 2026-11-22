//! matcher:正则对事件测试值(工具名/通知类型)筛选;空 = 匹配全部。
//! 编译失败在 discovery 时拒绝该组并警告,不致命。

use regex::Regex;

#[derive(Debug, Clone)]
pub struct HookMatcher {
    pattern: String,
    regex: Regex,
}

impl HookMatcher {
    /// `None`/空串 = 万能匹配。
    pub fn parse(pattern: Option<&str>) -> Result<Option<Self>, String> {
        let Some(pattern) = pattern.map(str::trim).filter(|p| !p.is_empty()) else {
            return Ok(None);
        };
        let regex = Regex::new(pattern).map_err(|error| format!("invalid matcher `{pattern}`: {error}"))?;
        Ok(Some(Self {
            pattern: pattern.to_string(),
            regex,
        }))
    }

    pub fn matches(&self, value: Option<&str>) -> bool {
        match value {
            Some(value) => self.regex.is_match(value),
            None => false,
        }
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

/// 万能匹配判定,discovery 与 dispatcher 共用。
pub fn matches_any(matcher: &Option<HookMatcher>, value: Option<&str>) -> bool {
    match matcher {
        None => true,
        Some(matcher) => matcher.matches(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matcher_matches_tool_names() {
        let matcher = HookMatcher::parse(Some("Bash|Write")).unwrap().unwrap();
        assert!(matcher.matches(Some("Bash")));
        assert!(matcher.matches(Some("WriteFile")));
        assert!(!matcher.matches(Some("read")));
        assert!(!matcher.matches(None));
    }

    #[test]
    fn empty_and_none_match_everything() {
        assert!(matches_any(&HookMatcher::parse(None).unwrap(), None));
        assert!(matches_any(&HookMatcher::parse(Some("  ")).unwrap(), None));
        let matcher = HookMatcher::parse(Some(".*")).unwrap();
        assert!(matches_any(&matcher, Some("anything")));
    }

    #[test]
    fn invalid_pattern_rejected() {
        assert!(HookMatcher::parse(Some("([")).is_err());
    }
}

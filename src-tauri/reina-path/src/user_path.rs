use std::env::VarError;
use std::fmt;
use std::path::PathBuf;

const MAX_EXPANSION_DEPTH: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathResolveError {
    EmptyPath,
    InvalidVariableSyntax(String),
    UndefinedVariable(String),
    NonUnicodeVariable(String),
    VariableCycle(Vec<String>),
    ExpansionDepthExceeded,
    NotAbsolute(String),
}

impl PathResolveError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyPath => "path_empty",
            Self::InvalidVariableSyntax(_) => "path_variable_syntax",
            Self::UndefinedVariable(_) => "path_variable_undefined",
            Self::NonUnicodeVariable(_) => "path_variable_non_unicode",
            Self::VariableCycle(_) => "path_variable_cycle",
            Self::ExpansionDepthExceeded => "path_variable_depth_exceeded",
            Self::NotAbsolute(_) => "path_not_absolute",
        }
    }
}

impl fmt::Display for PathResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath => write!(formatter, "路径不能为空"),
            Self::InvalidVariableSyntax(value) => {
                write!(formatter, "路径变量语法无效: {value}")
            }
            Self::UndefinedVariable(name) => write!(formatter, "环境变量未定义: {name}"),
            Self::NonUnicodeVariable(name) => {
                write!(formatter, "环境变量不是有效 Unicode 文本: {name}")
            }
            Self::VariableCycle(names) => {
                write!(formatter, "环境变量存在循环引用: {}", names.join(" -> "))
            }
            Self::ExpansionDepthExceeded => {
                write!(formatter, "环境变量递归展开超过 {MAX_EXPANSION_DEPTH} 层")
            }
            Self::NotAbsolute(path) => write!(formatter, "解析结果不是绝对路径: {path}"),
        }
    }
}

impl std::error::Error for PathResolveError {}

enum LookupError {
    Undefined,
    NonUnicode,
}

/// 解析用户配置的文件系统路径。
///
/// Windows 仅识别路径开头的 `%VAR%`；Linux 识别开头的 `$VAR`、`${VAR}`、
/// `~` 与 `~/`。解析只负责环境变量展开和绝对路径保证，不访问文件系统。
pub fn resolve_user_path(path: &str) -> Result<PathBuf, PathResolveError> {
    resolve_user_path_with(path, |name| match std::env::var(name) {
        Ok(value) => Ok(value),
        Err(VarError::NotPresent) => Err(LookupError::Undefined),
        Err(VarError::NotUnicode(_)) => Err(LookupError::NonUnicode),
    })
}

fn resolve_user_path_with<F>(path: &str, lookup: F) -> Result<PathBuf, PathResolveError>
where
    F: Fn(&str) -> Result<String, LookupError>,
{
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(PathResolveError::EmptyPath);
    }

    let expanded = expand_prefix(trimmed, &lookup, &mut Vec::new(), 0)?;
    let resolved = PathBuf::from(&expanded);
    if !resolved.is_absolute() {
        return Err(PathResolveError::NotAbsolute(expanded));
    }
    Ok(resolved)
}

fn lookup_variable<F>(name: &str, lookup: &F, stack: &[String]) -> Result<String, PathResolveError>
where
    F: Fn(&str) -> Result<String, LookupError>,
{
    if let Some(index) = stack
        .iter()
        .position(|current| variable_names_equal(current, name))
    {
        let mut cycle = stack[index..].to_vec();
        cycle.push(name.to_string());
        return Err(PathResolveError::VariableCycle(cycle));
    }
    lookup(name).map_err(|error| match error {
        LookupError::Undefined => PathResolveError::UndefinedVariable(name.to_string()),
        LookupError::NonUnicode => PathResolveError::NonUnicodeVariable(name.to_string()),
    })
}

fn expand_prefix<F>(
    value: &str,
    lookup: &F,
    stack: &mut Vec<String>,
    depth: usize,
) -> Result<String, PathResolveError>
where
    F: Fn(&str) -> Result<String, LookupError>,
{
    let Some((name, suffix)) = parse_variable_prefix(value)? else {
        return Ok(value.to_string());
    };
    if depth >= MAX_EXPANSION_DEPTH {
        return Err(PathResolveError::ExpansionDepthExceeded);
    }
    let replacement = lookup_variable(&name, lookup, stack)?;
    stack.push(name);
    let result = expand_prefix(&format!("{replacement}{suffix}"), lookup, stack, depth + 1);
    stack.pop();
    result
}

#[cfg(target_os = "windows")]
fn variable_names_equal(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[cfg(not(target_os = "windows"))]
fn variable_names_equal(left: &str, right: &str) -> bool {
    left == right
}

#[cfg(target_os = "windows")]
fn parse_variable_prefix(value: &str) -> Result<Option<(String, &str)>, PathResolveError> {
    if !value.starts_with('%') {
        return Ok(None);
    }
    let Some(end) = value[1..].find('%').map(|index| index + 1) else {
        return Err(PathResolveError::InvalidVariableSyntax(value.to_string()));
    };
    let name = &value[1..end];
    if name.is_empty() {
        return Err(PathResolveError::InvalidVariableSyntax(value.to_string()));
    }
    Ok(Some((name.to_string(), &value[end + 1..])))
}

#[cfg(not(target_os = "windows"))]
fn parse_variable_prefix(value: &str) -> Result<Option<(String, &str)>, PathResolveError> {
    if value == "~" {
        return Ok(Some(("HOME".to_string(), "")));
    }
    if let Some(suffix) = value.strip_prefix("~/") {
        return Ok(Some((
            "HOME".to_string(),
            &value[value.len() - suffix.len() - 1..],
        )));
    }
    if value.starts_with('~') {
        return Err(PathResolveError::InvalidVariableSyntax(value.to_string()));
    }
    if let Some(rest) = value.strip_prefix("${") {
        let Some(end) = rest.find('}') else {
            return Err(PathResolveError::InvalidVariableSyntax(value.to_string()));
        };
        let name = &rest[..end];
        if !is_valid_unix_variable_name(name) {
            return Err(PathResolveError::InvalidVariableSyntax(value.to_string()));
        }
        return Ok(Some((name.to_string(), &rest[end + 1..])));
    }
    let Some(rest) = value.strip_prefix('$') else {
        return Ok(None);
    };
    let name_len = rest
        .char_indices()
        .take_while(|(index, character)| {
            if *index == 0 {
                *character == '_' || character.is_ascii_alphabetic()
            } else {
                *character == '_' || character.is_ascii_alphanumeric()
            }
        })
        .map(|(index, character)| index + character.len_utf8())
        .last()
        .unwrap_or(0);
    if name_len == 0 {
        return Err(PathResolveError::InvalidVariableSyntax(value.to_string()));
    }
    Ok(Some((rest[..name_len].to_string(), &rest[name_len..])))
}

#[cfg(not(target_os = "windows"))]
fn is_valid_unix_variable_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn resolve_with_values(
        input: &str,
        values: &[(&str, &str)],
    ) -> Result<PathBuf, PathResolveError> {
        let values = values.iter().copied().collect::<HashMap<_, _>>();
        resolve_user_path_with(input, |name| {
            values
                .get(name)
                .map(|value| (*value).to_string())
                .ok_or(LookupError::Undefined)
        })
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn expands_windows_variable_prefix_recursively() {
        let resolved = resolve_with_values(
            r" %GAME_ROOT%\Title ",
            &[
                ("GAME_ROOT", r"%USER_HOME%\Games"),
                ("USER_HOME", r"C:\Users\Reina"),
            ],
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from(r"C:\Users\Reina\Games\Title"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn keeps_variable_like_text_in_the_middle_literal() {
        let resolved = resolve_with_values(r"C:\Games\%TITLE%", &[("TITLE", "Other")]).unwrap();
        assert_eq!(resolved, PathBuf::from(r"C:\Games\%TITLE%"));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn expands_linux_variables_and_home() {
        assert_eq!(
            resolve_with_values("$ROOT/Game", &[("ROOT", "/games")]).unwrap(),
            PathBuf::from("/games/Game")
        );
        assert_eq!(
            resolve_with_values("${ROOT}/Game", &[("ROOT", "/games")]).unwrap(),
            PathBuf::from("/games/Game")
        );
        assert_eq!(
            resolve_with_values("~/Game", &[("HOME", "/home/reina")]).unwrap(),
            PathBuf::from("/home/reina/Game")
        );
    }

    #[test]
    fn rejects_undefined_and_cyclic_variables() {
        #[cfg(target_os = "windows")]
        let missing = "%MISSING%\\Game";
        #[cfg(not(target_os = "windows"))]
        let missing = "$MISSING/Game";
        assert!(matches!(
            resolve_with_values(missing, &[]),
            Err(PathResolveError::UndefinedVariable(name)) if name == "MISSING"
        ));

        #[cfg(target_os = "windows")]
        let (input, values) = ("%A%", [("A", "%B%"), ("B", "%A%")]);
        #[cfg(not(target_os = "windows"))]
        let (input, values) = ("$A", [("A", "$B"), ("B", "$A")]);
        assert!(matches!(
            resolve_with_values(input, &values),
            Err(PathResolveError::VariableCycle(_))
        ));
    }

    #[test]
    fn rejects_relative_paths_and_accepts_missing_absolute_paths() {
        assert!(matches!(
            resolve_with_values("games/title", &[]),
            Err(PathResolveError::NotAbsolute(_))
        ));

        #[cfg(target_os = "windows")]
        let absolute = r"Z:\this-path-does-not-need-to-exist";
        #[cfg(not(target_os = "windows"))]
        let absolute = "/this-path-does-not-need-to-exist";
        assert_eq!(
            resolve_with_values(absolute, &[]).unwrap(),
            PathBuf::from(absolute)
        );
    }

    #[test]
    fn allows_sixteen_expansions_and_rejects_more() {
        #[cfg(target_os = "windows")]
        let input = "%V0%";
        #[cfg(not(target_os = "windows"))]
        let input = "$V0";
        #[cfg(target_os = "windows")]
        let absolute = r"C:\Resolved";
        #[cfg(not(target_os = "windows"))]
        let absolute = "/resolved";

        let resolve_chain = |terminal: usize| {
            resolve_user_path_with(input, |name| {
                let index = name
                    .strip_prefix('V')
                    .and_then(|value| value.parse::<usize>().ok())
                    .ok_or(LookupError::Undefined)?;
                if index == terminal {
                    Ok(absolute.to_string())
                } else {
                    #[cfg(target_os = "windows")]
                    return Ok(format!("%V{}%", index + 1));
                    #[cfg(not(target_os = "windows"))]
                    return Ok(format!("$V{}", index + 1));
                }
            })
        };

        assert_eq!(resolve_chain(15).unwrap(), PathBuf::from(absolute));
        assert!(matches!(
            resolve_chain(16),
            Err(PathResolveError::ExpansionDepthExceeded)
        ));
    }
}

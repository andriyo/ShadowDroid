//! Source-to-class resolution for line breakpoints without an IDE
//! (design §5.2): find the file in the project source index, read its
//! `package`, and decide which loaded classes can hold its lines.
//!
//! Everything here is pure; the session performs the JDWP lookups
//! (`SourceFile`, `LineTable`) on the candidates this module selects.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A source file the breakpoint names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceTarget {
    /// The file name the VM's `SourceFile` attribute carries (`Foo.kt`).
    pub basename: String,
    /// Declared package; `None` for the default package or when the file is
    /// not available locally.
    pub package: Option<String>,
    /// Local path, when found.
    pub path: Option<PathBuf>,
    /// Whether the `package` came from the file (true) or is unknown (false).
    pub package_known: bool,
    /// The requested line lies inside a Kotlin `inline fun` body.
    pub inline_body: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocateError {
    /// Several project files match a name or suffix.
    Ambiguous(Vec<String>),
    /// Not a file name the VM can match (`--file` must name a .kt/.java file).
    NotASourceFile(String),
    /// The located file has no code at the line: past its end, blank, or a
    /// comment. No class can ever bind it.
    NoCodeAtLine {
        path: String,
        line: u32,
        reason: &'static str,
    },
}

/// Resolve `--file` to a [`SourceTarget`].
///
/// * an existing path (absolute or relative to the cwd) is read directly;
/// * otherwise the project index (`project_root`, else the cwd) is searched
///   for a path ending in `file` component-wise;
/// * when nothing local matches, the bare basename is still usable: the
///   session falls back to loaded classes named after the file.
pub fn locate(
    file: &Path,
    line: u32,
    project_root: Option<&Path>,
) -> Result<SourceTarget, LocateError> {
    let basename = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !(basename.ends_with(".kt") || basename.ends_with(".java")) {
        return Err(LocateError::NotASourceFile(file.display().to_string()));
    }
    if file.is_file() {
        return checked(target_from_path(file.to_path_buf(), basename, line), line);
    }
    let root = project_root
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok());
    if let Some(root) = root {
        let index = crate::crashscan::source_index(&root);
        let wanted: Vec<String> = file
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .filter(|c| c != "." && !c.is_empty())
            .collect();
        let mut matches: Vec<String> = index
            .get(&basename)
            .into_iter()
            .flatten()
            .filter(|relative| ends_with_components(relative, &wanted))
            .cloned()
            .collect();
        matches.sort();
        match matches.len() {
            0 => {}
            1 => {
                return checked(
                    target_from_path(root.join(&matches[0]), basename, line),
                    line,
                );
            }
            _ => return Err(LocateError::Ambiguous(matches)),
        }
    }
    Ok(SourceTarget {
        basename,
        package: None,
        path: None,
        package_known: false,
        inline_body: false,
    })
}

/// Reject a line no class can hold: past the end of the file, blank, or a
/// comment. Anything else may carry code (Kotlin maps a closing brace to the
/// function's return), so the VM's line tables stay the authority.
fn checked(target: SourceTarget, line: u32) -> Result<SourceTarget, LocateError> {
    let Some(path) = &target.path else {
        return Ok(target);
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(target);
    };
    if let Some(reason) = no_code_reason(&text, line) {
        return Err(LocateError::NoCodeAtLine {
            path: path.display().to_string(),
            line,
            reason,
        });
    }
    Ok(target)
}

/// Why `line` (1-based) of `text` cannot hold code, if it cannot.
pub fn no_code_reason(text: &str, line: u32) -> Option<&'static str> {
    let Some(raw) = line
        .checked_sub(1)
        .and_then(|index| text.lines().nth(index as usize))
    else {
        return Some("past_end_of_file");
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some("blank_line");
    }
    if trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with("*")
        || trimmed.starts_with("import ")
        || trimmed.starts_with("package ")
    {
        return Some("comment_or_declaration");
    }
    None
}

fn ends_with_components(relative: &str, wanted: &[String]) -> bool {
    let have: Vec<&str> = Path::new(relative)
        .components()
        .map(|c| c.as_os_str().to_str().unwrap_or(""))
        .collect();
    have.len() >= wanted.len()
        && have[have.len() - wanted.len()..]
            .iter()
            .zip(wanted)
            .all(|(a, b)| *a == b)
}

fn target_from_path(path: PathBuf, basename: String, line: u32) -> SourceTarget {
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    SourceTarget {
        basename,
        package: parse_package(&text),
        package_known: true,
        inline_body: line_in_inline_function(&text, line),
        path: Some(path),
    }
}

/// The `package` declaration of a Kotlin or Java file.
pub fn parse_package(text: &str) -> Option<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?m)^\s*package\s+([A-Za-z_][\w`]*(?:\s*\.\s*[A-Za-z_`][\w`]*)*)")
            .expect("package regex")
    });
    let raw = re.captures(text)?.get(1)?.as_str();
    Some(raw.replace(['`', ' ', '\t'], ""))
}

/// `Lcom/example/Foo$Bar;` → `com.example.Foo$Bar`; `[I` → `int[]`.
pub fn type_name(signature: &str) -> String {
    let dims = signature.bytes().take_while(|b| *b == b'[').count();
    let element = &signature[dims..];
    let base = match element {
        "Z" => "boolean".to_string(),
        "B" => "byte".to_string(),
        "C" => "char".to_string(),
        "S" => "short".to_string(),
        "I" => "int".to_string(),
        "J" => "long".to_string(),
        "F" => "float".to_string(),
        "D" => "double".to_string(),
        "V" => "void".to_string(),
        other => other
            .strip_prefix('L')
            .and_then(|s| s.strip_suffix(';'))
            .unwrap_or(other)
            .replace('/', "."),
    };
    format!("{base}{}", "[]".repeat(dims))
}

/// Package of a class signature (`Lcom/a/B;` → `com.a`); `""` for the
/// default package.
pub fn signature_package(signature: &str) -> Option<String> {
    let inner = signature.strip_prefix('L')?.strip_suffix(';')?;
    Some(
        inner
            .rsplit_once('/')
            .map(|(pkg, _)| pkg.replace('/', "."))
            .unwrap_or_default(),
    )
}

/// Simple name without nesting (`Lcom/a/Foo$bar$1;` → `Foo`).
pub fn outer_simple_name(signature: &str) -> Option<String> {
    let inner = signature.strip_prefix('L')?.strip_suffix(';')?;
    let simple = inner.rsplit('/').next()?;
    Some(simple.split('$').next().unwrap_or(simple).to_string())
}

/// Whether a loaded class can hold lines of `target` before asking the VM
/// for its `SourceFile`: same package when known; else named after the file
/// (`Foo`, `FooKt`, `Foo$…`), which covers the usual one-class-per-file case.
pub fn is_candidate(signature: &str, target: &SourceTarget) -> bool {
    if !signature.starts_with('L') {
        return false;
    }
    if target.package_known {
        return signature_package(signature).as_deref()
            == Some(target.package.as_deref().unwrap_or(""));
    }
    let stem = target
        .basename
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(&target.basename);
    outer_simple_name(signature).is_some_and(|name| name == stem || name == format!("{stem}Kt"))
}

/// `ClassMatch` pattern for deferred binding (`com.example.*`).
pub fn package_pattern(target: &SourceTarget) -> Option<String> {
    match (&target.package, target.package_known) {
        (Some(package), true) if !package.is_empty() => Some(format!("{package}.*")),
        _ => None,
    }
}

/// Heuristic: is 1-based `line` inside the body of a Kotlin `inline fun`?
/// Brace-depth scan that ignores braces in `//` comments and simple string
/// literals; good enough to explain an unbound line, never used to bind one.
pub fn line_in_inline_function(text: &str, line: u32) -> bool {
    // (depth at which an inline body opened)
    let mut inline_depths: Vec<usize> = Vec::new();
    let mut depth = 0_usize;
    let mut pending_inline = false;
    for (index, raw) in text.lines().enumerate() {
        let number = index as u32 + 1;
        let code = strip_comment_and_strings(raw);
        if code.contains("inline fun ") || code.trim_start().starts_with("inline fun") {
            pending_inline = true;
        }
        if number == line {
            // Inside an inline body already, or the declaration line itself
            // when its body opens on this line.
            if !inline_depths.is_empty() {
                return true;
            }
            if pending_inline && code.contains('{') {
                return true;
            }
        }
        for ch in code.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    if pending_inline {
                        inline_depths.push(depth);
                        pending_inline = false;
                    }
                }
                '}' => {
                    if inline_depths.last() == Some(&depth) {
                        inline_depths.pop();
                    }
                    depth = depth.saturating_sub(1);
                }
                // An expression-bodied inline fun has no braces to track.
                '=' if pending_inline && !code.contains('{') => pending_inline = false,
                _ => {}
            }
        }
        if number >= line {
            break;
        }
    }
    false
}

fn strip_comment_and_strings(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_string = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_string {
            if ch == '\\' {
                chars.next();
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '/' if chars.peek() == Some(&'/') => break,
            _ => out.push(ch),
        }
    }
    out
}

/// A local's name without the `\N…` suffix Kotlin adds to slots of
/// inlined code (`it\1` → `it`, `$composer\6` → `$composer`).
pub fn display_local_name(name: &str) -> &str {
    name.split('\\').next().unwrap_or(name)
}

/// Kotlin compiler bookkeeping locals that are never user state:
/// inline-function markers (`$i$f$…`, `$i$a$…`).
pub fn is_hidden_local(name: &str) -> bool {
    name.starts_with("$i$f$") || name.starts_with("$i$a$")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_without_code_are_rejected_before_binding() {
        let text = "package a\n\nimport b.C\n// note\nclass A {\n    fun f() = 1\n}\n";
        assert_eq!(no_code_reason(text, 1), Some("comment_or_declaration"));
        assert_eq!(no_code_reason(text, 2), Some("blank_line"));
        assert_eq!(no_code_reason(text, 3), Some("comment_or_declaration"));
        assert_eq!(no_code_reason(text, 4), Some("comment_or_declaration"));
        assert_eq!(no_code_reason(text, 6), None);
        // A closing brace can map to the return instruction: left to the VM.
        assert_eq!(no_code_reason(text, 7), None);
        assert_eq!(no_code_reason(text, 8), Some("past_end_of_file"));
        assert_eq!(no_code_reason(text, 0), Some("past_end_of_file"));
    }

    #[test]
    fn packages_parse_in_both_languages() {
        assert_eq!(
            parse_package("// c\npackage io.github.x.sample\n\nimport a.b"),
            Some("io.github.x.sample".into())
        );
        assert_eq!(
            parse_package("package com.example.app;\nclass A {}"),
            Some("com.example.app".into())
        );
        assert_eq!(
            parse_package("package com.`fun`.app"),
            Some("com.fun.app".into())
        );
        assert_eq!(parse_package("class NoPackage"), None);
    }

    #[test]
    fn signatures_render_and_split() {
        assert_eq!(type_name("Lcom/a/Foo$Bar;"), "com.a.Foo$Bar");
        assert_eq!(type_name("[[I"), "int[][]");
        assert_eq!(type_name("[Ljava/lang/String;"), "java.lang.String[]");
        assert_eq!(signature_package("Lcom/a/Foo;"), Some("com.a".into()));
        assert_eq!(signature_package("LFoo;"), Some(String::new()));
        assert_eq!(outer_simple_name("Lcom/a/Foo$bar$1;"), Some("Foo".into()));
    }

    #[test]
    fn candidates_follow_package_or_file_stem() {
        let known = SourceTarget {
            basename: "MainActivity.kt".into(),
            package: Some("io.x.sample".into()),
            path: None,
            package_known: true,
            inline_body: false,
        };
        assert!(is_candidate("Lio/x/sample/MainActivity;", &known));
        assert!(is_candidate("Lio/x/sample/Other$1;", &known));
        assert!(!is_candidate("Lio/x/sample/sub/MainActivity;", &known));
        assert!(!is_candidate("[Lio/x/sample/MainActivity;", &known));
        assert_eq!(package_pattern(&known), Some("io.x.sample.*".into()));

        let unknown = SourceTarget {
            package: None,
            package_known: false,
            ..known
        };
        assert!(is_candidate("Lany/pkg/MainActivity$onCreate$1;", &unknown));
        assert!(is_candidate("Lany/pkg/MainActivityKt;", &unknown));
        assert!(!is_candidate("Lany/pkg/MainActivityHelper;", &unknown));
        assert_eq!(package_pattern(&unknown), None);
    }

    #[test]
    fn inline_bodies_are_detected() {
        let source = "\
package a
inline fun <T> measure(block: () -> T): T {
    val start = 1
    return block()
}
fun plain() {
    println(\"{\")
}
inline fun short() = 42
fun after() {
    val x = 1
}
";
        assert!(line_in_inline_function(source, 2));
        assert!(line_in_inline_function(source, 3));
        assert!(line_in_inline_function(source, 4));
        assert!(!line_in_inline_function(source, 6));
        assert!(!line_in_inline_function(source, 7));
        assert!(!line_in_inline_function(source, 11));
    }

    #[test]
    fn locate_reads_paths_and_searches_the_project_index() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("app/src/main/kotlin/io/x");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("Main.kt"), "package io.x\nfun main() {}\n").unwrap();
        let other = dir.path().join("lib/src/main/kotlin/io/y");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("Main.kt"), "package io.y\nfun other() {}\n").unwrap();

        let direct = locate(&src.join("Main.kt"), 2, None).unwrap();
        assert_eq!(direct.package.as_deref(), Some("io.x"));
        assert!(direct.package_known);

        let suffix = locate(Path::new("io/y/Main.kt"), 2, Some(dir.path())).unwrap();
        assert_eq!(suffix.package.as_deref(), Some("io.y"));
        // A located file is line-checked: the package line holds no code.
        assert!(matches!(
            locate(Path::new("io/y/Main.kt"), 1, Some(dir.path())),
            Err(LocateError::NoCodeAtLine { line: 1, .. })
        ));

        match locate(Path::new("Main.kt"), 1, Some(dir.path())) {
            Err(LocateError::Ambiguous(candidates)) => assert_eq!(candidates.len(), 2),
            other => panic!("{other:?}"),
        }

        let missing = locate(Path::new("Elsewhere.kt"), 1, Some(dir.path())).unwrap();
        assert!(!missing.package_known);
        assert_eq!(missing.basename, "Elsewhere.kt");

        assert!(matches!(
            locate(Path::new("README.md"), 1, Some(dir.path())),
            Err(LocateError::NotASourceFile(_))
        ));
    }

    #[test]
    fn inlined_slot_suffixes_are_stripped_for_display() {
        assert_eq!(display_local_name("it\\1"), "it");
        assert_eq!(display_local_name("$composer\\6"), "$composer");
        assert_eq!(display_local_name("plain"), "plain");
    }

    #[test]
    fn kotlin_markers_are_hidden() {
        assert!(is_hidden_local("$i$f$getValue\\1\\51"));
        assert!(is_hidden_local("$i$a$-let-MainActivity$onCreate$1"));
        assert!(!is_hidden_local("savedInstanceState"));
    }
}

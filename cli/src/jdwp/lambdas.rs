//! Telling a line's outer call site from the Kotlin lambdas on it
//! (`break line --variant all|outer|lambda`).
//!
//! Device-confirmed rule: a location is a lambda when its method name
//! contains `$lambda$` (Kotlin 2.x compiles lambda bodies to static
//! `outer$lambda$N` methods in the enclosing class; nesting adds segments),
//! or it is `invokeSuspend` of a suspend lambda, or `invoke` of a
//! `kotlin.jvm.internal.Lambda` / function-reference class. Everything else
//! is the outer call site, including named methods of `object :` classes.
//! Bridges (a function reference binds a synthetic `invoke()Object` next to
//! the real `invoke()V`) are never bound.

use serde::{Deserialize, Serialize};

use super::session::Session;
use super::vm::MethodInfo;

const ACC_BRIDGE: i32 = 0x0040;
const ACC_SYNTHETIC: i32 = 0x1000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum LineVariant {
    /// Every location on the line (Studio's default).
    #[default]
    All,
    /// Only the enclosing method, not lambdas on the line.
    Outer,
    /// Only the innermost lambda on the line.
    Lambda,
}

/// A compiler bridge: skipped for line binding. Kotlin `$lambda$` bodies are
/// synthetic too, but they hold real code.
pub fn is_bridge(method: &MethodInfo) -> bool {
    method.mod_bits & ACC_BRIDGE != 0
        || (method.mod_bits & ACC_SYNTHETIC != 0 && !method.name.contains("$lambda$"))
}

/// Superclasses that make `invoke`/`invokeSuspend` a lambda body.
const LAMBDA_BASES: &[&str] = &[
    "Lkotlin/jvm/internal/Lambda;",
    "Lkotlin/jvm/internal/FunctionReference;",
    "Lkotlin/jvm/internal/FunctionReferenceImpl;",
    "Lkotlin/coroutines/jvm/internal/SuspendLambda;",
    "Lkotlin/coroutines/jvm/internal/RestrictedSuspendLambda;",
];

/// Keep the candidates `variant` asks for; `lambda` keeps the deepest.
pub fn select_variant<T>(
    candidates: Vec<(T, u64, u32)>,
    variant: LineVariant,
) -> Vec<(T, u64, u32)> {
    match variant {
        LineVariant::All => candidates,
        LineVariant::Outer => candidates.into_iter().filter(|(_, _, d)| *d == 0).collect(),
        LineVariant::Lambda => {
            let deepest = candidates.iter().map(|(_, _, d)| *d).max().unwrap_or(0);
            if deepest == 0 {
                return Vec::new();
            }
            candidates
                .into_iter()
                .filter(|(_, _, d)| *d == deepest)
                .collect()
        }
    }
}

impl Session {
    /// Lambda nesting depth of `method` in class `class_id` (0: outer).
    pub(super) async fn lambda_depth(
        &self,
        class_id: u64,
        signature: &str,
        method: &MethodInfo,
    ) -> u32 {
        let segments = method.name.matches("$lambda$").count() as u32;
        if segments > 0 {
            return segments;
        }
        if method.name != "invoke" && method.name != "invokeSuspend" {
            return 0;
        }
        let Ok(chain) = self.hierarchy(class_id).await else {
            return 0;
        };
        for class in chain {
            if let Ok(parent) = self.signature(class).await
                && LAMBDA_BASES.contains(&parent.as_str())
            {
                // A lambda class nested in another lambda class
                // (`Outer$f$1$1`) is one level deeper.
                let nesting = signature
                    .rsplit('/')
                    .next()
                    .unwrap_or(signature)
                    .split('$')
                    .filter(|part| part.trim_end_matches(';').parse::<u32>().is_ok())
                    .count() as u32;
                return nesting.max(1);
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method(name: &str, mod_bits: i32) -> MethodInfo {
        MethodInfo {
            method_id: 1,
            name: name.into(),
            signature: "()V".into(),
            mod_bits,
        }
    }

    #[test]
    fn bridges_are_skipped_but_lambda_bodies_are_not() {
        assert!(is_bridge(&method("invoke", ACC_BRIDGE | ACC_SYNTHETIC)));
        assert!(is_bridge(&method("access$get", ACC_SYNTHETIC)));
        assert!(!is_bridge(&method(
            "onCreate$lambda$0",
            ACC_SYNTHETIC | 0x8
        )));
        assert!(!is_bridge(&method("onCreate", 1)));
    }

    #[test]
    fn variants_pick_outer_or_the_deepest_lambda() {
        let candidates = vec![("outer", 0, 0), ("l1", 4, 1), ("l2", 8, 2)];
        let names = |v| {
            select_variant(candidates.clone(), v)
                .into_iter()
                .map(|(n, _, _)| n)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(LineVariant::All), ["outer", "l1", "l2"]);
        assert_eq!(names(LineVariant::Outer), ["outer"]);
        assert_eq!(names(LineVariant::Lambda), ["l2"]);
        assert!(select_variant(vec![("outer", 0, 0)], LineVariant::Lambda).is_empty());
    }
}

package io.github.andriyo.shadowdroid.studio

import com.intellij.lang.Language
import com.intellij.openapi.application.ReadAction
import com.intellij.openapi.project.Project
import com.intellij.psi.PsiDocumentManager
import com.intellij.psi.PsiErrorElement
import com.intellij.psi.PsiExpressionCodeFragment
import com.intellij.psi.PsiFileFactory
import com.intellij.psi.PsiPrimitiveType
import com.intellij.psi.SyntaxTraverser
import com.intellij.xdebugger.breakpoints.XBreakpoint
import com.intellij.xdebugger.breakpoints.XBreakpointType
import com.intellij.xdebugger.evaluation.EvaluationMode
import com.intellij.xdebugger.impl.breakpoints.XExpressionImpl

/**
 * Set-time validation for breakpoint condition/log expressions.
 *
 * Builds the expression fragment through the breakpoint type's own
 * [com.intellij.xdebugger.evaluation.XDebuggerEditorsProvider] — the same
 * construction the runtime evaluator parses at hit time — so what this
 * rejects is what would have failed (and, before the behavior policy,
 * hung the IDE behind a modal dialog) when the breakpoint fires.
 */
internal object ExpressionValidation {
    private const val MAX_PROBLEMS = 8

    data class Problem(val kind: String, val message: String, val offset: Int?) {
        fun toMap(): Map<String, Any?> = BridgeProtocol.map(
            "kind", kind,
            "message", message,
            "offset", offset,
        )
    }

    /**
     * Returns the problems that make [expression] unusable on [breakpoint].
     * Empty means "no reason to reject": validation is best-effort and stays
     * permissive when the type has no editors provider or the fragment has no
     * source context to resolve against.
     */
    fun validate(
        project: Project,
        breakpoint: XBreakpoint<*>,
        expression: String,
        expectBoolean: Boolean,
    ): List<Problem> =
        try {
            ReadAction.computeBlocking<List<Problem>, RuntimeException> {
                validateInReadAction(project, breakpoint, expression, expectBoolean)
            }
        } catch (t: Throwable) {
            // Validation must never turn a working flow into a failure.
            emptyList()
        }

    private fun validateInReadAction(
        project: Project,
        breakpoint: XBreakpoint<*>,
        expression: String,
        expectBoolean: Boolean,
    ): List<Problem> {
        @Suppress("UNCHECKED_CAST")
        val type = breakpoint.type as XBreakpointType<XBreakpoint<*>, *>
        val position = runCatching { breakpoint.sourcePosition }.getOrNull()
        if (isKotlin(type, position?.file?.extension)) {
            return kotlinSyntaxProblems(project, expression) ?: emptyList()
        }
        val provider = type.getEditorsProvider(breakpoint, project) ?: return emptyList()
        val document = provider.createDocument(
            project,
            XExpressionImpl.fromText(expression),
            position,
            EvaluationMode.EXPRESSION,
        )
        val psi = PsiDocumentManager.getInstance(project).getPsiFile(document) ?: return emptyList()

        val syntax = SyntaxTraverser.psiTraverser(psi)
            .filter(PsiErrorElement::class.java)
            .take(MAX_PROBLEMS)
            .map { Problem("syntax", it.errorDescription, it.textOffset) }
            .toList()
        if (syntax.isNotEmpty()) return syntax

        // Deliberately NO name-resolution check. In a breakpoint code fragment
        // there is no running frame at set time and analysis is incomplete, so
        // resolve() returns null for perfectly valid references — e.g. a Kotlin
        // method call like `intent.getStringExtra("x")` — which would REJECT a
        // valid condition (a false positive is worse than the hang we fix). The
        // behavior policy makes runtime evaluation failures non-blocking and
        // records them on the breakpoint, so an unknown symbol is caught safely
        // at hit time via last_evaluation_error instead of wrongly at set time.
        // Only the type check below runs, and only where it is unambiguous.
        val problems = mutableListOf<Problem>()
        if (expectBoolean) {
            nonBooleanProblem(psi)?.let { problems += it }
        }
        return problems
    }

    private fun isKotlin(type: XBreakpointType<*, *>, extension: String?): Boolean =
        type.id.startsWith("kotlin") || extension == "kt" || extension == "kts"

    /**
     * Syntax problems in a Kotlin expression, or null without the Kotlin
     * plugin. The expression is parsed as a plain Kotlin file rather than
     * through the Kotlin debugger's code-fragment factory, which may run a
     * modal progress (invokeAndWait) and so must not be called from this
     * background read action: it logged a SEVERE error blaming ShadowDroid.
     * Semantic problems are deliberately left to the runtime evaluator.
     */
    private fun kotlinSyntaxProblems(project: Project, expression: String): List<Problem>? {
        val kotlin = Language.findLanguageByID("kotlin") ?: return null
        val prefix = "fun shadowDroidCondition() = run {\n"
        val file = PsiFileFactory.getInstance(project)
            .createFileFromText("shadowdroid_condition.kt", kotlin, "$prefix$expression\n}")
        return SyntaxTraverser.psiTraverser(file)
            .filter(PsiErrorElement::class.java)
            .take(MAX_PROBLEMS)
            .map { Problem("syntax", it.errorDescription, (it.textOffset - prefix.length).coerceAtLeast(0)) }
            .toList()
    }

    private fun nonBooleanProblem(psi: com.intellij.psi.PsiFile): Problem? {
        // Type checking is Java-only; the Kotlin fragment type requires the
        // analysis API and runtime evaluation reports the mismatch anyway.
        val fragment = psi as? PsiExpressionCodeFragment ?: return null
        val expressionType = runCatching { fragment.expression?.type }.getOrNull() ?: return null
        // Value-based comparison via equalsToText: a boolean primitive carrying
        // type annotations is not the canonical PsiTypes.booleanType() singleton,
        // so `==` would misflag a genuinely boolean condition.
        if (expressionType.equalsToText("boolean") || expressionType.equalsToText("java.lang.Boolean")) {
            return null
        }
        // Only a statically-known non-boolean PRIMITIVE (int, long, char, …) is an
        // unambiguous error. Object/unknown static types (Object, a type variable,
        // an unresolved call) can still be boolean at runtime, so defer them to the
        // runtime evaluator rather than risk rejecting a valid condition.
        if (expressionType !is PsiPrimitiveType) return null
        if (expressionType.equalsToText("void") || expressionType.equalsToText("null")) return null
        return Problem(
            "not_boolean",
            "condition must evaluate to boolean, got ${expressionType.presentableText}",
            null,
        )
    }
}

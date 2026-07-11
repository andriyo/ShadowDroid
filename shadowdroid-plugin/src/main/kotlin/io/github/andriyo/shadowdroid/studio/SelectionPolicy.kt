package io.github.andriyo.shadowdroid.studio

internal data class ProjectSelectorValue(
    val name: String,
    val basePath: String?,
)

/** Pure selector rules shared by the bridge and unit tests. */
internal object SelectionPolicy {
    fun sessionIndex(selector: String, sessionIds: List<String>): Int {
        val value = selector.trim()
        if (value.isEmpty()) throw IllegalArgumentException("debugger session selector is empty")

        value.toIntOrNull()?.let { index ->
            if (index in sessionIds.indices) return index
            throw IllegalArgumentException("debugger session not found: $selector")
        }

        val index = sessionIds.indexOf(value)
        if (index >= 0) return index
        throw IllegalArgumentException("debugger session not found: $selector")
    }

    fun requireExplicitSessionTarget(
        sessionCount: Int,
        sessionSelector: String?,
        deviceSelector: String?,
    ) {
        if (sessionCount > 1 && sessionSelector == null && deviceSelector == null) {
            throw IllegalArgumentException(
                "multiple debugger sessions are active; specify session id/index or device",
            )
        }
    }

    fun projectIndex(selector: String, projects: List<ProjectSelectorValue>): Int {
        val value = selector.trim()
        if (value.isEmpty()) throw IllegalArgumentException("project selector is empty")

        val basePathMatches = projects.indices.filter { projects[it].basePath == value }
        if (basePathMatches.size == 1) return basePathMatches.single()
        if (basePathMatches.size > 1) {
            throw IllegalArgumentException("project selector is ambiguous: $selector")
        }

        val nameMatches = projects.indices.filter { projects[it].name == value }
        if (nameMatches.size == 1) return nameMatches.single()
        if (nameMatches.size > 1) {
            throw IllegalArgumentException(
                "project name is ambiguous: $selector; specify its base path",
            )
        }
        throw IllegalArgumentException("project not found: $selector")
    }

    fun requireUnambiguousProjectFallback(projectCount: Int, strict: Boolean) {
        if (strict && projectCount > 1) {
            throw IllegalArgumentException(
                "multiple projects are open; specify project name/base path",
            )
        }
    }
}

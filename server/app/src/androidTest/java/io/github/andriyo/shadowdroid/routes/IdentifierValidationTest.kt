package io.github.andriyo.shadowdroid.routes

import io.github.andriyo.shadowdroid.BadRequest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class IdentifierValidationTest {
    @Test
    fun acceptsRealPackagesAndNormalizesActivities() {
        assertEquals("com.example.app", requireAndroidPackage("com.example.app"))
        assertEquals("android", requireAndroidPackage("android"))
        assertEquals(
            "com.example.app.MainActivity",
            normalizeAndroidActivity("com.example.app", ".MainActivity"),
        )
        assertEquals(
            "com.example.app.MainActivity",
            normalizeAndroidActivity("com.example.app", "MainActivity"),
        )
        assertEquals(
            "org.example.ui.MainActivity\$Nested",
            normalizeAndroidActivity("com.example.app", "org.example.ui.MainActivity\$Nested"),
        )
    }

    @Test
    fun rejectsShellSyntaxInPackagesAndActivities() {
        val injected =
            listOf(
                "com.example;id",
                "com.example\nother",
                "com.\$(id)",
                "com.'example'",
                "com.\"example\"",
            )
        injected.forEach { value ->
            assertThrows(BadRequest::class.java) { requireAndroidPackage(value) }
            assertThrows(BadRequest::class.java) {
                normalizeAndroidActivity("com.example.app", value)
            }
        }
    }

    @Test
    fun rejectsMismatchedActivityComponentPackage() {
        assertThrows(BadRequest::class.java) {
            normalizeAndroidActivity("com.example.app", "com.other/.MainActivity")
        }
    }

    @Test
    fun shellQuotingContainsQuotesAndMetacharactersInOneArgument() {
        assertEquals("'com.example.app'", quoteDeviceShellArg("com.example.app"))
        assertEquals(
            "'a'\"'\"'b;\$(id)\nnext'",
            quoteDeviceShellArg("a'b;\$(id)\nnext"),
        )
    }

    @Test
    fun appMutationReadbacksDistinguishSuccessFromFailure() {
        assertEquals(true, packagePathExists("package:/data/app/base.apk\n"))
        assertEquals(false, packagePathExists("Error: package not found\n"))
        assertEquals(true, pmClearSucceeded("Success\n"))
        assertEquals(false, pmClearSucceeded("Failed\n"))
        assertEquals(false, pmClearSucceeded(""))
    }
}

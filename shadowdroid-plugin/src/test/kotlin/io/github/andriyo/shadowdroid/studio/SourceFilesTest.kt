package io.github.andriyo.shadowdroid.studio

import org.junit.Test
import org.junit.Assert.assertEquals

class SourceFilesTest {
    private val paths = listOf(
        "/p/app/src/main/kotlin/io/x/sample/MainActivity.kt",
        "/p/lib/src/main/kotlin/io/x/lib/MainActivity.kt",
        "/p/app/src/main/kotlin/io/x/sample/OtherMainActivity.kt",
    )

    @Test
    fun aFileNameMatchesEveryFileWithThatName() {
        assertEquals(
            listOf(paths[0], paths[1]).sorted(),
            SourceFiles.matchingPaths("MainActivity.kt", paths),
        )
    }

    @Test
    fun morePathSegmentsNarrowTheMatch() {
        assertEquals(listOf(paths[0]), SourceFiles.matchingPaths("sample/MainActivity.kt", paths))
        assertEquals(listOf(paths[0]), SourceFiles.matchingPaths("./x/sample/MainActivity.kt", paths))
        assertEquals(listOf(paths[1]), SourceFiles.matchingPaths("lib\\MainActivity.kt", paths))
    }

    @Test
    fun matchesStopAtSegmentBoundaries() {
        assertEquals(emptyList<String>(), SourceFiles.matchingPaths("ample/MainActivity.kt", paths))
    }
}

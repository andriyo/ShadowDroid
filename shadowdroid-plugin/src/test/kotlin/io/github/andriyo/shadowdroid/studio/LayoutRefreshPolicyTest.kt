package io.github.andriyo.shadowdroid.studio

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class LayoutRefreshPolicyTest {
    @Test
    fun targetlessRequestKeepsExistingModelBehavior() {
        assertTrue(
            ready(
                targetRequested = false,
                targetMatches = false,
                liveFetchRequested = false,
                modelModified = false,
                generationChanged = false,
            ),
        )
    }

    @Test
    fun targetedRequestRejectsAvailableButStaleModel() {
        assertFalse(
            ready(
                targetMatches = true,
                liveFetchRequested = true,
                modelModified = false,
                generationChanged = false,
            ),
        )
        assertFalse(
            ready(
                targetMatches = true,
                liveFetchRequested = true,
                modelModified = true,
                generationChanged = false,
            ),
        )
    }

    @Test
    fun postFetchGenerationChangeMakesMatchingModelReady() {
        assertTrue(
            ready(
                targetMatches = true,
                liveFetchRequested = true,
                modelModified = true,
                generationChanged = true,
            ),
        )
    }

    @Test
    fun clientReplacementDuringFetchIsRejected() {
        assertFalse(
            ready(
                targetMatches = true,
                fetchClientMatches = false,
                liveFetchRequested = true,
                modelModified = true,
                generationChanged = true,
            ),
        )
    }

    @Test
    fun wrongClientNeverBecomesReady() {
        assertFalse(
            ready(
                targetMatches = false,
                liveFetchRequested = true,
                modelModified = true,
                generationChanged = true,
            ),
        )
    }

    private fun ready(
        targetRequested: Boolean = true,
        stateAvailable: Boolean = true,
        targetMatches: Boolean,
        fetchClientMatches: Boolean = true,
        liveFetchRequested: Boolean,
        modelModified: Boolean,
        generationChanged: Boolean,
    ): Boolean =
        LayoutRefreshPolicy.isReady(
            targetRequested = targetRequested,
            stateAvailable = stateAvailable,
            targetMatches = targetMatches,
            fetchClientMatches = fetchClientMatches,
            liveFetchRequested = liveFetchRequested,
            modelModified = modelModified,
            generationChanged = generationChanged,
        )
}

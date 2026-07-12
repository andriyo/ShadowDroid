package io.github.andriyo.shadowdroid.routes

import android.system.Os
import android.system.OsConstants
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.ktor.utils.io.ByteChannel
import io.ktor.utils.io.ByteReadChannel
import io.ktor.utils.io.writeFully
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.io.IOException

@RunWith(AndroidJUnit4::class)
class FileRoutesAtomicWriteTest {
    @Test
    fun streamedWriteReplacesDestinationAndReportsBytes() =
        runBlocking {
            val dir = testDirectory("success")
            val destination = File(dir, "payload.bin").apply { writeText("old") }
            val payload = ByteArray(2 * 1024 * 1024) { index -> (index % 251).toByte() }

            val bytes = writeFileAtomically(ByteReadChannel(payload), destination, 0x180)

            assertEquals(payload.size.toLong(), bytes)
            assertArrayEquals(payload, destination.readBytes())
            assertEquals(0x180, Os.stat(destination.path).st_mode and 0x1FF)
            assertFalse(dir.listFiles().orEmpty().any { it.name.contains(".shadowdroid-") })
            dir.deleteRecursively()
        }

    @Test
    fun interruptedWriteKeepsPreviousDestination() =
        runBlocking {
            val dir = testDirectory("interrupted")
            val destination = File(dir, "payload.bin").apply { writeText("old") }
            val source = ByteChannel(autoFlush = true)
            val producer =
                launch {
                    source.writeFully(ByteArray(4096) { 7 })
                    source.cancel(IOException("injected transfer failure"))
                }

            val result = runCatching { writeFileAtomically(source, destination, null) }
            producer.join()

            assertTrue(result.isFailure)
            assertEquals("old", destination.readText())
            assertFalse(dir.listFiles().orEmpty().any { it.name.contains(".shadowdroid-") })
            dir.deleteRecursively()
        }

    @Test
    fun longDestinationNameStillUsesBoundedTemporaryComponent() =
        runBlocking {
            val dir = testDirectory("long-name")
            val destination = File(dir, "x".repeat(240))

            writeFileAtomically(ByteReadChannel("ok".toByteArray()), destination, null)

            assertEquals("ok", destination.readText())
            assertFalse(dir.listFiles().orEmpty().any { it.name.contains(".shadowdroid-") })
            dir.deleteRecursively()
        }

    @Test
    fun nonRegularDestinationsAreRejectedWithoutChangingTheirTargets() =
        runBlocking {
            val dir = testDirectory("non-regular")
            val target = File(dir, "target.bin").apply { writeText("keep") }
            val link = File(dir, "link.bin")
            Os.symlink(target.path, link.path)

            val linkResult =
                runCatching {
                    writeFileAtomically(ByteReadChannel("replace".toByteArray()), link, null)
                }
            assertTrue(
                linkResult
                    .exceptionOrNull()
                    ?.message
                    .orEmpty()
                    .contains("non-regular"),
            )
            assertEquals("keep", target.readText())
            assertEquals(OsConstants.S_IFLNK, Os.lstat(link.path).st_mode and OsConstants.S_IFMT)

            val fifo = File(dir, "pipe")
            Os.mkfifo(fifo.path, 0x180)
            val fifoResult =
                runCatching {
                    writeFileAtomically(ByteReadChannel("replace".toByteArray()), fifo, null)
                }
            assertTrue(
                fifoResult
                    .exceptionOrNull()
                    ?.message
                    .orEmpty()
                    .contains("non-regular"),
            )
            assertEquals(OsConstants.S_IFIFO, Os.lstat(fifo.path).st_mode and OsConstants.S_IFMT)
            dir.deleteRecursively()
        }

    @Test
    fun siblingWithRootNamePrefixIsNotTreatedAsAChild() {
        // cacheDir is backed by the emulator's native app filesystem and
        // supports symlinks; emulated external/FUSE storage often does not.
        val cache = InstrumentationRegistry.getInstrumentation().targetContext.cacheDir
        val root = File(cache, "containment-root-${System.nanoTime()}").apply { check(mkdirs()) }
        val sibling =
            File(root.parentFile, "${root.name}-evil-${System.nanoTime()}").apply {
                check(mkdirs())
            }
        val link = File(root, "escape-${System.nanoTime()}")
        try {
            Os.symlink(sibling.path, link.path)
            val result =
                runCatching {
                    resolveUnderRoot(root, "${link.name}/payload.bin")
                }
            assertTrue(result.isFailure)
            assertTrue(
                result
                    .exceptionOrNull()
                    ?.message
                    .orEmpty()
                    .contains("escapes"),
            )
        } finally {
            link.delete()
            root.deleteRecursively()
            sibling.deleteRecursively()
        }
    }

    private fun testDirectory(suffix: String): File {
        val cache = InstrumentationRegistry.getInstrumentation().targetContext.cacheDir
        return File(cache, "shadowdroid-file-route-$suffix-${System.nanoTime()}").apply {
            check(mkdirs())
        }
    }
}
